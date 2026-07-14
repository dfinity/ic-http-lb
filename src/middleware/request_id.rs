use std::ops::Deref;

use axum::{extract::Request, middleware::Next, response::Response};
use bytes::Bytes;
use http::header::HeaderValue;
use ic_bn_lib::{http::headers::X_REQUEST_ID, uuid::Uuid};

#[derive(Clone, Copy, Default)]
pub struct RequestId(pub Uuid);

impl Deref for RequestId {
    type Target = Uuid;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

// Generate & insert request UUID into extensions and headers
pub async fn middleware(mut request: Request, next: Next) -> Response {
    let request_id = RequestId(Uuid::now_v7());
    let hdr = request_id.to_string();
    // UUID is guaranteed to fit into header value
    let hdr = HeaderValue::from_maybe_shared(Bytes::from(hdr)).unwrap();

    request.extensions_mut().insert(request_id);
    request.headers_mut().insert(X_REQUEST_ID, hdr.clone());

    let mut response = next.run(request).await;
    response.headers_mut().insert(X_REQUEST_ID, hdr);

    response
}

#[cfg(test)]
mod test {
    use axum::{Router, body::Body, extract::Extension, middleware::from_fn, routing::get};
    use http::StatusCode;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::*;

    async fn echo_request_id(Extension(id): Extension<RequestId>) -> String {
        id.to_string()
    }

    #[tokio::test]
    async fn test_request_id_middleware_sets_matching_header_and_extension() {
        let app = Router::new()
            .route("/", get(echo_request_id))
            .layer(from_fn(middleware));

        let request = Request::builder().uri("/").body(Body::empty()).unwrap();
        let response = app.oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let header_id = response
            .headers()
            .get(X_REQUEST_ID)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body_id = String::from_utf8(body.to_vec()).unwrap();

        assert_eq!(header_id, body_id);
    }
}
