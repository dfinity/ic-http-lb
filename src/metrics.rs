use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Error, anyhow};
use arc_swap::ArcSwap;
use async_trait::async_trait;
use axum::{Router, extract::State, response::IntoResponse, routing::get};
use bytes::{BufMut, Bytes, BytesMut};
use http::header::CONTENT_TYPE;
use ic_bn_lib::tasks::TaskManager;
use ic_bn_lib_common::traits::Run;
use prometheus::{Encoder, IntGauge, Registry, TextEncoder, register_int_gauge_with_registry};
use tikv_jemalloc_ctl::{epoch, stats};
use tokio_util::sync::CancellationToken;
use tower_http::compression::CompressionLayer;
use tracing::{debug, warn};

// https://prometheus.io/docs/instrumenting/exposition_formats/#basic-info
const PROMETHEUS_CONTENT_TYPE: &str = "text/plain; version=0.0.4";

pub struct MetricsCache {
    buffer: ArcSwap<Bytes>,
}

#[allow(clippy::new_without_default)]
impl MetricsCache {
    pub fn new() -> Self {
        Self {
            buffer: ArcSwap::new(Arc::new(Bytes::from(vec![]))),
        }
    }
}

pub struct MetricsRunner {
    metrics_cache: Arc<MetricsCache>,
    registry: Registry,
    encoder: TextEncoder,

    // Metrics
    mem_allocated: IntGauge,
    mem_resident: IntGauge,
}

// Snapshots & encodes the metrics for the handler to export
impl MetricsRunner {
    pub fn new(metrics_cache: Arc<MetricsCache>, registry: &Registry) -> Self {
        let mem_allocated = register_int_gauge_with_registry!(
            format!("memory_allocated"),
            format!("Allocated memory in bytes"),
            registry
        )
        .unwrap();

        let mem_resident = register_int_gauge_with_registry!(
            format!("memory_resident"),
            format!("Resident memory in bytes"),
            registry
        )
        .unwrap();

        Self {
            metrics_cache,
            registry: registry.clone(),
            encoder: TextEncoder::new(),
            mem_allocated,
            mem_resident,
        }
    }
}

impl MetricsRunner {
    async fn update(&self) -> Result<(), Error> {
        // Record jemalloc memory usage
        epoch::advance().map_err(|e| anyhow!("unable to advance epoch: {e:#}"))?;

        self.mem_allocated.set(
            stats::allocated::read().map_err(|e| anyhow!("unable to read stats: {e:#}"))? as i64,
        );
        self.mem_resident.set(
            stats::resident::read().map_err(|e| anyhow!("unable to read stats: {e:#}"))? as i64,
        );

        // Get a snapshot of metrics
        let metric_families = self.registry.gather();

        // Encode the metrics into the buffer
        let mut buffer = BytesMut::with_capacity(10 * 1024 * 1024).writer();
        self.encoder.encode(&metric_families, &mut buffer)?;

        // Store the new snapshot
        self.metrics_cache
            .buffer
            .store(Arc::new(buffer.into_inner().freeze()));

        Ok(())
    }
}

#[async_trait]
impl Run for MetricsRunner {
    async fn run(&self, _: CancellationToken) -> Result<(), Error> {
        let start = Instant::now();
        if let Err(e) = self.update().await {
            warn!("Unable to update metrics: {e:#}");
        } else {
            debug!("Metrics updated in {}ms", start.elapsed().as_millis());
        }

        Ok(())
    }
}

pub async fn handler(State(state): State<Arc<MetricsCache>>) -> impl IntoResponse {
    (
        [(CONTENT_TYPE, PROMETHEUS_CONTENT_TYPE)],
        state.buffer.load().as_ref().clone(),
    )
}

pub fn setup(registry: &Registry, tasks: &mut TaskManager) -> Router {
    let cache = Arc::new(MetricsCache::new());
    let runner = Arc::new(MetricsRunner::new(cache.clone(), registry));
    tasks.add_interval("metrics_runner", runner, Duration::from_secs(5));

    Router::new()
        .route("/metrics", get(handler))
        .layer(
            CompressionLayer::new()
                .gzip(true)
                .br(true)
                .zstd(true)
                .deflate(true),
        )
        .with_state(cache)
}
