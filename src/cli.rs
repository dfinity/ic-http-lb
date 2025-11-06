use std::{net::SocketAddr, path::PathBuf, time::Duration};

use clap::{Args, Parser};
use fqdn::FQDN;
use humantime::parse_duration;
use ic_bn_lib_common::{
    parse_size_usize,
    types::{
        acme::AcmeUrl,
        dns::DnsCli,
        http::{HttpClientCli, HttpServerCli, WafCli},
        vector::VectorCli,
    },
};
use url::Url;

use crate::core::{AUTHOR_NAME, SERVICE_NAME};

#[derive(Parser)]
#[clap(name = SERVICE_NAME)]
#[clap(author = AUTHOR_NAME)]
pub struct Cli {
    #[command(flatten, next_help_heading = "Config")]
    pub config: Config,

    #[command(flatten, next_help_heading = "Listen")]
    pub listen: Listen,

    #[command(flatten, next_help_heading = "HTTP Server")]
    pub http_server: HttpServerCli,

    #[command(flatten, next_help_heading = "HTTP Client")]
    pub http_client: HttpClientCli,

    #[command(flatten, next_help_heading = "Network")]
    pub network: Network,

    #[command(flatten, next_help_heading = "Limits")]
    pub limits: Limits,

    #[command(flatten, next_help_heading = "Retry")]
    pub retry: Retry,

    #[command(flatten, next_help_heading = "API")]
    pub api: Api,

    #[command(flatten, next_help_heading = "Health")]
    pub health: Health,

    #[command(flatten, next_help_heading = "DNS")]
    pub dns: DnsCli,

    #[command(flatten, next_help_heading = "Custom Domains")]
    pub custom_domains: Option<custom_domains_base::cli::CustomDomainsCli>,

    #[command(flatten, next_help_heading = "Certificates")]
    pub cert: Cert,

    #[command(flatten, next_help_heading = "Logging")]
    pub log: Log,

    #[command(flatten, next_help_heading = "WAF")]
    pub waf: WafCli,

    #[cfg(all(target_os = "linux", feature = "sev-snp"))]
    #[command(flatten, next_help_heading = "SEV-SNP")]
    pub sev_snp: ic_bn_lib_common::types::utils::SevSnpCli,

    #[command(flatten, next_help_heading = "Misc")]
    pub misc: Misc,
}

#[derive(Args)]
pub struct Listen {
    /// Where to listen for HTTP requests
    #[clap(env, long, default_value = "127.0.0.1:8080")]
    pub listen_http: SocketAddr,

    /// Where to listen for HTTPS requests
    #[clap(env, long, default_value = "127.0.0.1:8443")]
    pub listen_https: SocketAddr,

    /// Where to listen for metrics
    #[clap(env, long)]
    pub listen_metrics: Option<SocketAddr>,

    /// Option to only serve HTTP instead for testing
    #[clap(env, long)]
    pub listen_insecure_serve_http_only: bool,
}

#[derive(Args)]
pub struct Network {
    /// Number of HTTP clients to create to spread the load over
    #[clap(env, long, default_value = "16", value_parser = clap::value_parser!(u16).range(1..))]
    pub network_http_client_count: u16,

    /// Bypass verification of TLS certificates for all outgoing requests.
    /// *** Dangerous *** - use only for testing.
    #[clap(env, long)]
    pub network_http_client_insecure_bypass_tls_verification: bool,

    /// Whether to buffer request body from the client before sending it to the backend.
    /// If `retry_attempts` is >1 then this is implicitly enabled.
    /// The body is buffered only if it's size is known and smaller than `--limits-request-body-size`
    #[clap(env, long)]
    pub network_request_body_buffer: bool,

    /// Whether to buffer response body from the backend before sending it to the client.
    /// The body is buffered only if it's size is known and smaller than `--limits-response-body-size`
    #[clap(env, long)]
    pub network_response_body_buffer: bool,
}

#[derive(Args)]
pub struct Api {
    /// Where to listen for API requests in addition to the hostname below.
    #[clap(env, long, requires = "api_token")]
    pub api_listen: Option<SocketAddr>,

    /// Specify a hostname on which to respond to API requests.
    /// Requires `api_token` to be set.
    /// If not specified - API isn't enabled.
    #[clap(env, long, requires = "api_token")]
    pub api_hostname: Option<FQDN>,

    /// Set an API authentication token.
    #[clap(env, long)]
    pub api_token: Option<String>,

    /// Whether to try to issue the certificate for the `api_hostname`.
    /// If enabled - `api_acme_cache` needs to be set.
    #[clap(env, long, requires = "api_acme_cache")]
    pub api_acme: bool,

    /// Which ACME provider URL to use. Can be "le_stag", "le_prod" for LetsEncrypt, or a custom URL.
    /// Defaults to "le_stag".
    #[clap(env, long, default_value = "le_stag")]
    pub api_acme_url: AcmeUrl,

