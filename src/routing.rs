use std::{sync::Arc, time::Duration};

use anyhow::{Error, anyhow};
use axum::{
    Router,
    body::{Body, HttpBody as _},
    extract::{Request, State},
    handler::Handler,
    middleware::{from_fn, from_fn_with_state},
    response::{IntoResponse, Response},
};
use axum_extra::{extract::Host, middleware::option_layer};
use bytes::Bytes;
use derive_new::new;
use http::{HeaderValue, StatusCode, request::Parts};
use http_body_util::{BodyExt, Full, Limited};
use ic_bn_lib::{
    http::{body::buffer_body, extract_host, headers::X_FORWARDED_HOST, middleware::waf::WafLayer},
    utils::backend_router::Error as BackendRouterError,
    vector::client::Vector,
};
use prometheus::Registry;
use tokio::time::{sleep, timeout};
use tower::{ServiceBuilder, ServiceExt};
use tracing::{info, warn};

use crate::{
    backend::{BackendManager, REQUEST_CONTEXT},
    cli::Cli,
    middleware::{
        self,
        metrics::{Metrics, MetricsState},
    },
};

#[derive(Clone, Debug)]
pub struct Retries(pub u8);

#[allow(clippy::too_many_arguments)]
#[derive(Debug, new)]
pub struct HandlerState {
    backend_manager: Arc<BackendManager>,
    request_body_buffer: bool,
    request_body_size_limit: usize,
    request_body_timeout: Duration,
    response_body_buffer: bool,
    response_body_size_limit: usize,
    response_body_timeout: Duration,
    retry_attempts: u8,
    retry_interval: Duration,
    retry_interval_no_healthy_nodes: Duration,
}

/// Buffers the request body
async fn buffer_request(
    state: &HandlerState,
    request: Request,
) -> Result<(Parts, Full<Bytes>), Error> {
    // Buffer the request body
    let (parts, body) = request.into_parts();
    let body = match buffer_body(
        body,
        state.request_body_size_limit,
        state.request_body_timeout,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => {
            let e = anyhow!(e);
            info!("Unable to buffer the request body: {e:#}");
            return Err(e);
        }
    };
    let body = Full::new(body);

    Ok((parts, body))
}

