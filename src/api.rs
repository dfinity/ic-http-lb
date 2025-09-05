use std::{str::FromStr, sync::Arc};

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
use ic_bn_lib::http::middleware::waf::{self, WafLayer};
use tracing::{Level, warn};
use tracing_core::LevelFilter;
use tracing_subscriber::{Registry, reload::Handle};
#[cfg(all(target_os = "linux", feature = "sev-snp"))]
use {anyhow::Context as _, axum::routing::post, ic_bn_lib::http::middleware::rate_limiter};

use crate::{
    backend::{BackendManager, Config},
    cli::Cli,
};

#[derive(Debug, new)]
pub struct ApiState {
    token: String,
    backend_manager: Arc<BackendManager>,
    log_handle: Arc<Handle<LevelFilter, Registry>>,
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

pub async fn log_handler(
    State(state): State<Arc<ApiState>>,
    Path(log_level): Path<String>,
) -> Response {
    let Ok(log_level) = Level::from_str(&log_level) else {
        return (
            StatusCode::BAD_REQUEST,
            format!("Unable to parse '{log_level}' as log level"),
        )
            .into_response();
    };
    let level_filter = LevelFilter::from_level(log_level);
    let _ = state.log_handle.modify(|f| *f = level_filter);

    "Ok\n".into_response()
}

pub async fn config_reload(State(state): State<Arc<ApiState>>) -> Response {
    warn!("API request: config reload");

    if let Err(e) = state.backend_manager.load_config().await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Error reloading config: {e:#}"),
        )
            .into_response();
    };

    "Ok\n".into_response()
}

pub async fn config_get(State(state): State<Arc<ApiState>>) -> Response {
    warn!("API request: config get");

    let cfg = state.backend_manager.get_config().await;
    let Ok(cfg) = serde_json::to_string_pretty(&cfg) else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Unable to encode to JSON",
        )
            .into_response();
    };

    cfg.into_response()
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
    cli: &Cli,
    backend_manager: Arc<BackendManager>,
    log_handle: Handle<LevelFilter, Registry>,
    waf_layer: Option<WafLayer>,
) -> Result<Router, Error> {
    let state = Arc::new(ApiState::new(
        cli.api
            .api_token
            .clone()
            .ok_or_else(|| anyhow!("API token not specified"))?,
        backend_manager,
        Arc::new(log_handle),
    ));

    let auth = from_fn_with_state(state.clone(), auth_middleware);

    let mut router = Router::new()
        .route("/health", get(health_handler))
        .route("/log/{log_level}", get(log_handler).layer(auth.clone()))
        .route(
            "/backend/{backend}/{action}",
            get(backend_handler).layer(auth.clone()),
        )
        .route("/config/reload", get(config_reload).layer(auth.clone()))
        .route("/config/get", get(config_get).layer(auth.clone()))
        .route("/config/put", put(config_put).layer(auth.clone()));

    if let Some(v) = waf_layer {
        router = router.nest("/waf", waf::create_router(v).layer(auth))
    }

    #[allow(unused_mut)]
    let mut router = router.with_state(state);

    #[cfg(all(target_os = "linux", feature = "sev-snp"))]
    if cli.sev_snp.sev_snp_enable {
        router = router.route(
            "/sev-snp/report",
            post(ic_bn_lib::utils::sev_snp::handler)
                .with_state(
                    ic_bn_lib::utils::sev_snp::SevSnpState::new(
                        cli.sev_snp.sev_snp_cache_ttl,
                        cli.sev_snp.sev_snp_cache_size,
                    )
                    .context("unable to init SEV-SNP")?,
                )
                .layer(rate_limiter::layer_global(
                    50,
                    100,
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
    use clap::Parser;
    use http::{HeaderValue, Method, Request, Uri};
    use ic_bn_lib::{http::HyperClient, hval};
    use prometheus::Registry;
    use tokio::fs;
    use tower::ServiceExt;
    use tracing_subscriber::reload;

    use super::*;

    #[tokio::test]
    async fn test_api_auth() {
        let args: Vec<&str> = vec!["", "--config-path", "foo", "--api-token", "deadbeef"];
        let cli = Cli::parse_from(args);

        let client = Arc::new(HyperClient::default());
        let bm = BackendManager::new(
            client,
            PathBuf::new(),
            Duration::from_secs(1),
            Duration::from_secs(3),
            &Registry::new(),
        );

        let (_, reload_handle) = reload::Layer::new(LevelFilter::WARN);
        let router = setup_api_axum_router(&cli, Arc::new(bm), reload_handle, None).unwrap();

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
            HeaderValue::from_str(&format!("Bearer {}", cli.api.api_token.unwrap())).unwrap(),
        );

        let resp = router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_config() {
        let args: Vec<&str> = vec!["", "--config-path", "foo", "--api-token", "deadbeef"];
        let cli = Cli::parse_from(args);

        let client = Arc::new(HyperClient::default());
        let bm = BackendManager::new(
            client,
            PathBuf::from_str("testconfig.yaml").unwrap(),
            Duration::from_secs(1),
            Duration::from_secs(3),
            &Registry::new(),
        );

        let (_, reload_handle) = reload::Layer::new(LevelFilter::WARN);
        let router = setup_api_axum_router(&cli, Arc::new(bm), reload_handle, None).unwrap();

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
            HeaderValue::from_str(&format!("Bearer {}", cli.api.api_token.unwrap())).unwrap(),
        );

        let resp = router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        fs::remove_file("testconfig.yaml").await.unwrap();
    }
}
