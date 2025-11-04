use std::sync::{Arc, OnceLock};

use anyhow::{Context, Error};
use axum::{Router, body::Body};
use ic_bn_lib::{
    http::{
        self as bnhttp, ClientHttp, HyperClient, HyperClientLeastLoaded, ReqwestClient,
        ServerBuilder, dns, middleware::waf::WafLayer, redirect_to_https,
    },
    rustls,
    tasks::TaskManager,
    tls::{prepare_client_config, verify::NoopServerCertVerifier},
    vector::client::Vector,
};
use prometheus::Registry;
use tokio::{
    select,
    signal::{
        ctrl_c,
        unix::{SignalKind, signal},
    },
};
use tracing::warn;
use tracing_core::LevelFilter;
use tracing_subscriber::reload::Handle;

use crate::{
    api::setup_api_axum_router, backend::BackendManager, cli::Cli, metrics,
    routing::setup_axum_router, tls,
};

pub const SERVICE_NAME: &str = "ic_http_lb";
pub const AUTHOR_NAME: &str = "Boundary Node Team <boundary-nodes@dfinity.org>";

// Store env/hostname in statics so that we don't have to clone them
pub static ENV: OnceLock<String> = OnceLock::new();
pub static HOSTNAME: OnceLock<String> = OnceLock::new();

pub async fn main(
    cli: &Cli,
    log_handle: Handle<LevelFilter, tracing_subscriber::Registry>,
) -> Result<(), Error> {
    ENV.set(cli.misc.env.clone()).unwrap();
    HOSTNAME.set(cli.misc.hostname.clone()).unwrap();

    let mut tasks = TaskManager::new();
    let registry = Registry::new_custom(Some(SERVICE_NAME.to_string()), None)
        .context("unable to create Prometheus registry")?;

    // Create HTTP client to talk to the backends
    let mut http_client_opts: bnhttp::client::Options = (&cli.http_client).into();
    let mut http_client_tls_config = prepare_client_config(&[&rustls::version::TLS13]);

    // Disable TLS certificate verification if instructed
    if cli
        .network
        .network_http_client_insecure_bypass_tls_verification
    {
        http_client_tls_config
            .dangerous()
            .set_certificate_verifier(Arc::new(NoopServerCertVerifier::default()));
    }

    http_client_opts.tls_config = Some(http_client_tls_config);

    let resolver = dns::Resolver::new((&cli.dns).into());

    let http_client_reqwest = Arc::new(
        ReqwestClient::new(http_client_opts.clone(), Some(resolver.clone()))
            .context("unable to setup HTTP client")?,
    );

    let http_client: Arc<dyn ClientHttp<Body>> = if cli.network.network_http_client_count > 1 {
        Arc::new(HyperClientLeastLoaded::new(
            http_client_opts,
            resolver,
            cli.network.network_http_client_count as usize,
            Some(&registry),
        ))
    } else {
        Arc::new(HyperClient::new(http_client_opts, resolver))
    };

    // Setup Vector
    let vector = cli.log.vector.log_vector_url.as_ref().map(|_| {
        Arc::new(Vector::new(
            &cli.log.vector,
            http_client_reqwest.clone(),
            &registry,
        ))
    });

    // Setup backend routing
    let backend_manager = Arc::new(BackendManager::new(
        http_client.clone(),
        cli.config.config_path.clone(),
        cli.health.health_check_interval,
        cli.health.health_check_timeout,
        &registry,
    ));

    // Setup WAF
    let waf_layer = if cli.waf.waf_enable {
        let v = WafLayer::new_from_cli(&cli.waf, Some(http_client_reqwest.clone()))
            .context("unable to create WAF layer")?;

        // Run background poller
        tasks.add("waf", Arc::new(v.clone()));
        Some(v)
    } else {
        None
    };

    // Setup API router if configured
    let axum_router_api = if cli.api.api_hostname.is_some() || cli.api.api_listen.is_some() {
        Some(
            setup_api_axum_router(cli, backend_manager.clone(), log_handle, waf_layer.clone())
                .context("unable to setup API Axum Router")?,
        )
    } else {
        None
    };

    let axum_router = setup_axum_router(
        cli,
        axum_router_api.clone(),
        backend_manager.clone(),
        vector.clone(),
        &registry,
        waf_layer,
    )
    .context("unable to setup Axum Router")?;

    // HTTP server metrics
    let http_metrics = bnhttp::server::Metrics::new(&registry);

    // Set up HTTP router (redirecting to HTTPS or serving all endpoints)
    let axum_router_http = if !cli.listen.listen_insecure_serve_http_only {
        Router::new().fallback(redirect_to_https)
    } else {
        axum_router.clone()
    };

    let server_http = Arc::new(
        ServerBuilder::new(axum_router_http)
            .listen_tcp(cli.listen.listen_http)
            .with_options((&cli.http_server).into())
            .with_metrics(http_metrics.clone())
            .build()
            .context("unable to build HTTP server")?,
    );
    tasks.add("server_http", server_http);

    // Start API server if configured
    if let (Some(listen), Some(router)) = (cli.api.api_listen, axum_router_api) {
        let server_api = Arc::new(
            ServerBuilder::new(router)
                .listen_tcp(listen)
                .with_options((&cli.http_server).into())
                .with_metrics(http_metrics.clone())
                .build()
                .context("unable to build API HTTP server")?,
        );

        tasks.add("server_api", server_api);
    }

    // Start metrics server if configures
    if let Some(addr) = cli.listen.listen_metrics {
        let router = metrics::setup(&registry, &mut tasks);

        let srv = Arc::new(
            bnhttp::ServerBuilder::new(router)
                .listen_tcp(addr)
                .with_options((&cli.http_server).into())
                .with_metrics(http_metrics.clone())
                .build()
                .unwrap(),
        );

        tasks.add("metrics_server", srv);
    }

    // Create HTTPS server
    if !cli.listen.listen_insecure_serve_http_only {
        // Prepare TLS related stuff
        let rustls_cfg = tls::setup(cli, &mut tasks, http_client_reqwest, &registry)
            .await
            .context("unable to setup TLS")?;

        let server_https = Arc::new(
            ServerBuilder::new(axum_router)
                .listen_tcp(cli.listen.listen_https)
                .with_options((&cli.http_server).into())
                .with_metrics(http_metrics)
                .with_rustls_config(rustls_cfg)
                .build()
                .context("unable to build HTTP server")?,
        );

        tasks.add("server_https", server_https);
    }

    // Load the initial config
    backend_manager
        .load_config()
        .await
        .context("unable to load backends config")?;
    tasks.add("backend_manager", backend_manager);

    // Run background tasks
    tasks.start();

    // Wait for the shutdown
    warn!("Started");
    let mut sig = signal(SignalKind::terminate()).context("unable to listen for SIGTERM")?;
    select! {
        _ = sig.recv() => {}
        _ = ctrl_c() => {}
    }

    warn!("Shutdown signal received");
    tasks.stop().await;

    // Vector should stop last to ensure that all requests are finished & flushed
    if let Some(v) = vector {
        v.stop().await;
    }

    warn!("Shutdown complete");
    Ok(())
}
