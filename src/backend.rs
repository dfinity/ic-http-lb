use std::{cell::RefCell, fmt::Display, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Error, anyhow, bail};
use arc_swap::ArcSwapOption;
use async_trait::async_trait;
use axum::{body::Body, extract::Request, response::Response};
use derive_new::new;
use http::{Uri, Version, uri::PathAndQuery};
use ic_bn_lib::{
    http::{ClientHttp, headers::strip_connection_headers},
    tasks::Run,
    utils::{
        backend_router::BackendRouter,
        distributor::{self, ExecutesRequest, Strategy},
        health_check::{self, ChecksTarget, TargetState},
    },
};
use itertools::Itertools;
use prometheus::Registry;
use serde::{Deserialize, Serialize};
use tokio::{
    fs, select,
    signal::unix::{SignalKind, signal},
    sync::{Mutex, MutexGuard},
    task_local,
    time::timeout,
};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use url::Url;

pub type LBBackendRouter = BackendRouter<Arc<Backend>, Request, Response, ic_bn_lib::http::Error>;

task_local! {
    pub static REQUEST_CONTEXT: RefCell<RequestContext>;
}

/// Request context information
#[derive(Debug, Clone, Default)]
pub struct RequestContext {
    pub backend: Option<Arc<Backend>>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Eq, PartialEq)]
