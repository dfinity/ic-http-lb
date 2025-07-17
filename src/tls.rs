use std::sync::Arc;

use anyhow::{Error, bail};
use ic_bn_lib::{
    http::{ALPN_ACME, Client},
    rustls::{
        server::{ResolvesServerCert, ServerConfig},
        version::{TLS12, TLS13},
    },
    tasks::TaskManager,
    tls::{
        self,
        acme::alpn::{AcmeAlpn, Opts},
        prepare_server_config,
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

    // Create Dir providers
    for v in &cli.cert.cert_provider_dir {
        cert_providers.push(Arc::new(providers::Dir::new(v.clone())));
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

    // Setup ACME ALPN for API endpoint if configured
    let api_acme_resolver: Option<Arc<dyn ResolvesServerCert>> = if cli.api.api_acme {
        let acme_alpn = Arc::new(AcmeAlpn::new(Opts {
            acme_url: cli.api.api_acme_url.clone(),
            domains: vec![cli.api.api_hostname.clone().unwrap().to_string()],
            contact: "mailto:boundary-nodes@dfinity.org".to_string(),
            cache_path: cli.api.api_acme_cache.clone().unwrap(),
        }));
        tasks.add("acme_alpn", acme_alpn.clone());

        Some(acme_alpn)
    } else {
        None
    };

    // Set up certificate resolver
    let certificate_resolver = Arc::new(resolver::AggregatingResolver::new(
        api_acme_resolver,
        vec![cert_storage],
        resolver::Metrics::new(registry),
    ));

    let mut tls_opts: tls::Options = (&cli.http_server).into();
    tls_opts.tls_versions = vec![&TLS13, &TLS12];

    // To perform TLS-ALPN-01 validation we need to add ACME ALPN to the list
    if cli.api.api_acme {
        tls_opts.additional_alpn = vec![ALPN_ACME.to_vec()];
    }

    // Generate Rustls config
    let config = prepare_server_config(tls_opts, certificate_resolver, registry);

    Ok(config)
}
