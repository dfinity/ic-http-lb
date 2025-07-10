mod api;
mod backend;
mod cli;
mod core;
mod log;
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
    setup_logging(&cli.log).context("unable to setup logging")?;

    warn!("Env: {}, Hostname: {}", cli.misc.env, cli.misc.hostname);

    let threads = if let Some(v) = cli.misc.threads {
        v
    } else {
        std::thread::available_parallelism()
            .context("unable to get the number of CPUs")?
            .get()
    };

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(threads)
        .build()?
        .block_on(core::main(&cli))
        .context("failure")?;

    Ok(())
}
