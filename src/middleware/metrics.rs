use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    Extension,
    body::HttpBody,
    extract::{Request, State},
    middleware::Next,
    response::Response,
};
use bytes::Bytes;
use derive_new::new;
use http::HeaderValue;
use ic_bn_lib::{
    http::{
        ConnInfo, extract_authority, headers::X_REAL_IP, http_method, http_version, server::TlsInfo,
    },
    vector::client::Vector,
};
use prometheus::{
    HistogramVec, IntCounterVec, Registry, register_histogram_vec_with_registry,
    register_int_counter_vec_with_registry,
};
use serde_json::json;
use tracing::info;

use crate::{
    backend::Backend,
    core::{ENV, HOSTNAME},
    middleware::request_id::RequestId,
    routing::Retries,
};

pub const HTTP_DURATION_BUCKETS: &[f64] = &[0.05, 0.2, 1.0, 2.0];

#[derive(Clone)]
pub struct Metrics {
    pub requests: IntCounterVec,
    pub duration: HistogramVec,
}

impl Metrics {
    pub fn new(registry: &Registry) -> Self {
        const LABELS_HTTP: &[&str] = &["tls", "method", "http", "status", "backend", "retried"];

        Self {
            requests: register_int_counter_vec_with_registry!(
                format!("http_requests"),
                format!("Counts occurrences of requests"),
                LABELS_HTTP,
                registry
            )
            .unwrap(),

            duration: register_histogram_vec_with_registry!(
                format!("http_requests_duration_sec"),
                format!("Records the duration of request processing in seconds"),
                LABELS_HTTP,
                HTTP_DURATION_BUCKETS.to_vec(),
                registry
            )
            .unwrap(),
        }
    }
}

#[derive(new)]
pub struct MetricsState {
    vector: Option<Arc<Vector>>,
    metrics: Metrics,
    log_requests: bool,
}

pub async fn middleware(
    State(state): State<Arc<MetricsState>>,
    Extension(conn_info): Extension<Arc<ConnInfo>>,
    mut request: Request,
    next: Next,
) -> Response {
    let tls_info = request.extensions().get::<Arc<TlsInfo>>().cloned();
    let method = http_method(request.method());
    let authority = extract_authority(&request).unwrap_or_default().to_string();
    let http_version = http_version(request.version());
    let path = request.uri().path().to_string();
    let query = request.uri().query().unwrap_or("").to_string();
    let request_size = request
        .body()
        .size_hint()
        .exact()
        .map(|x| x as i64)
        .unwrap_or(-1);
    let remote_addr = conn_info.remote_addr.ip().to_canonical().to_string();
    let timestamp = time::OffsetDateTime::now_utc();

    request.headers_mut().insert(
        X_REAL_IP,
        HeaderValue::from_maybe_shared(Bytes::from(remote_addr.clone())).unwrap(),
    );

    // Execute the request
    let start = Instant::now();
    let mut response = next.run(request).await;
    let duration = start.elapsed().as_secs_f64();

    let backend = response
        .extensions_mut()
        .remove::<Arc<Backend>>()
        .map(|x| x.name.clone())
        .unwrap_or_default();
    let response_size = response
        .body()
        .size_hint()
        .exact()
        .map(|x| x as i64)
        .unwrap_or(-1);
    let request_id = response
        .extensions_mut()
        .remove::<RequestId>()
        .map(|x| x.to_string())
        .unwrap_or_default();
    let status = response.status();
    let conn_id = conn_info.id.to_string();
    let retries = response
        .extensions_mut()
        .remove::<Retries>()
        .map(|x| x.0)
        .unwrap_or(0);

    let (tls_version, tls_cipher, tls_handshake) =
        tls_info.as_ref().map_or(("", "", Duration::ZERO), |x| {
            (
                x.protocol.as_str().unwrap(),
                x.cipher.as_str().unwrap(),
                x.handshake_dur,
            )
        });

    let labels = &[
        tls_version,
        method,
        http_version,
        status.as_str(),
        backend.as_str(),
        if retries > 0 { "yes" } else { "no" },
    ];

    state.metrics.requests.with_label_values(labels).inc();
    state
        .metrics
        .duration
        .with_label_values(labels)
        .observe(duration);

    if state.log_requests {
        info!(
            request_id,
            conn_id,
            tls_version,
            tls_cipher,
            tls_handshake = tls_handshake.as_secs_f64(),
            http_version,
            authority,
            method,
            path,
            query,
            remote_addr,
            status = status.as_str(),
            duration,
            backend,
            request_size,
            response_size,
        )
    }

    if let Some(v) = &state.vector {
        let event = json! ({
            "env": ENV.get().unwrap(),
            "hostname": HOSTNAME.get().unwrap(),
            "timestamp": timestamp.unix_timestamp(),
            "conn_id": conn_id,
            "request_id": request_id,
            "tls_version": tls_version,
            "tls_cipher": tls_cipher,
            "tls_handshake": tls_handshake.as_secs_f64(),
            "http_version": http_version,
            "authority": authority,
            "method": method,
            "path": path,
            "query": query,
            "status": status.as_u16(),
            "duration": duration,
            "backend": backend,
            "remote_addr": remote_addr,
            "request_size": request_size,
            "response_size": response_size,
        });

        v.send(event);
    }

    response
}
