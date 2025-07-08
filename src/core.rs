use std::sync::{Arc, OnceLock};

use anyhow::{Context, Error};
use arc_swap::ArcSwapOption;
use ic_bn_lib::{
    http::{self as bnhttp, ReqwestClientLeastLoaded, dns},
    rustls,
    tls::{prepare_client_config, verify::NoopServerCertVerifier},
};
use tokio::{
    select,
    signal::unix::{SignalKind, signal},
};
use tokio_util::sync::CancellationToken;

use crate::{
    cli::Cli,
    routing::{BackendManager, setup_axum_router},
};

pub const SERVICE_NAME: &str = "ic-http-lb";
pub const AUTHOR_NAME: &str = "Boundary Node Team <boundary-nodes@dfinity.org>";

// Store env/hostname in statics so that we don't have to clone them
pub static ENV: OnceLock<String> = OnceLock::new();
pub static HOSTNAME: OnceLock<String> = OnceLock::new();

pub async fn main(cli: &Cli) -> Result<(), Error> {
    ENV.set(cli.misc.env.clone()).unwrap();
    HOSTNAME.set(cli.misc.hostname.clone()).unwrap();

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

    let resolver = dns::Resolver::new(dns::Options::default());
    let http_client = Arc::new(
        ReqwestClientLeastLoaded::new(
            http_client_opts,
            Some(resolver),
            cli.network.network_http_client_count as usize,
            None,
        )
        .context("unable to setup HTTP client")?,
    );

    let backend_router = Arc::new(ArcSwapOption::empty());
    let backend_manager = BackendManager::new(
        http_client,
        cli.backends.backends_config.clone(),
        backend_router.clone(),
    );

    let axum_router = setup_axum_router(backend_router).context("unable to setup Axum Router")?;

    let server = ic_bn_lib::http::ServerBuilder::new(axum_router)
        .listen_tcp(cli.listen.listen)
        .with_options((&cli.http_server).into())
        .build()
        .unwrap();

    backend_manager
        .load_config()
        .await
        .context("unable to load backends config")?;

    let mut sig = signal(SignalKind::hangup()).context("unable to listen for SIGHUP")?;
    tokio::spawn(async move {
        loop {
            select! {
                _ = sig.recv() => {
                    println!("got config reload signal");
                    match backend_manager.load_config().await {
                        Ok(_) => println!("configuration reloaded"),
                        Err(e) => println!("failed to reload config: {e:#}"),
                    }
                }
            }
        }
    });

    server
        .serve(CancellationToken::new())
        .await
        .context("unable to listen")?;

    Ok(())
}