pub struct Config {
    strategy: Strategy,
    backends: Vec<BackendConf>,
    fallback: Option<Vec<BackendConf>>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            strategy: Strategy::LeastOutstandingRequests,
            backends: vec![],
            fallback: None,
        }
    }
}

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
    backends: Vec<BackendConf>,
    strategy: Strategy,
    check_interval: Duration,
    check_timeout: Duration,
    metrics_health_checker: health_check::Metrics,
    metrics_distributor: distributor::Metrics,
) -> LBBackendRouter {
    let backends = backends
        .into_iter()
        .map(|x| {
            let w = x.weight;
            (Arc::new(x.into()), w)
        })
        .collect::<Vec<_>>();

    BackendRouter::new(
        &backends,
        Arc::new(RequestExecutor::new(client.clone())),
        Arc::new(BackendHealthChecker::new(client, check_timeout)),
        strategy,
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
    backend_router_fallback: Arc<ArcSwapOption<LBBackendRouter>>,
    check_interval: Duration,
    check_timeout: Duration,
    config: Mutex<Config>,
    metrics_distributor: distributor::Metrics,
    metrics_health_checker: health_check::Metrics,
}

impl BackendManager {
    /// Create a new BackendManager
    pub fn new(
        client: Arc<dyn ClientHttp<Body>>,
        config_path: PathBuf,
        check_interval: Duration,
        check_timeout: Duration,
        registry: &Registry,
    ) -> Self {
        Self {
            client,
            config_path,
            backend_router: Arc::new(ArcSwapOption::empty()),
            backend_router_fallback: Arc::new(ArcSwapOption::empty()),
            check_interval,
            check_timeout,
            config: Mutex::new(Config::default()),
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

    fn create_backend_router(
        &self,
        backends: Vec<BackendConf>,
        strategy: Strategy,
    ) -> LBBackendRouter {
        setup_backend_router(
            self.client.clone(),
            backends.clone(),
            strategy,
            self.check_interval,
            self.check_timeout,
            self.metrics_health_checker.clone(),
            self.metrics_distributor.clone(),
        )
    }

    // Updates given router with new set of backends
    async fn update_router(
        &self,
        router: &Arc<ArcSwapOption<LBBackendRouter>>,
        backends: Vec<BackendConf>,
        strategy: Strategy,
    ) {
        // Prepare new BackendRouter
        let router_new = if !backends.is_empty() {
            let r = self.create_backend_router(backends.clone(), strategy);
            // Wait until all backends have known health status
            Self::wait_healthchecks(&r).await;
            Some(Arc::new(r))
        } else {
            None
        };

        // Get the old BackendRouter
        let router_old = router.load_full();

        // Install the new one
        router.store(router_new);

        // Shut down the old one if any
        if let Some(v) = router_old {
            v.stop().await;
            warn!("Old backend router stopped");
        }
    }

    /// Applies the config
    pub async fn apply(&self, config: MutexGuard<'_, Config>) {
        // Filter out disabled backends
        let backends_enabled = config
            .backends
            .clone()
            .into_iter()
            .filter(|x| x.enabled)
            .collect::<Vec<_>>();

        let backends_fallback_enabled = config
            .fallback
            .clone()
            .unwrap_or_default()
            .into_iter()
            .filter(|x| x.enabled)
            .collect::<Vec<_>>();

        self.update_router(&self.backend_router, backends_enabled, config.strategy)
            .await;

        self.update_router(
            &self.backend_router_fallback,
            backends_fallback_enabled,
            config.strategy,
        )
        .await;

        warn!("New backends applied");
    }

    /// Loads the backends from the config file
    pub async fn load_config(&self) -> Result<(), Error> {
        // Load
        let cfg = fs::read(&self.config_path)
            .await
            .context("unable to read backends config")?;

        let config: Config =
            serde_yaml_ng::from_slice(&cfg).context("unable to parse backends config")?;

        self.set_config(config).await?;

        warn!("Config file loaded");
        Ok(())
    }

    /// Replaces the current config with the provided one
    pub async fn set_config(&self, config_new: Config) -> Result<(), Error> {
        // Ensure all backend names are unique
        if !config_new
            .backends
            .iter()
            .chain(config_new.fallback.iter().flatten())
            .map(|x| x.name.clone())
            .all_unique()
        {
            bail!("Non-unique backend names");
        }

        let mut config = self.config.lock().await;
        *config = config_new;
        self.apply(config).await;

        Ok(())
    }

    /// Returns the current config
    pub async fn get_config(&self) -> Config {
        self.config.lock().await.clone()
    }

    /// Persists the current config to disk
    pub async fn persist_config(&self) -> Result<(), Error> {
        let cfg = self.config.lock().await.clone();
        let yaml = serde_yaml_ng::to_string(&cfg).context("unable to serialize config to YAML")?;

        fs::write(&self.config_path, &yaml)
            .await
            .context("unable to save config to disk")
    }

    /// Get the main or the fallback router
    pub fn get_backend_router(&self) -> Option<Arc<LBBackendRouter>> {
        self.backend_router
            .load_full()
            // Return main router only if it has healthy nodes
            .filter(|x| !x.get_healthy().is_empty())
            // Otherwise fallback, if there's one
            .or_else(|| self.backend_router_fallback.load_full())
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
        let mut config = self.config.lock().await;

        // Find the backend & set the status
        let b = config
            .backends
            .iter_mut()
            .find(|x| x.name == name)
            .ok_or_else(|| anyhow!("Backend not found: {name}"))?;
        b.enabled = enabled;

        // Apply the new config
        self.apply(config).await;

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
        let req = Request::builder()
            .uri(target.uri_health.clone())
            .body(Body::empty())
            .unwrap();

        let res = match timeout(self.timeout, self.client.execute(req)).await {
            Ok(res) => match res {
                Ok(v) => v,
                Err(e) => {
                    info!("Health check failed for {target}: request failed: {e:#}");
                    return TargetState::Degraded;
                }
            },

            Err(_) => {
                info!("Health check failed for {target}: request timed out");
                return TargetState::Degraded;
            }
        };

        if !res.status().is_success() {
            info!(
                "Health check failed for {target}: bad status code: {}",
                res.status()
            );
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
    type Request = Request<Body>;
    type Response = Response<Body>;
    type Error = ic_bn_lib::http::Error;

    async fn execute(
        &self,
        backend: &Arc<Backend>,
        mut req: Self::Request,
    ) -> Result<Self::Response, Self::Error> {
        // Store the selected backend in the request context
        let _ = REQUEST_CONTEXT.try_with(|x| {
            x.borrow_mut().backend = Some(backend.clone());
        });

        let uri = match Uri::builder()
            .scheme(backend.url.scheme())
            .authority(backend.url.authority())
            .path_and_query(
                req.uri()
                    .path_and_query()
                    .unwrap_or(&self.pq_default)
                    .as_str(),
            )
            .build()
        {
            Ok(v) => v,
            Err(e) => {
                warn!(
                    "Invalid URL '{}://{}/{:?}': {e:#}",
                    backend.url.scheme(),
                    backend.url.authority(),
                    req.uri().path_and_query(),
                );
                return Err(e.into());
            }
        };

        *req.uri_mut() = uri;

        // Override version to HTTP/1.1, otherwise Hyper would fail to send HTTP/2 requests over HTTP/1.1 backend links.
        // Sending HTTP/1.1 requests over HTTP/2 connections is fine.
        *req.version_mut() = Version::HTTP_11;

        // Sanitize the request
        strip_connection_headers(req.headers_mut());

        // Execute it
        self.client.execute(req).await
    }
}
