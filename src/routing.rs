use std::{sync::Arc, time::Duration};

use anyhow::Error;
use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    handler::Handler,
    middleware::{from_fn, from_fn_with_state},
    response::{IntoResponse, Response},
};
use axum_extra::extract::Host;
use bytes::Bytes;
use derive_new::new;
use http::{HeaderValue, StatusCode};
use http_body_util::Full;
use ic_bn_lib::{
    http::{body::buffer_body, extract_host, headers::X_FORWARDED_HOST},
    utils::backend_router::Error as BackendRouterError,
    vector::client::Vector,
};
use prometheus::Registry;
use tokio::time::sleep;
use tower::ServiceExt;

use crate::{
    backend::BackendManager,
    cli::Cli,
    middleware::{
        self,
        metrics::{Metrics, MetricsState},
    },
};

#[derive(Debug, new)]
pub struct HandlerState {
    backend_manager: Arc<BackendManager>,
    body_size_limit: usize,
    body_timeout: Duration,
    retry_attempts: u8,
    retry_interval: Duration,
    retry_interval_no_healthy_nodes: Duration,
}

pub async fn handler(
    State(state): State<Arc<HandlerState>>,
    Host(host): Host,
    mut request: Request,
) -> Response {
    let Some(backend_router) = state.backend_manager.get_backend_router() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "Service is not yet ready").into_response();
    };

    request.headers_mut().insert(
        X_FORWARDED_HOST,
        HeaderValue::from_maybe_shared(Bytes::from(host)).unwrap(),
    );

    // Don't buffer the body if no retries are planned
    if state.retry_attempts == 1 {
        return match backend_router.execute(request).await {
            Err(BackendRouterError::NoHealthyNodes) => {
                "No healthy HTTP gateways available".into_response()
            }
            Err(BackendRouterError::Inner(e)) => format!("Error: {e:#}").into_response(),
            Ok(v) => v,
        };
    }

    // Buffer the request body
    let (parts, body) = request.into_parts();
    let Ok(body) = buffer_body(body, state.body_size_limit, state.body_timeout).await else {
        return (StatusCode::SERVICE_UNAVAILABLE, "Unable to buffer body").into_response();
    };
    let body = Full::new(body);

    let mut retries = state.retry_attempts;
    let mut delay = state.retry_interval;

    loop {
        let body = Body::new(body.clone());
        let request = Request::from_parts(parts.clone(), body);

        let resp = backend_router.execute(request).await;
        let error = match resp {
            Err(BackendRouterError::NoHealthyNodes) => {
                sleep(state.retry_interval_no_healthy_nodes).await;
                "No healthy HTTP gateways available".into()
            }
            Err(BackendRouterError::Inner(e)) => {
                sleep(delay).await;
                delay *= 2;
                format!("Error: {e:#}")
            }
            Ok(v) => break v,
        };

        retries -= 1;
        if retries == 0 {
            break (StatusCode::SERVICE_UNAVAILABLE, error).into_response();
        }
    }
}

/// Creates top-level Axum Router
pub fn setup_axum_router(
    cli: &Cli,
    router_api: Option<Router>,
    backend_manager: Arc<BackendManager>,
    vector: Option<Arc<Vector>>,
    registry: &Registry,
) -> Result<Router, Error> {
    let state = Arc::new(HandlerState::new(
        backend_manager,
        cli.limits.limits_request_body_size,
        cli.limits.limits_request_body_timeout,
        cli.retry.retry_attempts,
        cli.retry.retry_interval,
        cli.health.health_check_interval.div_f64(4.0),
    ));

    let api_hostname = cli.api.api_hostname.clone().map(|x| x.to_string());
    let metrics = Metrics::new(registry);
    let metrics_state = Arc::new(MetricsState::new(vector, metrics, cli.log.log_requests));

    Ok(Router::new()
        .fallback(|Host(host): Host, request: Request| async move {
            // See if we have API enabled
            if let Some(v) = router_api {
                // Check if the request's host matches API hostname
                if api_hostname.zip(extract_host(&host)).map(|(a, b)| a == b) == Some(true) {
                    return v.oneshot(request).await;
                }
            }

            Ok(handler.call(request, state).await)
        })
        .layer(from_fn(middleware::request_id::middleware))
        .layer(from_fn_with_state(
            metrics_state,
            middleware::metrics::middleware,
        )))
}
