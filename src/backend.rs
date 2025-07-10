use std::{fmt::Display, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Error, anyhow};
use arc_swap::ArcSwapOption;
use async_trait::async_trait;
use axum::{body::Body, extract::Request, response::Response};
use derive_new::new;
use http::{Uri, uri::PathAndQuery};
use ic_bn_lib::{
    http::{ClientHttp, proxy::proxy_http},
    tasks::Run,
    utils::{
        backend_router::BackendRouter,
        distributor::ExecutesRequest,
        health_check::{ChecksTarget, TargetState},
    },
};
use serde::{Deserialize, Serialize};
use tokio::{
    fs, select,
    signal::unix::{SignalKind, signal},
    sync::Mutex,
};
use tokio_util::sync::CancellationToken;
use tracing::warn;
use url::Url;

pub type LBBackendRouter = BackendRouter<Arc<Backend>, Request, Response, ic_bn_lib::http::Error>;

/// Backend as represented in the config file
#[derive(Serialize, Deserialize, Clone, Debug, Eq, PartialEq)]
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
    uri_health: Uri,
    weight: usize,
}

impl Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({}, w{})", self.name, self.url, self.weight)
    }
}

impl From<BackendConf> for Backend {
    fn from(v: BackendConf) -> Self {
        let uri_health = Uri::builder()
            .scheme(v.url.scheme())
            .authority(v.url.authority())
            .path_and_query("/health")
            .build()
            .expect("able to build health URI");

        Self {
            name: v.name,
            url: v.url,
            uri_health,
            weight: v.weight,
        }
    }
}

pub fn setup_backend_router(
    client: Arc<dyn ClientHttp<Body>>,
    backends_conf: Vec<BackendConf>,
) -> LBBackendRouter {
    let mut backends = vec![];
    for b in backends_conf {
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

/// Manages backends configuration
#[derive(Debug, new)]
pub struct BackendManager {
    client: Arc<dyn ClientHttp<Body>>,
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
            let states = h.iter().map(|x| x.1).collect::<Vec<_>>();

            if done {
                warn!("All backends have known health state: {states:?}");
                return;
            }
        }
    }

    /// Applies the config from locally stored backends
    pub async fn apply(&self) {
        // Keep the backends locked to ensure that we don't reload concurrently
        let backends = self.backends.lock().await;

        // Filter out disabled backends
        let backends_enabled = backends
            .clone()
            .into_iter()
            .filter(|x| x.enabled)
            .collect::<Vec<_>>();

        // Prepare a new BackendRouter
        let backend_router = setup_backend_router(self.client.clone(), backends_enabled.clone());

        // If there are any enabled backends -  wait until all of them have known health status
        if !backends_enabled.is_empty() {
            Self::wait_healthchecks(&backend_router).await;
        }

        // Get the old BackendRouter if any
        let old_backend_router = self.backend_router.load_full();

        // Install the new one
        self.backend_router.store(Some(Arc::new(backend_router)));

        // Shut down the old one if it exists
        if let Some(v) = old_backend_router {
            v.stop().await;
            warn!("Old backend router stopped");
        }

        warn!("New backends applied");
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

        warn!("Config file loaded");
        Ok(())
    }

    /// Returns the current config
    pub async fn get_config(&self) -> Vec<BackendConf> {
        self.backends.lock().await.clone()
    }

    /// Get a list of healthy nodes
    pub fn get_healthy_nodes(&self) -> Arc<Vec<Arc<Backend>>> {
        self.backend_router
            .load_full()
            .map(|x| x.get_healthy())
            .unwrap_or_else(|| Arc::new(vec![]))
    }

    /// Enables or disables the given backend
    pub async fn set_backend_state(&self, name: String, enabled: bool) -> Result<(), Error> {
        // Keep the backends locked to ensure that we don't reload concurrently
        let mut backends = self.backends.lock().await;

        let backend = backends
            .iter_mut()
            .find(|x| x.name == name)
            .ok_or_else(|| anyhow!("Backend not found: {name}"))?;
        backend.enabled = enabled;
        drop(backends);

        self.apply().await;
        Ok(())
    }
}

#[async_trait]
impl Run for BackendManager {
    async fn run(&self, token: CancellationToken) -> Result<(), Error> {
        let mut sig = signal(SignalKind::hangup()).context("unable to listen for SIGHUP")?;

        warn!("BackendManager: Task started");
        loop {
            select! {
                _ = token.cancelled() => {
                    warn!("BackendManager: Task stopped");
                    break;
                },

                _ = sig.recv() => {
                    warn!("BackendManager: received config reload signal");
                    match self.load_config().await {
                        Ok(_) => warn!("BackendManager: configuration reloaded successfully"),
                        Err(e) => warn!("BackendManager: failed to reload config: {e:#}"),
                    }
                }
            }
        }

        Ok(())
    }
}

#[derive(Debug, new)]
pub struct BackendHealthChecker {
    client: Arc<dyn ClientHttp<Body>>,
}

#[async_trait]
impl ChecksTarget<Arc<Backend>> for BackendHealthChecker {
    async fn check(&self, target: &Arc<Backend>) -> TargetState {
        let req = http::Request::builder()
            .uri(target.uri_health.clone())
            .body(Body::empty())
            .unwrap();

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
    #[new(value = "PathAndQuery::from_static(\"/\")")]
    pq_default: PathAndQuery,
    client: Arc<dyn ClientHttp<Body>>,
}

#[async_trait]
impl ExecutesRequest<Arc<Backend>> for RequestExecutor {
    type Request = http::Request<Body>;
    type Response = http::Response<Body>;
    type Error = ic_bn_lib::http::Error;

    async fn execute(
        &self,
        backend: &Arc<Backend>,
        mut req: Self::Request,
    ) -> Result<Self::Response, Self::Error> {
        let uri = Uri::builder()
            .scheme(backend.url.scheme())
            .authority(backend.url.authority())
            .path_and_query(
                req.uri()
                    .path_and_query()
                    .unwrap_or(&self.pq_default)
                    .as_str(),
            )
            .build()
            .unwrap();

        *req.uri_mut() = uri;

        proxy_http(req, &self.client).await
    }
}
