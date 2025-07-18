use std::sync::Arc;

use anyhow::Error;
use arc_swap::ArcSwapOption;
use axum::{
    Router,
    extract::{Request, State},
    handler::Handler,
    middleware::{from_fn, from_fn_with_state},
    response::{IntoResponse, Response},
    routing::get,
};
use axum_extra::extract::Host;
use bytes::Bytes;
use derive_new::new;
use http::{HeaderValue, StatusCode};
use ic_bn_lib::{
    http::{extract_host, headers::X_FORWARDED_HOST},
    vector::client::Vector,
};
use prometheus::Registry;
use tower::ServiceExt;

use crate::{
    backend::LBBackendRouter,
    cli::Cli,
    middleware::{
        self,
        metrics::{Metrics, MetricsState},
    },
};

#[derive(Debug, new)]
pub struct HandlerState {
    backend_router: Arc<ArcSwapOption<LBBackendRouter>>,
}

pub async fn handler(
    State(state): State<Arc<HandlerState>>,
    Host(host): Host,
    mut request: Request,
) -> Response {
    let Some(backend_router) = state.backend_router.load_full() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "Service is not yet ready").into_response();
    };

    request.headers_mut().insert(
        X_FORWARDED_HOST,
        HeaderValue::from_maybe_shared(Bytes::from(host)).unwrap(),
    );

    let resp = backend_router.execute(request).await;
    match resp {
        Err(e) => (StatusCode::SERVICE_UNAVAILABLE, format!("Error: {e:#}")).into_response(),
        Ok(v) => v,
    }
}

/// Creates top-level Axum Router
pub fn setup_axum_router(
    cli: &Cli,
    router_api: Option<Router>,
    backend_router: Arc<ArcSwapOption<LBBackendRouter>>,
    vector: Option<Arc<Vector>>,
    registry: &Registry,
) -> Result<Router, Error> {
    let state = Arc::new(HandlerState::new(backend_router));
    let api_hostname = cli.api.api_hostname.clone().map(|x| x.to_string());
    let metrics = Metrics::new(registry);
    let metrics_state = Arc::new(MetricsState::new(vector, metrics, cli.log.log_requests));

    Ok(Router::new()
        .route("/foobar_test_route", get(async || "foobar"))
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
