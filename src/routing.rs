use std::{fmt::Display, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Error};
use arc_swap::ArcSwapOption;
use async_trait::async_trait;
use axum::{
    Extension, Router,
    body::Body,
    extract::{Request, State},
    response::{IntoResponse, Response},
};
use axum_extra::extract::Host;
use bytes::Bytes;
use derive_new::new;
use http::{HeaderValue, Method, StatusCode};
use ic_bn_lib::{
    http::{
        Client, ConnInfo,
        headers::{X_FORWARDED_HOST, X_REAL_IP},
        proxy::proxy,
    },
    reqwest,
    utils::{
        backend_router::BackendRouter,
        distributor::ExecutesRequest,
        health_check::{ChecksTarget, TargetState},
    },
};
use serde::Deserialize;
use tokio::{fs, sync::Mutex};
use url::Url;

type LBBackendRouter = BackendRouter<Arc<Backend>, Request, Response, ic_bn_lib::http::Error>;

/// Backend as represented in the config file
#[derive(Deserialize, Clone, Debug, Eq, PartialEq)]
pub struct BackendConf {
    pub name: String,
    pub url: Url,
    pub enabled: bool,
    pub weight: usize,
}

/// Backend after preprocessing
#[derive(Clone, Debug)]
pub struct Backend {
    name: String,
    url: Url,
    url_health: Url,
    weight: usize,
}

impl Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({}, w{})", self.name, self.url, self.weight)
    }
}

impl From<BackendConf> for Backend {
    fn from(v: BackendConf) -> Self {
        let mut url_health = v.url.clone();
        url_health.set_path("/health");

        Self {
            name: v.name,
            url: v.url,
            url_health,
            weight: v.weight,
        }
    }
}

/// Manages backend configuration
#[derive(Debug, new)]
pub struct BackendManager {
    client: Arc<dyn Client>,
    config_path: PathBuf,
    backend_router: Arc<ArcSwapOption<LBBackendRouter>>,
    #[new(default)]
    backends: Mutex<Vec<BackendConf>>,
}

impl BackendManager {
    /// Waits until all nodes transition from Unknown health state
    async fn wait_healthchecks(backend_router: &LBBackendRouter) {
        let mut rx = backend_router.subscribe();
        while rx.changed().await.is_ok() {
            let h = rx.borrow_and_update().clone();
            let done = h.iter().all(|x| x.1 != TargetState::Unknown);
            if done {
                println!("all backends have known health state");
                return;
            }
        }
    }

    /// Applies the config from locally stored backends
    pub async fn apply(&self) {
        // Keep the backends locked to ensure that we don't reload concurrently
        let backends = self.backends.lock().await;
        // Prepare a new BackendRouter
        let backend_router = setup_backend_router(self.client.clone(), backends.clone());
        // Wait until all backends in it have known health status
        Self::wait_healthchecks(&backend_router).await;
        // Get the old BackendRouter if any
        let old_backend_router = self.backend_router.load_full();
        // Install the new one
        self.backend_router.store(Some(Arc::new(backend_router)));
        // Shut down the old one if it exists
        if let Some(v) = old_backend_router {
            v.stop().await;
            println!("old backend router stopped");
        }
        println!("new backends applied");
    }

    /// Loads the backends from the config file
    pub async fn load_config(&self) -> Result<(), Error> {
        // Load
        let backends = fs::read(&self.config_path)
            .await
            .context("unable to read backends config")?;

        let backends: Vec<BackendConf> =
            serde_yaml_ng::from_slice(&backends).context("unable to parse backends config")?;

        *self.backends.lock().await = backends;
        self.apply().await;

        println!("config file loaded");
        Ok(())
    }
}

/// Healthchecks the backends using its HTTP client
#[derive(Debug, new)]
pub struct BackendHealthChecker {
    client: Arc<dyn Client>,
}

#[async_trait]
impl ChecksTarget<Arc<Backend>> for BackendHealthChecker {
    async fn check(&self, target: &Arc<Backend>) -> TargetState {
        let req = reqwest::Request::new(Method::GET, target.url_health.clone());

        let Ok(res) = self.client.execute(req).await else {
            return TargetState::Degraded;
        };

        if !res.status().is_success() {
            return TargetState::Degraded;
        }

        TargetState::Healthy
    }
}

/// Executes the requests using its HTTP client
#[derive(Debug, new)]
pub struct RequestExecutor {
    client: Arc<dyn Client>,
}

#[async_trait]
impl ExecutesRequest<Arc<Backend>> for RequestExecutor {
    type Request = http::Request<Body>;
    type Response = http::Response<Body>;
    type Error = ic_bn_lib::http::Error;

    async fn execute(
        &self,
        backend: &Arc<Backend>,
        req: Self::Request,
    ) -> Result<Self::Response, Self::Error> {
        let mut url = backend.url.clone();
        url.set_path(req.uri().path());
        url.set_query(req.uri().query());

        proxy(url, req, &self.client).await
    }
}

#[derive(Debug, new)]
pub struct HandlerState {
    backend_router: Arc<ArcSwapOption<LBBackendRouter>>,
}

pub async fn handler(
    State(state): State<Arc<HandlerState>>,
    Extension(conn_info): Extension<Arc<ConnInfo>>,
    Host(host): Host,
    mut request: Request,
) -> Response {
    let Some(backend_router) = state.backend_router.load_full() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "Service is not yet ready").into_response();
    };

    request.headers_mut().insert(
        X_FORWARDED_HOST,
        HeaderValue::from_maybe_shared(Bytes::from(host)).unwrap(),
    );

    request.headers_mut().insert(
        X_REAL_IP,
        HeaderValue::from_maybe_shared(Bytes::from(conn_info.remote_addr.ip().to_string()))
            .unwrap(),
    );

    let resp = backend_router.execute(request).await;
    match resp {
        Err(e) => (StatusCode::SERVICE_UNAVAILABLE, format!("Error: {e:#}")).into_response(),
        Ok(v) => v,
    }
}

pub fn setup_backend_router(
    client: Arc<dyn Client>,
    backends_conf: Vec<BackendConf>,
) -> LBBackendRouter {
    let mut backends = vec![];
    for b in backends_conf.into_iter().filter(|x| x.enabled) {
        let weight = b.weight;
        backends.push((Arc::new(b.into()), weight));
    }

    BackendRouter::new(
        &backends,
        Arc::new(RequestExecutor::new(client.clone())),
        Arc::new(BackendHealthChecker::new(client)),
        ic_bn_lib::utils::distributor::Strategy::WeightedRoundRobin,
        Duration::from_secs(1),
    )
}

pub fn setup_axum_router(
    backend_router: Arc<ArcSwapOption<LBBackendRouter>>,
) -> Result<Router, Error> {
    let state = Arc::new(HandlerState::new(backend_router));

    Ok(Router::new().fallback(handler).with_state(state))
}
