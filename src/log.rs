use anyhow::{Context, Error};
use tracing_subscriber::{
    filter::LevelFilter,
    fmt::layer,
    layer::{Layer, SubscriberExt},
    registry::Registry,
    reload::{self, Handle},
};

use crate::cli::Log;

// Sets up logging
pub fn setup_logging(cli: &Log) -> Result<Handle<LevelFilter, Registry>, Error> {
    let level_filter = LevelFilter::from_level(cli.log_level);
    let (level_filter, reload_handle) = reload::Layer::new(level_filter);

    let subscriber = Registry::default()
        // Stdout
        .with((cli.log_stdout).then(|| layer().with_filter(level_filter)));

    tracing::subscriber::set_global_default(subscriber)
        .context("unable to set global subscriber")?;

    Ok(reload_handle)
}
