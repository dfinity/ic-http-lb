mod api;
mod backend;
mod cli;
mod core;
mod log;
mod metrics;
mod middleware;
mod routing;
mod tls;

use anyhow::{Context, Error};
use clap::Parser;
use tikv_jemallocator::Jemalloc;
use tracing::warn;

use crate::{cli::Cli, log::setup_logging};

#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

fn main() -> Result<(), Error> {
    let cli = Cli::parse();
    let log_handle = setup_logging(&cli.log).context("unable to setup logging")?;

    let threads = if let Some(v) = cli.misc.threads {
        v
    } else {
        std::thread::available_parallelism()
            .context("unable to get the number of CPUs")?
            .get()
    };

    warn!(
        "Env: {}, Hostname: {}, using {threads} threads",
        cli.misc.env, cli.misc.hostname
    );

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(threads)
        .build()?
        .block_on(core::main(&cli, log_handle))
        .context("Startup failed")?;

    Ok(())
}

#[cfg(test)]
mod test {
    use std::{net::SocketAddr, str::FromStr, time::Duration};

    use axum::{Router, routing::get};
    use clap::Parser;
    use http::StatusCode;
    use ic_bn_lib::{
        http::Server,
        reqwest,
        tests::{TEST_CERT_1, TEST_KEY_1},
    };
    use ic_bn_lib_common::types::http::{Addr, Metrics, ServerOptions};
    use prometheus::Registry;
    use serde_json::json;
    use tempfile::tempdir;
    use tokio::{fs, net::TcpListener, time::sleep};
    use tokio_util::sync::CancellationToken;

    use crate::{cli::Cli, log::setup_logging};

    #[tokio::test]
    async fn test_load_balancer() {
        // Set up stub backend on a random port
        let opts = ServerOptions::default();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = Server::new(
            Addr::Tcp(addr),
            Router::new()
                .route("/health", get(async || "HEALTHY"))
                .fallback(async || "DEADBEEF"),
            opts,
            Metrics::new(&Registry::new()),
            None,
        );

        tokio::spawn(async move {
            server
                .serve_with_listener(listener.into(), CancellationToken::new())
                .await
                .unwrap();
        });

        // Create a temporary config pointing to a stub backend
        let dir = tempdir().unwrap();
        let cfg = json!({
            "strategy": "wrr",
            "backends": [
                // Good backend
                {
                    "name": "foo",
                    "url": format!("http://{addr}"),
                    "enabled": true,
                    "weight": 1,
                },
                // Bad backend
                {
                    "name": "bar",
                    "url": format!("http://127.0.0.1:666"),
                    "enabled": true,
                    "weight": 1,
                }
            ]
        })
        .to_string();

        fs::write(dir.path().join("config.json"), cfg)
            .await
            .unwrap();

        fs::write(
            dir.path().join("test_cert.pem"),
            [TEST_CERT_1, TEST_KEY_1].concat(),
        )
        .await
        .unwrap();

        let cfg_path = dir.path().join("config.json");
        let crt_path = dir.path().join("test_cert.pem");

        let args = vec![
            "",
            "--config-path",
            cfg_path.to_str().unwrap(),
            "--listen-http",
            "127.0.0.1:18080",
            "--listen-https",
            "127.0.0.1:18443",
            "--cert-provider-file",
            crt_path.to_str().unwrap(),
            "--api-hostname",
            "foobarhost",
            "--api-token",
            "blah",
        ];

        let cli = Cli::parse_from(args);
        let log_handle = setup_logging(&cli.log).unwrap();

        tokio::spawn(async move { crate::core::main(&cli, log_handle).await.unwrap() });

        let client = reqwest::ClientBuilder::new()
            .danger_accept_invalid_certs(true)
            .danger_accept_invalid_hostnames(true)
            .resolve("foobarhost", SocketAddr::from_str("127.0.0.1:0").unwrap())
            .build()
            .unwrap();

        // Wait until the LB is healthy for around 10s
        for _ in 0..1000 {
            let r = client
                .execute(
                    client
                        .get("https://foobarhost:18443/health")
                        .build()
                        .unwrap(),
                )
                .await;

            if let Ok(v) = r
                && v.status() == StatusCode::NO_CONTENT
            {
                break;
            }

            sleep(Duration::from_millis(10)).await;
        }

        // Check that the request is proxied to the backend correctly
        let r = client.get("https://127.0.0.1:18443/test").build().unwrap();
        let r = client.execute(r).await.unwrap();
        assert!(r.status().is_success());

        let body = r.text().await.unwrap();
        assert_eq!(body, "DEADBEEF");
    }
}
