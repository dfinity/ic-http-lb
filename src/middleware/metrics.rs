use std::sync::Arc;

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
    http::{ConnInfo, extract_authority, headers::X_REAL_IP, http_method, http_version},
    vector::client::Vector,
};
use serde_json::json;
use tracing::info;

use crate::{backend::Backend, middleware::request_id::RequestId};

#[derive(new)]
pub struct MetricsState {
    vector: Option<Arc<Vector>>,
    log_requests: bool,
}

pub async fn middleware(
    State(state): State<Arc<MetricsState>>,
    Extension(conn_info): Extension<Arc<ConnInfo>>,
    mut request: Request,
    next: Next,
) -> Response {
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
    let remote_addr = conn_info.remote_addr.ip().to_string();

    request.headers_mut().insert(
        X_REAL_IP,
        HeaderValue::from_maybe_shared(Bytes::from(remote_addr.clone())).unwrap(),
    );

    let mut response = next.run(request).await;
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

    if state.log_requests {
        info!(
            request_id,
            http_version,
            authority,
            method,
            path,
            query,
            remote_addr,
            status = status.as_str(),
            backend,
            request_size,
            response_size,
        )
    }

    if let Some(v) = &state.vector {
        let event = json! ({
            "request_id": request_id,
            "http_version": http_version,
            "authority": authority,
            "method": method,
            "path": path,
            "query": query,
            "status": status.as_str(),
            "backend": backend,
            "remote_addr": remote_addr,
            "request_size": request_size,
            "response_size": response_size,
        });

        v.send(event);
    }

    response
}
