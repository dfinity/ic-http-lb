#![allow(clippy::significant_drop_tightening)]

use std::{cell::RefCell, fmt::Display, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Error, anyhow, bail};
use arc_swap::ArcSwapOption;
use async_trait::async_trait;
use axum::{body::Body, extract::Request, response::Response};
use derive_new::new;
use http::{Uri, Version, uri::PathAndQuery};
use ic_bn_lib::{
    http::{ClientHttp, Error as HttpError, headers::strip_connection_headers},
    lb::{
        ChecksTarget, ExecutesRequest, TargetState,
        backend_router::BackendRouter,
        distributor::{self, Strategy},
        health_check::{self},
    },
    tasks::Run,
};
use itertools::Itertools;
use prometheus::Registry;
use serde::{Deserialize, Serialize};
use tokio::{
    fs, select,
    signal::unix::{SignalKind, signal},
    sync::{Mutex, OwnedMutexGuard},
    task_local,
    time::timeout,
};
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use tracing::{info, warn};
use url::Url;

pub type LBBackendRouter = BackendRouter<Arc<Backend>, Request, Response, HttpError>;

task_local! {
    pub static REQUEST_CONTEXT: RefCell<RequestContext>;
}

/// Request context information
#[derive(Debug, Clone, Default)]
pub struct RequestContext {
    pub backend: Option<Arc<Backend>>,
    pub request_body_buffered: bool,
    pub response_body_buffered: bool,
}

/// Backend configuration
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

impl Config {
    /// Checks that the config is valid
    pub fn validate(&self) -> Result<(), Error> {
        // Ensure all backend names are unique
        if !self
            .backends
            .iter()
            .chain(self.fallback.iter().flatten())
            .map(|x| &x.name)
            .all_unique()
        {
            bail!("Non-unique backend names");
        }

        Ok(())
    }
}

/// Single backend as represented in the config file
#[derive(Serialize, Deserialize, Clone, Debug, Eq, PartialEq)]
pub struct BackendConf {
    pub name: String,
    pub url: Url,
    pub enabled: bool,
    pub weight: usize,
}

/// Single backend after preprocessing
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

/// Sets up the `LBBackendRouter` instance using the given config parameters
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
    config: Arc<Mutex<Config>>,
    /// Tracks the config updates in progress so that the shutdown waits for them
    updates: TaskTracker,
    metrics_distributor: distributor::Metrics,
    metrics_health_checker: health_check::Metrics,
}

impl Display for BackendManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BackendManager")
    }
}

