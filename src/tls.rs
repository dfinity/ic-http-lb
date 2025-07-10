use std::sync::Arc;

use anyhow::{Error, bail};
use ic_bn_lib::{
    http::Client,
    rustls::{
        server::ServerConfig,
        version::{TLS12, TLS13},
    },
    tasks::TaskManager,
    tls::{
        self, prepare_server_config,
        providers::{self, Aggregator, Issuer, ProvidesCertificates, issuer, storage},
        resolver,
    },
};
use prometheus::Registry;

use crate::cli::Cli;

// Prepares the stuff needed for serving TLS
pub async fn setup(
    cli: &Cli,
    tasks: &mut TaskManager,
    http_client: Arc<dyn Client>,
    registry: &Registry,
) -> Result<ServerConfig, Error> {
    // Prepare certificate storage
    let cert_storage = Arc::new(storage::Storage::new(
        cli.cert.cert_default.clone(),
        storage::Metrics::new(registry),
    ));

    // Setup certificate providers
    let mut cert_providers: Vec<Arc<dyn ProvidesCertificates>> = vec![];

    // Create issuer providers
    let issuer_metrics = issuer::Metrics::new(registry);
    for v in &cli.cert.cert_provider_issuer_url {
        let issuer = Arc::new(Issuer::new(
            http_client.clone(),
            v.clone(),
            issuer_metrics.clone(),
        ));

        cert_providers.push(issuer.clone());
        tasks.add_interval(
            &format!("{issuer:?}"),
            issuer,
            cli.cert.cert_provider_issuer_poll_interval,
        );
    }

    // Create File providers
    for v in &cli.cert.cert_provider_file {
        cert_providers.push(Arc::new(providers::File::new(v.clone())));
    }

    if cert_providers.is_empty() {
        bail!("No certificate providers specified - HTTPS cannot be used");
    }

    // Create certificate aggregator that combines all providers
    let cert_aggregator = Arc::new(Aggregator::new(cert_providers, cert_storage.clone()));
    tasks.add_interval(
        "cert_aggregator",
        cert_aggregator,
        cli.cert.cert_provider_poll_interval,
    );

    // Set up certificate resolver
    let certificate_resolver = Arc::new(resolver::AggregatingResolver::new(
        None,
        vec![cert_storage],
        resolver::Metrics::new(registry),
    ));

    let mut tls_opts: tls::Options = (&cli.http_server).into();
    tls_opts.tls_versions = vec![&TLS13, &TLS12];

    // Generate Rustls config
    let config = prepare_server_config(tls_opts, certificate_resolver, registry);

    Ok(config)
}
