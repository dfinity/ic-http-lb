#![allow(clippy::enum_variant_names)]

use std::{sync::Arc, time::Duration};

use anyhow::{Context, Error, anyhow};
use axum::{
    Router,
    body::{Body, HttpBody as _},
    extract::{Request, State},
    handler::Handler,
    middleware::from_fn_with_state,
    response::{IntoResponse, Response},
};
use axum_extra::middleware::option_layer;
use bytes::Bytes;
use derive_new::new;
use http::{HeaderValue, StatusCode, request::Parts};
use http_body_util::{BodyExt, Full, Limited};
use ic_bn_lib::{
    http::{
        body::buffer_body,
        extract_authority, extract_host,
        headers::X_FORWARDED_HOST,
        middleware::{request_meta, waf::WafLayer},
    },
    lb::backend_router::Error as BackendRouterError,
    vector::client::Vector,
};
use prometheus::Registry;
use strum::IntoStaticStr;
use tokio::time::{sleep, timeout};
use tower::{ServiceBuilder, ServiceExt};
use tracing::info;

use crate::{
    backend::{BackendManager, REQUEST_CONTEXT},
    cli::Cli,
    middleware::{
        self,
        metrics::{Metrics, MetricsState},
    },
};

#[derive(Clone, Debug, thiserror::Error, IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum ErrorCause {
    #[error("Backend request error: {0}")]
    BackendRequestError(String),
    #[error("Backend body error: {0}")]
    BackendBodyError(String),
    #[error("Backend timeout")]
    BackendTimeout,
    #[error("No authority")]
    NoAuthority,
    #[error("Service not ready")]
    ServiceNotReady,
    #[error("No healthy backends available")]
    NoHealthyBackends,
    #[error("Timed out buffering the request body")]
    RequestBodyBufferTimeout,
}

impl IntoResponse for ErrorCause {
    fn into_response(self) -> Response {
        let status = match self {
            Self::BackendRequestError(_) | Self::BackendBodyError(_) => StatusCode::BAD_GATEWAY,
            Self::BackendTimeout => StatusCode::GATEWAY_TIMEOUT,
            Self::NoAuthority => StatusCode::BAD_REQUEST,
            Self::ServiceNotReady | Self::NoHealthyBackends => StatusCode::SERVICE_UNAVAILABLE,
            Self::RequestBodyBufferTimeout => StatusCode::REQUEST_TIMEOUT,
        };

        let mut response = (status, self.to_string()).into_response();
        response.extensions_mut().insert(self);
        response
    }
}

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
        .is_some_and(|x| x <= state.response_body_size_limit as u64);

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
        .map_or_else(|| "unknown".into(), |x| x.name.clone());

    let body = Limited::new(body, state.response_body_size_limit);
    let Ok(body) = timeout(state.response_body_timeout, body.collect()).await else {
        info!("Timed out reading response body from backend '{backend}'");
        return ErrorCause::BackendTimeout.into_response();
    };

    let body = match body {
        Ok(v) => Body::from(v.to_bytes()),
        Err(e) => {
            let resp = ErrorCause::BackendBodyError(e.to_string()).into_response();
            info!(
                "Unable to read response body from backend '{backend}': {:#}",
                anyhow!(e)
            );
            return resp;
        }
    };

    // Store the flag for the metrics
    let _ = REQUEST_CONTEXT.try_with(|x| {
        x.borrow_mut().response_body_buffered = true;
    });

    Response::from_parts(parts, body)
}

