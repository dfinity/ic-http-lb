use std::sync::Arc;

use anyhow::{Error, anyhow};
use axum::{
    Router,
    extract::{Path, Request, State},
    middleware::{Next, from_fn_with_state},
    response::{IntoResponse, Response},
    routing::get,
};
use derive_new::new;
use http::{StatusCode, header::AUTHORIZATION};

use crate::{backend::BackendManager, cli::Cli};

#[derive(Debug, new)]
pub struct ApiState {
    token: String,
    backend_manager: Arc<BackendManager>,
}

pub async fn auth_middleware(
    State(state): State<Arc<ApiState>>,
    request: Request,
    next: Next,
) -> Response {
    let Some(auth) = request.headers().get(AUTHORIZATION) else {
        return (StatusCode::UNAUTHORIZED, "Authorization header not found").into_response();
    };

    let auth = auth.as_bytes();
    if !auth.starts_with(b"Bearer ") || auth.len() < 8 {
        return (StatusCode::UNAUTHORIZED, "Incorrect header format").into_response();
    }

    if &auth[7..] != state.token.as_bytes() {
        return (StatusCode::UNAUTHORIZED, "Incorrect bearer token").into_response();
    }

    next.run(request).await
}

pub async fn health_handler(State(state): State<Arc<ApiState>>) -> impl IntoResponse {
    if !state.backend_manager.get_healthy_nodes().is_empty() {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

pub async fn backend_handler(
    State(state): State<Arc<ApiState>>,
    Path((backend, action)): Path<(String, String)>,
) -> Result<impl IntoResponse, impl IntoResponse> {
    match action.as_str() {
        "enable" | "disable" => state
            .backend_manager
            .set_backend_state(backend, action == "enable")
            .await
            .map_err(|e| (StatusCode::BAD_REQUEST, format!("Error: {e:#}")))?,

        _ => return Err((StatusCode::BAD_REQUEST, format!("Unknown action: {action}"))),
    };

    Ok((StatusCode::OK, "OK".to_string()))
}

pub async fn config_handler(
    State(state): State<Arc<ApiState>>,
    Path(action): Path<String>,
) -> Result<impl IntoResponse, impl IntoResponse> {
    match action.as_str() {
        "reload" => state
            .backend_manager
            .load_config()
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Error: {e:#}")))?,

        "dump" => {
            let cfg = state.backend_manager.get_config().await;
            let cfg = serde_json::to_string_pretty(&cfg)
                .unwrap_or_else(|e| format!("unable to encode to JSON: {e:#}"));

            return Ok((StatusCode::OK, cfg));
        }

        _ => return Err((StatusCode::BAD_REQUEST, format!("Unknown action: {action}"))),
    };

    Ok((StatusCode::OK, "Ok".to_string()))
}

pub fn setup_api_axum_router(
    cli: &Cli,
    backend_manager: Arc<BackendManager>,
) -> Result<Router, Error> {
    let state = Arc::new(ApiState::new(
        cli.api
            .api_token
            .clone()
            .ok_or_else(|| anyhow!("API token not specified"))?,
        backend_manager,
    ));

    let auth = from_fn_with_state(state.clone(), auth_middleware);
    Ok(Router::new()
        .route("/health", get(health_handler))
        .route(
            "/backend/{backend}/{action}",
            get(backend_handler).layer(auth.clone()),
        )
        .route("/config/{action}", get(config_handler).layer(auth))
        .with_state(state))
}

#[cfg(test)]
mod test {
    use std::path::PathBuf;

    use arc_swap::ArcSwapOption;
    use axum::body::Body;
    use clap::Parser;
    use http::{Request, Uri};
    use ic_bn_lib::{
        http::{HyperClient, client::Options, dns::Resolver},
        hval,
    };
    use tower::ServiceExt;

    use super::*;

    #[tokio::test]
    async fn test_api_auth() {
        let args: Vec<&str> = vec!["", "--api-token", "deadbeef", "--backends-config", "foo"];
        let cli = Cli::parse_from(args);

        let client = Arc::new(HyperClient::new(Options::default(), Resolver::default()).unwrap());
        let bm = BackendManager::new(client, PathBuf::new(), Arc::new(ArcSwapOption::empty()));

        let router = setup_api_axum_router(&cli, Arc::new(bm)).unwrap();

        // Bad header
        let mut req = Request::new(Body::empty());
        req.headers_mut().insert(AUTHORIZATION, hval!("beef"));

        let resp = router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        let mut req = Request::new(Body::empty());
        req.headers_mut().insert(AUTHORIZATION, hval!("Bearer "));

        let resp = router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // Bad token
        let mut req = Request::new(Body::empty());
        req.headers_mut()
            .insert(AUTHORIZATION, hval!("Bearer foobar"));

        let resp = router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // Good token
        let mut req = Request::new(Body::empty());
        *req.uri_mut() = Uri::from_static("http://foo/config/dump");
        req.headers_mut()
            .insert(AUTHORIZATION, hval!("Bearer deadbeef"));

        let resp = router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
