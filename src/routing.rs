use std::sync::Arc;

use anyhow::Error;
use arc_swap::ArcSwapOption;
use axum::{
    Extension, Router,
    extract::{Request, State},
    handler::Handler,
    response::{IntoResponse, Response},
};
use axum_extra::extract::Host;
use bytes::Bytes;
use derive_new::new;
use http::{HeaderValue, StatusCode};
use ic_bn_lib::http::{
    ConnInfo, extract_host,
    headers::{X_FORWARDED_HOST, X_REAL_IP},
};
use tower::ServiceExt;

use crate::{backend::LBBackendRouter, cli::Cli};

#[derive(Debug, new)]
pub struct HandlerState {
    backend_router: Arc<ArcSwapOption<LBBackendRouter>>,
}

pub async fn handler(
    State(state): State<Arc<HandlerState>>,
    Extension(conn_info): Extension<Arc<ConnInfo>>,
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

    request.headers_mut().insert(
        X_REAL_IP,
        HeaderValue::from_maybe_shared(Bytes::from(conn_info.remote_addr.ip().to_string()))
            .unwrap(),
    );

    let resp = backend_router.execute(request).await;
    match resp {
        Err(e) => (StatusCode::SERVICE_UNAVAILABLE, format!("Error: {e:#}")).into_response(),
        Ok(v) => v,
    }
}

pub fn setup_axum_router(
    cli: &Cli,
    router_api: Option<Router>,
    backend_router: Arc<ArcSwapOption<LBBackendRouter>>,
) -> Result<Router, Error> {
    let state = Arc::new(HandlerState::new(backend_router));

    let state_clone = state.clone();
    let api_hostname = cli.api.api_hostname.clone().map(|x| x.to_string());

    Ok(Router::new()
        .fallback(|Host(host): Host, request: Request| async move {
            // See if we have API enabled
            if let Some(v) = router_api {
                // Check if the request's host matches API hostname
                if api_hostname.zip(extract_host(&host)).map(|(a, b)| a == b) == Some(true) {
                    return v.oneshot(request).await;
                }
            }

            Ok(handler.call(request, state_clone).await)
        })
        .with_state(state))
}