pub async fn handler(State(state): State<Arc<HandlerState>>, mut request: Request) -> Response {
    let Some(host) = extract_authority(&request).map(|x| x.to_string()) else {
        return ErrorCause::NoAuthority.into_response();
    };

    let Some(backend_router) = state.backend_manager.get_backend_router() else {
        return ErrorCause::ServiceNotReady.into_response();
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
        .is_some_and(|x| x <= state.request_body_size_limit as u64);

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
                info!("Unable to execute the request: No healthy backends available");
                return ErrorCause::NoHealthyBackends.into_response();
            }
            Err(BackendRouterError::Inner(e)) => {
                info!("Unable to execute the request: {e:#}");
                return ErrorCause::BackendRequestError(e.to_string()).into_response();
            }
            Ok(v) => v,
        };

        return buffer_response(&state, response).await;
    }

    // Buffer the request body
    let Ok((parts, body)) = buffer_request(&state, request).await else {
        return ErrorCause::RequestBodyBufferTimeout.into_response();
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

        let error_response = match backend_router.execute(request).await {
            Err(BackendRouterError::NoHealthyNodes) => {
                info!("Unable to execute the request: No healthy backends available");
                sleep(state.retry_interval_no_healthy_nodes).await;
                ErrorCause::NoHealthyBackends.into_response()
            }
            Err(BackendRouterError::Inner(e)) => {
                info!("Unable to execute the request: {e:#}");
                sleep(delay).await;
                delay *= 2;
                ErrorCause::BackendRequestError(e.to_string()).into_response()
            }
            Ok(v) => {
                break v;
            }
        };

        retries -= 1;
        if retries == 0 {
            return error_response;
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
) -> anyhow::Result<Router> {
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
        .layer(from_fn_with_state(
            Arc::new(
                request_meta::RequestMetaState::new_with_geoip(
                    cli.network.network_trust_x_real_ip_from.clone(),
                    cli.network.network_trust_x_request_id_from.clone(),
                    cli.misc.geoip_db.clone(),
                )
                .context("unable to build RequestMeta state")?,
            ),
            request_meta::middleware,
        ))
        .layer(from_fn_with_state(
            metrics_state,
            middleware::metrics::middleware,
        ))
        .layer(option_layer(waf_layer));

    Ok(Router::new()
        .fallback(|request: Request| async move {
            let Some(host) = extract_authority(&request) else {
                return Ok(ErrorCause::NoAuthority.into_response());
            };

            // See if we have API enabled
            if let Some(v) = router_api {
                // Check if the request's host matches API hostname
                if api_hostname
                    .zip(extract_host(host))
                    .is_some_and(|(a, b)| a == b)
                {
                    return v.oneshot(request).await;
                }
            }

            Ok(handler.call(request, state).await)
        })
        .layer(middlewares))
}

#[cfg(test)]
mod test {
    use std::path::PathBuf;

    use http::header::HOST;
    use ic_bn_lib::http::HyperClient;

    use super::*;

    fn new_backend_manager() -> Arc<BackendManager> {
        Arc::new(BackendManager::new(
            Arc::new(HyperClient::default()),
            PathBuf::new(),
            Duration::from_secs(1),
            Duration::from_secs(1),
            &Registry::new(),
        ))
    }

    fn new_state(response_body_buffer: bool, response_body_size_limit: usize) -> HandlerState {
        HandlerState::new(
            new_backend_manager(),
            false,
            1024,
            Duration::from_secs(1),
            response_body_buffer,
            response_body_size_limit,
            Duration::from_secs(1),
            1,
            Duration::from_millis(1),
            Duration::from_millis(1),
        )
    }

    #[tokio::test]
    async fn test_buffer_response_disabled_passes_through() {
        let state = new_state(false, 1024);
        let response = Response::new(Body::from(Bytes::from_static(b"hello")));

        let response = buffer_response(&state, response).await;
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"hello");
    }

    #[tokio::test]
    async fn test_buffer_response_buffers_small_body() {
        let state = new_state(true, 1024);
        let response = Response::new(Body::from(Bytes::from_static(b"hello")));

        let response = buffer_response(&state, response).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"hello");
    }

    #[tokio::test]
    async fn test_buffer_response_skips_body_over_limit() {
        // Body size hint (5 bytes) exceeds the configured limit (2), so it's left untouched
        let state = new_state(true, 2);
        let response = Response::new(Body::from(Bytes::from_static(b"hello")));

        let response = buffer_response(&state, response).await;
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"hello");
    }

    #[tokio::test]
    async fn test_buffer_request_ok() {
        let state = new_state(false, 1024);
        let request = Request::new(Body::from(Bytes::from_static(b"payload")));

        let (_, body) = buffer_request(&state, request).await.unwrap();
        let bytes = body.collect().await.unwrap().to_bytes();
        assert_eq!(&bytes[..], b"payload");
    }

    #[tokio::test]
    async fn test_buffer_request_too_big() {
        let mut state = new_state(false, 1024);
        state.request_body_size_limit = 3;
        let request = Request::new(Body::from(Bytes::from_static(b"payload")));

        assert!(buffer_request(&state, request).await.is_err());
    }

    #[tokio::test]
    async fn test_handler_missing_authority_returns_bad_request() {
        let state = Arc::new(new_state(false, 1024));
        let request = Request::builder().uri("/").body(Body::empty()).unwrap();

        let response = handler(State(state), request).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_handler_backend_not_ready_returns_service_unavailable() {
        let state = Arc::new(new_state(false, 1024));
        let request = Request::builder()
            .uri("/")
            .header(HOST, "example.com")
            .body(Body::empty())
            .unwrap();

        let response = handler(State(state), request).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