impl BackendManager {
    /// Create a new `BackendManager`
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
            config: Arc::new(Mutex::new(Config::default())),
            updates: TaskTracker::new(),
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
            backends,
            strategy,
            self.check_interval,
            self.check_timeout,
            self.metrics_health_checker.clone(),
            self.metrics_distributor.clone(),
        )
    }

    /// Updates given router with new set of backends
    async fn update_router(
        &self,
        router: &Arc<ArcSwapOption<LBBackendRouter>>,
        backends: Vec<BackendConf>,
        strategy: Strategy,
    ) {
        // Prepare new BackendRouter
        let router_new = if backends.is_empty() {
            None
        } else {
            let r = self.create_backend_router(backends.clone(), strategy);
            // Wait until all backends have known health status
            Self::wait_healthchecks(&r).await;
            Some(Arc::new(r))
        };

        // Get the old BackendRouter
        let router_old = router.load_full();

        // Install the new one
        router.store(router_new);

        // Shut down the old one if any.
        // It's not reachable anymore, so do it in the background: stopping waits for
        // the in-flight health checks to finish which can take up to `check_timeout`.
        if let Some(v) = router_old {
            let name = self.to_string();
            tokio::spawn(async move {
                v.stop().await;
                warn!("{name}: old backend router stopped");
            });
        }
    }

    /// Applies the main backends from the config to the main router
    async fn apply_main(&self, config: &Config) {
        // Filter out disabled backends
        let backends_enabled = config
            .backends
            .iter()
            .filter(|x| x.enabled)
            .cloned()
            .collect::<Vec<_>>();

        self.update_router(&self.backend_router, backends_enabled, config.strategy)
            .await;
    }

    /// Applies the fallback backends from the config to the fallback router
    async fn apply_fallback(&self, config: &Config) {
        // Filter out disabled backends
        let backends_fallback_enabled = config
            .fallback
            .iter()
            .flatten()
            .filter(|x| x.enabled)
            .cloned()
            .collect::<Vec<_>>();

        self.update_router(
            &self.backend_router_fallback,
            backends_fallback_enabled,
            config.strategy,
        )
        .await;
    }

    /// Applies the config to both main & fallback routers
    async fn apply(&self, config: &Config) {
        self.apply_main(config).await;
        self.apply_fallback(config).await;

        warn!("{self}: new backends applied");
    }

    /// Runs the given config update with the config locked.
    ///
    /// Waiting for the lock is cancellable: if the caller goes away while waiting then nothing is changed.
    /// Once the lock is acquired, the update runs to completion in a separate task even if the caller
    /// goes away (e.g. the API client disconnects), so that it's never abandoned halfway.
    /// The task is tracked so that the shutdown waits for it to finish.
    ///
    /// The config lock is held until the update finishes: the shutdown in `run()` relies on this.
    async fn run_update<T, F, Fut>(self: &Arc<Self>, f: F) -> Result<T, Error>
    where
        T: Send + 'static,
        F: FnOnce(Arc<Self>, OwnedMutexGuard<Config>) -> Fut,
        Fut: Future<Output = Result<T, Error>> + Send + 'static,
    {
        let config = self.config.clone().lock_owned().await;

        // Don't start new updates once the shutdown has begun
        if self.updates.is_closed() {
            bail!("shutting down, config updates are not accepted");
        }

        let fut = f(self.clone(), config);
        let name = self.to_string();

        self.updates
            .spawn(async move {
                let res = fut.await;
                // Log the error here too since the caller might be gone already
                if let Err(e) = &res {
                    warn!("{name}: config update failed: {e:#}");
                }
                res
            })
            .await
            .context("config update task failed")?
    }

    /// Applies the given config and optionally persists it to disk.
    /// The config must be locked by the caller.
    async fn set_config_locked(
        &self,
        config: &mut Config,
        config_new: Config,
        persist: bool,
    ) -> Result<(), Error> {
        config_new.validate()?;

        *config = config_new;
        self.apply(config).await;

        if persist {
            self.persist(config)
                .await
                .context("unable to persist new config")?;
        }

        Ok(())
    }

    /// Loads the config from the file and applies it.
    /// The config must be locked by the caller.
    async fn load_config_locked(&self, config: &mut Config) -> Result<(), Error> {
        let cfg = fs::read(&self.config_path)
            .await
            .context("unable to read backends config")?;

        let config_new: Config =
            serde_yaml_ng::from_slice(&cfg).context("unable to parse backends config")?;

        self.set_config_locked(config, config_new, false).await?;

        warn!("{self}: config file loaded");
        Ok(())
    }

    /// Writes the given config to disk
    async fn persist(&self, config: &Config) -> Result<(), Error> {
        let yaml =
            serde_yaml_ng::to_string(config).context("unable to serialize config to YAML")?;

        fs::write(&self.config_path, &yaml)
            .await
            .context("unable to save config to disk")
    }

    /// Loads the config from the file and applies it
    pub async fn load_config(self: &Arc<Self>) -> Result<(), Error> {
        self.run_update(
            |this, mut config| async move { this.load_config_locked(&mut config).await },
        )
        .await
    }

    /// Replaces the current config with the provided one and optionally persists it to disk
    pub async fn set_config(
        self: &Arc<Self>,
        config_new: Config,
        persist: bool,
    ) -> Result<(), Error> {
        self.run_update(move |this, mut config| async move {
            this.set_config_locked(&mut config, config_new, persist)
                .await
        })
        .await
    }

    /// Returns the current config
    pub async fn get_config(&self) -> Config {
        self.config.lock().await.clone()
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
            .map_or_else(|| Arc::new(vec![]), |x| x.get_healthy())
    }

    /// Enables or disables the given backend
    pub async fn set_backend_state(
        self: &Arc<Self>,
        name: String,
        enabled: bool,
    ) -> Result<(), Error> {
        self.run_update(move |this, mut config| async move {
            // Find the backend & set the status
            let b = config
                .backends
                .iter_mut()
                .find(|x| x.name == name)
                .ok_or_else(|| anyhow!("Backend not found: {name}"))?;
            b.enabled = enabled;

            // Only the main backends are affected, so there's no need to rebuild the fallback router.
            // Rebuilding it would wait for the health checks of all fallback backends
            // (usually all of the backends in other regions) which can take up to `check_timeout`.
            this.apply_main(&config).await;

            warn!("{this}: main backends applied");
            Ok(())
        })
        .await
    }
}

