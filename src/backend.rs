use std::{fmt::Display, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Error, anyhow};
use arc_swap::ArcSwapOption;
use async_trait::async_trait;
use axum::{body::Body, extract::Request, response::Response};
use derive_new::new;
use http::{Uri, Version, uri::PathAndQuery};
use ic_bn_lib::{
    http::{ClientHttp, proxy::proxy_http},
    tasks::Run,
    utils::{
        backend_router::BackendRouter,
        distributor::{self, ExecutesRequest},
        health_check::{self, ChecksTarget, TargetState},
    },
};
use prometheus::Registry;
use serde::{Deserialize, Serialize};
use tokio::{
    fs, select,
    signal::unix::{SignalKind, signal},
    sync::{Mutex, MutexGuard},
    time::timeout,
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
    pub name: String,
    url: Url,
    uri_health: Uri,
}

impl Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name)
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
        }
    }
}

pub fn setup_backend_router(
    client: Arc<dyn ClientHttp<Body>>,
    backends_conf: Vec<BackendConf>,
    check_interval: Duration,
    check_timeout: Duration,
    metrics_health_checker: health_check::Metrics,
    metrics_distributor: distributor::Metrics,
) -> LBBackendRouter {
    let mut backends = vec![];
    for b in backends_conf {
        let weight = b.weight;
        backends.push((Arc::new(b.into()), weight));
    }

    BackendRouter::new(
        &backends,
        Arc::new(RequestExecutor::new(client.clone())),
        Arc::new(BackendHealthChecker::new(client, check_timeout)),
        ic_bn_lib::utils::distributor::Strategy::WeightedRoundRobin,
        check_interval,
        metrics_health_checker,
        metrics_distributor,
    )
}

/// Manages backends configuration
#[derive(Debug)]
pub struct BackendManager {
    client: Arc<dyn ClientHttp<Body>>,
    config_path: PathBuf,
    backend_router: Arc<ArcSwapOption<LBBackendRouter>>,
    check_interval: Duration,
    check_timeout: Duration,
    backends: Mutex<Vec<BackendConf>>,
    metrics_distributor: distributor::Metrics,
    metrics_health_checker: health_check::Metrics,
}

impl BackendManager {
    /// Create a new BackendManager
    pub fn new(
        client: Arc<dyn ClientHttp<Body>>,
        config_path: PathBuf,
        backend_router: Arc<ArcSwapOption<LBBackendRouter>>,
        check_interval: Duration,
        check_timeout: Duration,
        registry: &Registry,
    ) -> Self {
        Self {
            client,
            config_path,
            backend_router,
            check_interval,
            check_timeout,
            backends: Mutex::new(vec![]),
            metrics_distributor: distributor::Metrics::new(registry),
            metrics_health_checker: health_check::Metrics::new(registry),
        }
    }

    /// Waits until all nodes transition from Unknown health state
    async fn wait_healthchecks(backend_router: &LBBackendRouter) {
        let mut rx = backend_router.subscribe();
        while rx.changed().await.is_ok() {
            let h = rx.borrow_and_update().clone();
            let done = h.iter().all(|x| x.1 != TargetState::Unknown);

            if done {
                let states = h.iter().map(|x| x.1).collect::<Vec<_>>();
                warn!("All backends have known health state: {states:?}");
                return;
            }
        }
    }

    /// Applies the config
    pub async fn apply(&self, backends: MutexGuard<'_, Vec<BackendConf>>) {
        // Filter out disabled backends
        let backends_enabled = backends
            .clone()
            .into_iter()
            .filter(|x| x.enabled)
            .collect::<Vec<_>>();

        // Prepare a new BackendRouter
        let backend_router = setup_backend_router(
            self.client.clone(),
            backends_enabled.clone(),
            self.check_interval,
            self.check_timeout,
            self.metrics_health_checker.clone(),
            self.metrics_distributor.clone(),
        );

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
        let cfg = fs::read(&self.config_path)
            .await
            .context("unable to read backends config")?;

        let backends_new: Vec<BackendConf> =
            serde_yaml_ng::from_slice(&cfg).context("unable to parse backends config")?;

        let mut backends = self.backends.lock().await;
        *backends = backends_new;

        self.apply(backends).await;

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
        // Keep the backends locked to ensure that we don't update them concurrently
        let mut backends = self.backends.lock().await;

        // Find the backend & set the status
        let backend = backends
            .iter_mut()
            .find(|x| x.name == name)
            .ok_or_else(|| anyhow!("Backend not found: {name}"))?;
        backend.enabled = enabled;

        // Apply the new config
        self.apply(backends).await;

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
    timeout: Duration,
}

#[async_trait]
impl ChecksTarget<Arc<Backend>> for BackendHealthChecker {
    async fn check(&self, target: &Arc<Backend>) -> TargetState {
        let req = http::Request::builder()
            .uri(target.uri_health.clone())
            .body(Body::empty())
            .unwrap();

        let Ok(Ok(res)) = timeout(self.timeout, self.client.execute(req)).await else {
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
            .context("invalid URL")?;

        *req.uri_mut() = uri;

        // Override version to HTTP/1.1, otherwise Hyper would fail to send HTTP/2 requests over HTTP/1.1 backend links.
        // Sending HTTP/1.1 requests over HTTP/2 connections is fine.
        *req.version_mut() = Version::HTTP_11;

        proxy_http(req, &self.client).await.map(|mut x| {
            // Insert the backend into response for observability
            x.extensions_mut().insert(backend.clone());
            x
        })
    }
}