/// Buffers the response body
async fn buffer_response(state: &HandlerState, response: Response) -> Response {
    // Return the response as-is if no buffering was requested
    if !state.response_body_buffer {
        return response;
    }

    // Check if the response body size is known and it is small enough
    let body_bufferable = response
        .body()
        .size_hint()
        .exact()
        .map(|x| x <= state.response_body_size_limit as u64)
        == Some(true);

    // Return the response as-is if the body isn't bufferable
    if !body_bufferable {
        return response;
    }

    // Buffer the response
    let (parts, body) = response.into_parts();
    let backend = REQUEST_CONTEXT
        .try_with(|x| x.clone())
        .unwrap_or_default()
        .into_inner()
        .backend
        .map(|x| x.name.clone())
        .unwrap_or_else(|| "unknown".into());
    let body = Limited::new(body, state.response_body_size_limit);

    let Ok(body) = timeout(state.response_body_timeout, body.collect()).await else {
        info!("Timed out reading response body from backend '{backend}'");
        return (
            StatusCode::GATEWAY_TIMEOUT,
            "Timed out reading response body from the backend",
        )
            .into_response();
    };

    let body = match body {
        Ok(v) => Body::from(v.to_bytes()),
        Err(e) => {
            info!(
                "Unable to read response body from backend '{backend}': {:#}",
                anyhow!(e)
            );
            return (
                StatusCode::BAD_GATEWAY,
                "Error reading response body from the backend",
            )
                .into_response();
        }
    };

    // Store the flag for the metrics
    let _ = REQUEST_CONTEXT.try_with(|x| {
        x.borrow_mut().response_body_buffered = true;
    });

    Response::from_parts(parts, body)
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
        HeaderValue::from_maybe_shared(Bytes::from(host)).unwrap(), // Host is guaranteed to fit into HeaderValue
    );

    // Check if the request body size is known and it is small enough
    let request_body_bufferable = request
        .body()
        .size_hint()
        .exact()
        .map(|x| x <= state.request_body_size_limit as u64)
        == Some(true);

    // Buffer the request body only if:
    // - It is bufferable, and:
    //   * We want to do retries
    //     or
    //   * We were told to buffer it explicitly
    let request_should_buffer =
        request_body_bufferable && (state.retry_attempts > 1 || state.request_body_buffer);

    if !request_should_buffer {
        let response = match backend_router.execute(request).await {
            Err(BackendRouterError::NoHealthyNodes) => {
                "No healthy HTTP gateways available".into_response()
            }
            Err(BackendRouterError::Inner(e)) => {
                info!("Unable to execute the request: {:#}", anyhow!(e));
                (
                    StatusCode::BAD_GATEWAY,
                    "Unable to execute the request to the backend",
                )
                    .into_response()
            }
            Ok(v) => v,
        };

        return buffer_response(&state, response).await;
    }

    // Buffer the request body
    let Ok((parts, body)) = buffer_request(&state, request).await else {
        return (StatusCode::REQUEST_TIMEOUT, "Unable to buffer body").into_response();
    };

    // Store the flag for the metrics
    let _ = REQUEST_CONTEXT.try_with(|x| {
        x.borrow_mut().request_body_buffered = true;
    });

    let mut retries = state.retry_attempts;
    let mut delay = state.retry_interval;

    let mut response = loop {
        let body = Body::new(body.clone());
        let request = Request::from_parts(parts.clone(), body);

        let error = match backend_router.execute(request).await {
            Err(BackendRouterError::NoHealthyNodes) => {
                sleep(state.retry_interval_no_healthy_nodes).await;
                "No healthy HTTP gateways available".into()
            }
            Err(BackendRouterError::Inner(e)) => {
                let e = format!("Error: {:#}", anyhow!(e));
                warn!("{e}");
                sleep(delay).await;
                delay *= 2;
                e
            }
            Ok(v) => {
                break v;
            }
        };

        retries -= 1;
        if retries == 0 {
            return (StatusCode::SERVICE_UNAVAILABLE, error).into_response();
        }
    };

    response
        .extensions_mut()
        .insert(Retries(state.retry_attempts - retries));

    buffer_response(&state, response).await
}

/// Creates top-level Axum Router
pub fn setup_axum_router(
    cli: &Cli,
    router_api: Option<Router>,
    backend_manager: Arc<BackendManager>,
    vector: Option<Arc<Vector>>,
    registry: &Registry,
    waf_layer: Option<WafLayer>,
) -> Result<Router, Error> {
    let state = Arc::new(HandlerState::new(
        backend_manager,
        cli.network.network_request_body_buffer,
        cli.limits.limits_request_body_size,
        cli.limits.limits_request_body_timeout,
        cli.network.network_response_body_buffer,
        cli.limits.limits_response_body_size,
        cli.limits.limits_response_body_timeout,
        cli.retry.retry_attempts,
        cli.retry.retry_interval,
        cli.health.health_check_interval.div_f64(4.0),
    ));

    let api_hostname = cli.api.api_hostname.clone().map(|x| x.to_string());
    let metrics = Metrics::new(registry);
    let metrics_state = Arc::new(MetricsState::new(
        vector,
        metrics,
        cli.log.log_requests,
        cli.log.log_requests_long,
    ));

    let middlewares = ServiceBuilder::new()
        .layer(from_fn(middleware::request_id::middleware))
        .layer(from_fn_with_state(
            metrics_state,
            middleware::metrics::middleware,
        ))
        .layer(option_layer(waf_layer));

    let router = Router::new()
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
        .layer(middlewares);

    Ok(router)
}
