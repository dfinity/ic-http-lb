use std::sync::Arc;

use anyhow::{Error, anyhow};
use axum::{
    Router,
    extract::{Path, Request, State},
    middleware::{Next, from_fn_with_state},
    response::{IntoResponse, Response},
    routing::{get, put},
};
use bytes::Bytes;
use derive_new::new;
use http::{StatusCode, header::AUTHORIZATION};
use tracing::warn;
#[cfg(target_os = "linux")]
use {anyhow::Context as _, axum::routing::post, ic_bn_lib::http::middleware::rate_limiter};

use crate::backend::{BackendManager, Config};

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
        "enable" | "disable" => {
            warn!("API request: {action} backend {backend}");

            state
                .backend_manager
                .set_backend_state(backend, action == "enable")
                .await
                .map_err(|e| (StatusCode::BAD_REQUEST, format!("Error: {e:#}")))?
        }

        _ => return Err((StatusCode::BAD_REQUEST, format!("Unknown action: {action}"))),
    };

    Ok((StatusCode::OK, "Ok\n".to_string()))
}

pub async fn config_reload(
    State(state): State<Arc<ApiState>>,
) -> Result<impl IntoResponse, impl IntoResponse> {
    warn!("API request: config reload");

    state
        .backend_manager
        .load_config()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Error: {e:#}")))?;

    // Oh Rust type inference...
    Ok::<(StatusCode, String), (StatusCode, String)>((StatusCode::OK, "Ok\n".to_string()))
}

pub async fn config_get(
    State(state): State<Arc<ApiState>>,
) -> Result<impl IntoResponse, impl IntoResponse> {
    warn!("API request: config get");

    let cfg = state.backend_manager.get_config().await;
    let cfg = serde_json::to_string_pretty(&cfg).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("unable to encode to JSON: {e:#}"),
        )
    })?;

    // Oh Rust type inference...
    Ok::<(StatusCode, String), (StatusCode, String)>((StatusCode::OK, cfg))
}

pub async fn config_put(
    State(state): State<Arc<ApiState>>,
    body: Bytes,
) -> Result<impl IntoResponse, impl IntoResponse> {
    warn!("API request: config put");

    let Ok(cfg): Result<Config, _> =
        serde_json::from_slice(&body).or_else(|_| serde_yaml_ng::from_slice(&body))
    else {
        return Err((
            StatusCode::BAD_REQUEST,
            "Unable to parse config as JSON or YAML".to_string(),
        ));
    };

    state.backend_manager.set_config(cfg).await.map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("Unable to apply new config: {e:#}"),
        )
    })?;

    state.backend_manager.persist_config().await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Unable to persist new config: {e:#}"),
        )
    })?;

    Ok((StatusCode::OK, "Ok\n"))
}

pub fn setup_api_axum_router(
    #[cfg(target_os = "linux")] enable_sev_snp: bool,
    api_token: Option<String>,
    backend_manager: Arc<BackendManager>,
) -> Result<Router, Error> {
    let state = Arc::new(ApiState::new(
        api_token.ok_or_else(|| anyhow!("API token not specified"))?,
        backend_manager,
    ));

    let auth = from_fn_with_state(state.clone(), auth_middleware);

    #[allow(unused_mut)]
    let mut router = Router::new()
        .route("/health", get(health_handler))
        .route(
            "/backend/{backend}/{action}",
            get(backend_handler).layer(auth.clone()),
        )
        .route("/config/reload", get(config_reload).layer(auth.clone()))
        .route("/config/get", get(config_get).layer(auth.clone()))
        .route("/config/put", put(config_put).layer(auth))
        .with_state(state);

    #[cfg(target_os = "linux")]
    if enable_sev_snp {
        router = router.route(
            "/sev-snp/report",
            post(ic_bn_lib::utils::sev_snp::handler)
                .with_state(
                    ic_bn_lib::utils::sev_snp::SevSnpState::new()
                        .context("unable to init SEV-SNP")?,
                )
                .layer(rate_limiter::layer_global(
                    1,
                    2,
                    (
                        StatusCode::TOO_MANY_REQUESTS,
                        "Too many requests, try again later",
                    ),
                )?),
        );
    }

    Ok(router)
}

#[cfg(test)]
mod test {
    use std::{path::PathBuf, str::FromStr, time::Duration};

    use axum::body::Body;
    use http::{HeaderValue, Method, Request, Uri};
    use ic_bn_lib::{http::HyperClient, hval};
    use prometheus::Registry;
    use tokio::fs;
    use tower::ServiceExt;

    use super::*;

    #[tokio::test]
    async fn test_api_auth() {
        let client = Arc::new(HyperClient::default());
        let bm = BackendManager::new(
            client,
            PathBuf::new(),
            Duration::from_secs(1),
            Duration::from_secs(3),
            &Registry::new(),
        );

        let token = "deadbeef".to_string();
        let router = setup_api_axum_router(Some(token.clone()), Arc::new(bm)).unwrap();

        // Bad header
        let mut req = Request::builder()
            .uri("/config/get")
            .body(Body::empty())
            .unwrap();
        req.headers_mut().insert(AUTHORIZATION, hval!("beef"));

        let resp = router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        let mut req: Request<Body> = Request::builder()
            .uri("/config/get")
            .body(Body::empty())
            .unwrap();
        req.headers_mut().insert(AUTHORIZATION, hval!("Bearer "));

        let resp = router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // Bad token
        let mut req = Request::builder()
            .uri("/config/get")
            .body(Body::empty())
            .unwrap();
        req.headers_mut()
            .insert(AUTHORIZATION, hval!("Bearer foobar"));

        let resp = router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // Good token
        let mut req = Request::builder()
            .uri("/config/get")
            .body(Body::empty())
            .unwrap();
        *req.uri_mut() = Uri::from_static("http://foo/config/get");
        req.headers_mut().insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        );

        let resp = router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_config() {
        let client = Arc::new(HyperClient::default());
        let bm = BackendManager::new(
            client,
            PathBuf::from_str("testconfig.yaml").unwrap(),
            Duration::from_secs(1),
            Duration::from_secs(3),
            &Registry::new(),
        );

        let token = "deadbeef".to_string();
        let router = setup_api_axum_router(Some(token.clone()), Arc::new(bm)).unwrap();

        // Upload config
        let config = include_bytes!("../config.yaml").as_slice();
        let body = Body::from(config);

        let mut req = Request::builder()
            .uri("/config/put")
            .method(Method::PUT)
            .body(body)
            .unwrap();
        *req.uri_mut() = Uri::from_static("http://foo/config/put");
        req.headers_mut().insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        );

        let resp = router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        fs::remove_file("testconfig.yaml").await.unwrap();
    }
}
