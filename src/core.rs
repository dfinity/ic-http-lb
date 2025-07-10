use std::sync::{Arc, OnceLock};

use anyhow::{Context, Error};
use arc_swap::ArcSwapOption;
use axum::Router;
use ic_bn_lib::{
    http::{
        self as bnhttp, HyperClientLeastLoaded, ReqwestClient, ServerBuilder, dns,
        redirect_to_https,
    },
    rustls,
    tasks::TaskManager,
    tls::{prepare_client_config, verify::NoopServerCertVerifier},
};
use prometheus::Registry;
use tokio::signal::ctrl_c;
use tracing::warn;

use crate::{
    api::setup_api_axum_router, backend::BackendManager, cli::Cli, routing::setup_axum_router, tls,
};

pub const SERVICE_NAME: &str = "ic-http-lb";
pub const AUTHOR_NAME: &str = "Boundary Node Team <boundary-nodes@dfinity.org>";

// Store env/hostname in statics so that we don't have to clone them
pub static ENV: OnceLock<String> = OnceLock::new();
pub static HOSTNAME: OnceLock<String> = OnceLock::new();

pub async fn main(cli: &Cli) -> Result<(), Error> {
    ENV.set(cli.misc.env.clone()).unwrap();
    HOSTNAME.set(cli.misc.hostname.clone()).unwrap();

    let mut tasks = TaskManager::new();
    let registry = Registry::new_custom(Some(SERVICE_NAME.into()), None)
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

    let http_client = Arc::new(
        HyperClientLeastLoaded::new(
            http_client_opts,
            resolver,
            cli.network.network_http_client_count as usize,
            None,
        )
        .context("unable to build HTTP client")?,
    );

    // Setup backend routing
    let backend_router = Arc::new(ArcSwapOption::empty());
    let backend_manager = Arc::new(BackendManager::new(
        http_client.clone(),
        cli.backends.backends_config.clone(),
        backend_router.clone(),
    ));

    let axum_router_api = if cli.api.api_hostname.is_some() || cli.api.api_listen.is_some() {
        Some(
            setup_api_axum_router(cli, backend_manager.clone())
                .context("unable to setup API Axum Router")?,
        )
    } else {
        None
    };

    let axum_router = setup_axum_router(cli, axum_router_api.clone(), backend_router)
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

    // Run background tasks
    tasks.add("backend_manager", backend_manager);
    tasks.add("server_http", server_http);

    tasks.start();

    // Wait for the shutdown
    warn!("Started");
    ctrl_c().await.unwrap();

    warn!("Shutdown signal received");
    tasks.stop().await;

    warn!("Shutdown complete");
    Ok(())
}