#[async_trait]
impl Run for BackendManager {
    async fn run(&self, token: CancellationToken) -> Result<(), Error> {
        let mut sig = signal(SignalKind::hangup()).context("unable to listen for SIGHUP")?;

        warn!("{self}: task started");
        loop {
            select! {
                () = token.cancelled() => {
                    // Stop accepting new config updates
                    self.updates.close();

                    // An update that got past the `is_closed()` check in `run_update()` did so while
                    // holding the config lock, and it keeps the lock until it finishes.
                    // Taking the lock here waits for such an update, even if it wasn't
                    // registered with the tracker yet when `wait()` below would have returned.
                    drop(self.config.lock().await);

                    // Wait for the config updates in progress to finish
                    self.updates.wait().await;

                    warn!("{self}: task stopped");
                    break;
                },

                _ = sig.recv() => {
                    warn!("{self}: received config reload signal");

                    let mut config = self.config.lock().await;
                    match self.load_config_locked(&mut config).await {
                        Ok(()) => warn!("{self}: configuration reloaded successfully"),
                        Err(e) => warn!("{self}: failed to reload config: {e:#}"),
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
            .unwrap(); // This should never fail

        let res = if let Ok(res) = timeout(self.timeout, self.client.execute(req)).await {
            match res {
                Ok(v) => v,
                Err(e) => {
                    info!("Health check failed for {target}: request failed: {e:#}");
                    return TargetState::Degraded;
                }
            }
        } else {
            info!("Health check failed for {target}: request timed out");
            return TargetState::Degraded;
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
    client: Arc<dyn ClientHttp<Body>>,
}

#[async_trait]
impl ExecutesRequest<Arc<Backend>> for RequestExecutor {
    type Request = Request<Body>;
    type Response = Response<Body>;
    type Error = HttpError;

    async fn execute(
        &self,
        backend: &Arc<Backend>,
        mut req: Self::Request,
    ) -> Result<Self::Response, Self::Error> {
        const PQ_DEFAULT: PathAndQuery = PathAndQuery::from_static("/");

        // Store the selected backend in the request context
        let _ = REQUEST_CONTEXT.try_with(|x| {
            x.borrow_mut().backend = Some(backend.clone());
        });

        let uri = match Uri::builder()
            .scheme(backend.url.scheme())
            .authority(backend.url.authority())
            .path_and_query(req.uri().path_and_query().unwrap_or(&PQ_DEFAULT).as_str())
            .build()
        {
            Ok(v) => v,
            Err(e) => {
                warn!(
                    "Invalid URL '{}://{}{}': {e:#}",
                    backend.url.scheme(),
                    backend.url.authority(),
                    req.uri().path_and_query().unwrap_or(&PQ_DEFAULT),
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

#[cfg(test)]
pub mod test {
    use std::{net::SocketAddr, path::PathBuf, time::Instant};

    use axum::Router;
    use http::StatusCode;
    use ic_bn_lib::http::{HyperClient, Server, server::ServerOptions};
    use tokio::net::TcpListener;

    use super::*;

    fn backend_conf(name: &str, url: &str) -> BackendConf {
        BackendConf {
            name: name.into(),
            url: Url::parse(url).unwrap(),
            enabled: true,
            weight: 1,
        }
    }

    fn new_manager() -> Arc<BackendManager> {
        let client = Arc::new(HyperClient::default());
        Arc::new(BackendManager::new(
            client,
            PathBuf::new(),
            Duration::from_millis(50),
            Duration::from_millis(500),
            &Registry::new(),
        ))
    }

    /// Spawns a minimal HTTP server that responds to any request (including `/health`) with 200.
    pub async fn spawn_healthy_backend() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = Server::new(
            ic_bn_lib::network::Addr::Tcp(addr),
            Router::new().fallback(async || StatusCode::OK),
            ServerOptions::default(),
            ic_bn_lib::http::server::metrics::Metrics::new(&Registry::new()),
            None,
        );

        tokio::spawn(async move {
            server
                .serve_with_listener(listener.into(), CancellationToken::new())
                .await
                .unwrap();
        });

        addr
    }

    /// Spawns a TCP listener that accepts connections but never responds,
    /// so that health checks against it always time out.
    pub async fn spawn_blackhole_backend() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            loop {
                let (s, _) = listener.accept().await.unwrap();

                // Keep the connection open without ever responding
                tokio::spawn(async move {
                    let _s = s;
                    std::future::pending::<()>().await;
                });
            }
        });

        addr
    }

    #[test]
    fn test_config_default() {
        let cfg = Config::default();
        assert_eq!(cfg.strategy, Strategy::LeastOutstandingRequests);
        assert_eq!(cfg.backends, Vec::<BackendConf>::new());
        assert!(cfg.fallback.is_none());
    }

    #[test]
    fn test_backend_from_conf_builds_health_uri() {
        let conf = backend_conf("foo", "http://example.com:1234/base/path");
        let backend: Backend = conf.into();

        assert_eq!(backend.name, "foo");
        assert_eq!(
            backend.uri_health.to_string(),
            "http://example.com:1234/health"
        );
    }

    #[tokio::test]
    async fn test_set_config_rejects_duplicate_names() {
        let bm = new_manager();

        let config = Config {
            strategy: Strategy::LeastOutstandingRequests,
            backends: vec![backend_conf("dup", "http://127.0.0.1:1")],
            fallback: Some(vec![backend_conf("dup", "http://127.0.0.1:2")]),
        };

        let err = bm.set_config(config, false).await.unwrap_err();
        assert!(err.to_string().contains("Non-unique"));
    }

    #[tokio::test]
    async fn test_set_config_empty_backends_ok() {
        let bm = new_manager();

        let config = Config {
            strategy: Strategy::LeastOutstandingRequests,
            backends: vec![],
            fallback: None,
        };

        bm.set_config(config.clone(), false).await.unwrap();
        assert_eq!(bm.get_config().await, config);
        assert!(bm.get_backend_router().is_none());
        assert!(bm.get_healthy_nodes().is_empty());
    }

    #[tokio::test]
    async fn test_set_backend_state_not_found() {
        let bm = new_manager();
        let err = bm
            .set_backend_state("nonexistent".into(), true)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Backend not found"));
    }

    #[tokio::test]
    async fn test_set_backend_state_toggle() {
        let bm = new_manager();

        // Backend is disabled initially - no health check happens
        let config = Config {
            strategy: Strategy::LeastOutstandingRequests,
            backends: vec![BackendConf {
                enabled: false,
                ..backend_conf("foo", "http://127.0.0.1:1")
            }],
            fallback: None,
        };
        bm.set_config(config, false).await.unwrap();
        assert!(bm.get_backend_router().is_none());

        // Enabling it triggers a health check against a closed port -> stays unhealthy
        bm.set_backend_state("foo".into(), true).await.unwrap();
        assert!(bm.get_healthy_nodes().is_empty());
        assert!(bm.get_backend_router().is_none());

        // Disabling it again removes the router entirely
        bm.set_backend_state("foo".into(), false).await.unwrap();
        assert!(bm.get_backend_router().is_none());
    }

    #[tokio::test]
    async fn test_set_backend_state_ignores_slow_fallback() {
        let bm = new_manager();
        let healthy_addr = spawn_healthy_backend().await;
        let blackhole_addr = spawn_blackhole_backend().await;

        let config = Config {
            strategy: Strategy::LeastOutstandingRequests,
            backends: vec![BackendConf {
                enabled: false,
                ..backend_conf("main", &format!("http://{healthy_addr}"))
            }],
            fallback: Some(vec![backend_conf(
                "fb",
                &format!("http://{blackhole_addr}"),
            )]),
        };

        // This one waits for the fallback health check to time out
        bm.set_config(config, false).await.unwrap();

        // Enabling a main backend must not wait for the fallback health checks
        let start = Instant::now();
        bm.set_backend_state("main".into(), true).await.unwrap();
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(250),
            "enabling took {elapsed:?}"
        );

        let healthy = bm.get_healthy_nodes();
        assert_eq!(healthy.len(), 1);
        assert_eq!(healthy[0].name, "main");
    }

    #[tokio::test]
    async fn test_set_backend_state_does_not_wait_for_old_router() {
        let bm = new_manager();
        let healthy_addr = spawn_healthy_backend().await;
        let blackhole_addr = spawn_blackhole_backend().await;

        let config = Config {
            strategy: Strategy::LeastOutstandingRequests,
            backends: vec![
                backend_conf("good", &format!("http://{healthy_addr}")),
                backend_conf("bad", &format!("http://{blackhole_addr}")),
            ],
            fallback: None,
        };

        bm.set_config(config, false).await.unwrap();

        // The old router has a health check of "bad" in-flight most of the time,
        // stopping it must not block the API call
        let start = Instant::now();
        bm.set_backend_state("bad".into(), false).await.unwrap();
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(250),
            "disabling took {elapsed:?}"
        );

        let healthy = bm.get_healthy_nodes();
        assert_eq!(healthy.len(), 1);
        assert_eq!(healthy[0].name, "good");
    }

    #[tokio::test]
    async fn test_update_cancelled_while_waiting_for_lock_does_nothing() {
        let bm = new_manager();
        let healthy_addr = spawn_healthy_backend().await;
        let blackhole_addr = spawn_blackhole_backend().await;

        let config = Config {
            strategy: Strategy::LeastOutstandingRequests,
            backends: vec![
                backend_conf("good", &format!("http://{healthy_addr}")),
                backend_conf("bad", &format!("http://{blackhole_addr}")),
            ],
            fallback: None,
        };

        // Applying it holds the lock for the health check timeout due to the "bad" backend
        let bm2 = bm.clone();
        let slow = tokio::spawn(async move { bm2.set_config(config, false).await });
        tokio::time::sleep(Duration::from_millis(50)).await;

        // This one queues up behind it and its caller goes away before it gets the lock
        let bm2 = bm.clone();
        let queued = tokio::spawn(async move { bm2.set_backend_state("good".into(), false).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!queued.is_finished());
        queued.abort();

        slow.await.unwrap().unwrap();

        // The cancelled update must never be applied
        tokio::time::sleep(Duration::from_millis(100)).await;
        let cfg = bm.get_config().await;
        assert!(cfg.backends.iter().all(|x| x.enabled));
    }

    #[tokio::test]
    async fn test_shutdown_waits_for_updates() {
        let bm = new_manager();
        let blackhole_addr = spawn_blackhole_backend().await;

        let token = CancellationToken::new();
        let bm2 = bm.clone();
        let token2 = token.clone();
        let runner = tokio::spawn(async move { bm2.run(token2).await });

        let config = Config {
            strategy: Strategy::LeastOutstandingRequests,
            backends: vec![backend_conf("bad", &format!("http://{blackhole_addr}"))],
            fallback: None,
        };

        // Start an update that takes the health check timeout and make its caller go away
        let bm2 = bm.clone();
        let config2 = config.clone();
        let caller = tokio::spawn(async move { bm2.set_config(config2, false).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        caller.abort();

        // Shutdown must wait for the update to finish
        token.cancel();
        runner.await.unwrap().unwrap();
        assert_eq!(
            *bm.config.try_lock().expect("update must be finished"),
            config
        );

        // No new updates are accepted after that
        assert!(bm.set_config(Config::default(), false).await.is_err());
        assert_eq!(bm.get_config().await, config);
    }

    #[tokio::test]
    async fn test_get_backend_router_falls_back_when_main_unhealthy() {
        let bm = new_manager();
        let healthy_addr = spawn_healthy_backend().await;

        let config = Config {
            strategy: Strategy::LeastOutstandingRequests,
            backends: vec![backend_conf("main", "http://127.0.0.1:1")],
            fallback: Some(vec![backend_conf("fb", &format!("http://{healthy_addr}"))]),
        };

        bm.set_config(config, false).await.unwrap();

        // Main backend is unreachable, so it has no healthy nodes
        assert!(bm.get_healthy_nodes().is_empty());

        // But the router falls back to the healthy fallback backend
        let router = bm.get_backend_router().expect("should fall back");
        let healthy = router.get_healthy();
        assert_eq!(healthy.len(), 1);
        assert_eq!(healthy[0].name, "fb");
    }
}
