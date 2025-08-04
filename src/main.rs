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