    /// Path to a folder where to store ACME cache (account, certificates etc)
    #[clap(env, long)]
    pub api_acme_cache: Option<PathBuf>,
}

#[derive(Args)]
pub struct Health {
    /// How frequently to check health of the backends
    #[clap(env, long, default_value = "1s", value_parser = parse_duration)]
    pub health_check_interval: Duration,

    /// How long to wait for the health check to finish.
    /// This is applied on top of all HTTP timeouts and has precedence.
    #[clap(env, long, default_value = "10s", value_parser = parse_duration)]
    pub health_check_timeout: Duration,
}

#[derive(Args)]
pub struct Cert {
    /// Read certificates from given files.
    /// Each file should be PEM-encoded concatenated certificate chain with a private key.
    #[clap(env, long, value_delimiter = ',')]
    pub cert_provider_file: Vec<PathBuf>,

    /// Read certificates from given directories
    /// Each certificate should be a pair .pem + .key files with the same base name.
    #[clap(env, long, value_delimiter = ',')]
    pub cert_provider_dir: Vec<PathBuf>,

    /// Request certificates from the 'certificate-issuer' instances reachable over given URLs.
    #[clap(env, long, value_delimiter = ',')]
    pub cert_provider_issuer_url: Vec<Url>,

    /// How frequently to refresh certificate issuers
    #[clap(env, long, default_value = "30s", value_parser = parse_duration)]
    pub cert_provider_issuer_poll_interval: Duration,

    /// How frequently to poll providers for certificates
    #[clap(env, long, default_value = "5s", value_parser = parse_duration)]
    pub cert_provider_poll_interval: Duration,

    /// Default certificate to serve when there's no SNI in the request.
    /// Tries to find a certificate that covers given FQDN.
    /// If not found or not specified - picks the first one available.
    #[clap(env, long)]
    pub cert_default: Option<FQDN>,
}

#[derive(Args)]
pub struct Config {
    /// Path to the YAML or JSON file with backend configuration (see `config.yaml` for an example)
    #[clap(env, long)]
    pub config_path: PathBuf,
}

#[derive(Args)]
pub struct Retry {
    /// Number of request attempts to do.
    /// Only network errors are retried, not HTTP codes.
    /// If the number of attempts is 1 then we don't retry.
    #[clap(env, long, default_value = "1", value_parser = clap::value_parser!(u8).range(1..))]
    pub retry_attempts: u8,

    /// Initial retry interval, with each retry it is doubled.
    /// If there are no healthy nodes then we wait 1/4 of `health_check_interval` instead in
    /// hope that some node would become healthy.
    #[clap(env, long, default_value = "50ms", value_parser = parse_duration)]
    pub retry_interval: Duration,
}

#[derive(Args)]
pub struct Limits {
    /// Maximum request body size if buffering.
    #[clap(env, long, default_value = "10MB", value_parser = parse_size_usize)]
    pub limits_request_body_size: usize,

    /// Maximum time allowed to buffer the request body.
    #[clap(env, long, default_value = "30s", value_parser = parse_duration)]
    pub limits_request_body_timeout: Duration,

    /// Maximum response body size if buffering.
    #[clap(env, long, default_value = "30MB", value_parser = parse_size_usize)]
    pub limits_response_body_size: usize,

    /// Maximum time allowed to buffer the response body.
    #[clap(env, long, default_value = "60s", value_parser = parse_duration)]
    pub limits_response_body_timeout: Duration,
}

#[derive(Args)]
pub struct Log {
    /// Logging level to use
    #[clap(env, long, default_value = "warn")]
    pub log_level: tracing::Level,

    /// Enables logging to stdout
    #[clap(env, long)]
    pub log_stdout: bool,

    /// Enables logging of all HTTP requests to stdout at INFO level.
    /// This does not affect Vector logging,
    /// if it's enabled then it will log in any case.
    #[clap(env, long)]
    pub log_requests: bool,

    /// Log requests that take longer than this to complete at WARN level.
    /// This works independently of `--log-requests` and raises the log level from INFO to WARN if
    /// the request duration exceeds this.
    #[clap(env, long, value_parser = parse_duration)]
    pub log_requests_long: Option<Duration>,

    #[command(flatten, next_help_heading = "Vector")]
    pub vector: VectorCli,
}

#[derive(Args)]
pub struct Misc {
    /// Environment we run in to specify in the logs
    #[clap(env, long, default_value = "dev")]
    pub env: String,

    /// Local hostname to identify in e.g. logs.
    /// If not specified - tries to obtain it.
    #[clap(env, long, default_value = hostname::get().unwrap().into_string().unwrap())]
    pub hostname: String,

    /// Number of Tokio threads to use to serve requests.
    /// Defaults to the number of CPUs
    #[clap(env, long)]
    pub threads: Option<usize>,
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_cli() {
        let args: Vec<&str> = vec!["", "--config-path", "foo"];
        Cli::parse_from(args);
    }
}
