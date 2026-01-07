#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::body::{Body, to_bytes};
    use axum::extract::Request;
    use axum::http::{HeaderName, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::{get, post};
    use axum_idempotent::{IdempotentLayer, IdempotentOptions};
    use ruts::store::memory::MemoryStore;
    use ruts::{CookieOptions, SessionLayer};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;
    use tower::ServiceExt;
    use tower_cookies::CookieManagerLayer;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn reset_counter() {
        COUNTER.store(0, Ordering::SeqCst);
    }

    async fn increment_counter() -> String {
        let count = COUNTER.fetch_add(1, Ordering::SeqCst);
        format!("Response #{}", count)
    }

    async fn return_error() -> impl IntoResponse {
        COUNTER.fetch_add(1, Ordering::SeqCst);
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error")
    }

    async fn create_test_app(idempotent_options: IdempotentOptions) -> Router {
        let store = Arc::new(MemoryStore::new());
        let cookie_options = CookieOptions::build().name("session").max_age(10).path("/");
        let session_layer = SessionLayer::new(store.clone()).with_cookie_options(cookie_options);
        let idempotent_layer = IdempotentLayer::<MemoryStore>::new(idempotent_options);

        Router::new()
            .route("/test", post(increment_counter))
            .route("/error", get(return_error))
            .layer(idempotent_layer)
            .layer(session_layer)
            .layer(CookieManagerLayer::new())
    }

    fn get_session_cookie(response: &axum::http::Response<Body>) -> axum::http::HeaderValue {
        response
            .headers()
            .get_all("set-cookie")
            .iter()
            .find(|&cookie| cookie.to_str().unwrap().starts_with("session="))
            .cloned()
            .expect("Session cookie not found")
    }

    #[tokio::test]
    async fn test_basic_idempotency_with_hashing() {
        reset_counter();
        let options = IdempotentOptions::default().expire_after(3);
        let app = create_test_app(options).await;

        let response1 = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/test")
                    .method("POST")
                    .body(Body::from("test"))
                    .unwrap(),
            )
            .await
            .unwrap();

        let session_cookie = get_session_cookie(&response1);
        let body1 = to_bytes(response1.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body1[..], b"Response #0");

        let response2 = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/test")
                    .method("POST")
                    .header("cookie", session_cookie.clone())
                    .body(Body::from("test"))
                    .unwrap(),
            )
            .await
            .unwrap();
        let body2 = to_bytes(response2.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body2[..], b"Response #0"); // Counter is still 0.

        tokio::time::sleep(Duration::from_secs(3)).await;

        let response3 = app
            .oneshot(
                Request::builder()
                    .uri("/test")
                    .method("POST")
                    .header("cookie", session_cookie)
                    .body(Body::from("test"))
                    .unwrap(),
            )
            .await
            .unwrap();
        let body3 = to_bytes(response3.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body3[..], b"Response #1"); // Counter is now 1.
    }

    #[tokio::test]
    async fn test_idempotency_key_header_mode() {
        reset_counter();
        let options =
            IdempotentOptions::default().use_idempotency_key_header(Some("idempotency-key"));
        let app = create_test_app(options).await;

        let response1 = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/test")
                    .method("POST")
                    .header("idempotency-key", "key-1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let session_cookie = get_session_cookie(&response1);
        assert_eq!(COUNTER.load(Ordering::SeqCst), 1);
        assert!(response1.headers().get("idempotency-replayed").is_none());

        let response2 = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/test")
                    .method("POST")
                    .header("cookie", session_cookie.clone())
                    .header("idempotency-key", "key-1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(COUNTER.load(Ordering::SeqCst), 1); // Counter did not increment.
        assert_eq!(
            response2.headers().get("idempotency-replayed").unwrap(),
            "true"
        );

        let _response3 = app
            .oneshot(
                Request::builder()
                    .uri("/test")
                    .method("POST")
                    .header("cookie", session_cookie)
                    .header("idempotency-key", "key-2")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(COUNTER.load(Ordering::SeqCst), 2); // Counter incremented.
    }

    #[tokio::test]
    async fn test_ignore_body_mode() {
        reset_counter();
        let options = IdempotentOptions::default().ignore_body(true);
        let app = create_test_app(options).await;

        // First request executes handler.
        let response1 = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/test")
                    .method("POST")
                    .body(Body::from("body A"))
                    .unwrap(),
            )
            .await
            .unwrap();
        let session_cookie = get_session_cookie(&response1);
        assert_eq!(COUNTER.load(Ordering::SeqCst), 1);

        // Second request with a different body should be treated as identical and return a cached response.
        let response2 = app
            .oneshot(
                Request::builder()
                    .uri("/test")
                    .method("POST")
                    .header("cookie", session_cookie)
                    .body(Body::from("body B"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(COUNTER.load(Ordering::SeqCst), 1); // Counter did not increment.
        let body = to_bytes(response2.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body[..], b"Response #0");
    }

    #[tokio::test]
    async fn test_ignore_header_mode() {
        reset_counter();
        let options =
            IdempotentOptions::default().ignore_header(HeaderName::from_static("x-request-id"));
        let app = create_test_app(options).await;

        let response1 = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/test")
                    .method("POST")
                    .header("x-request-id", "123")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let session_cookie = get_session_cookie(&response1);
        assert_eq!(COUNTER.load(Ordering::SeqCst), 1);

        let response2 = app
            .oneshot(
                Request::builder()
                    .uri("/test")
                    .method("POST")
                    .header("cookie", session_cookie)
                    .header("x-request-id", "456")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(COUNTER.load(Ordering::SeqCst), 1); // Counter did not increment.
        let body = to_bytes(response2.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body[..], b"Response #0");
    }

    #[tokio::test]
    async fn test_ignored_status_code() {
        reset_counter();
        let options = IdempotentOptions::default();
        let app = create_test_app(options).await;

        let response1 = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/error")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let session_cookie = response1
            .headers()
            .get_all("set-cookie")
            .iter()
            .find(|&cookie| cookie.to_str().unwrap().starts_with("session="));

        assert!(
            session_cookie.is_none(),
            "A session cookie should NOT be set on an error response"
        );
        assert_eq!(COUNTER.load(Ordering::SeqCst), 1);
        assert_eq!(response1.status(), StatusCode::INTERNAL_SERVER_ERROR);

        let response2 = app
            .oneshot(
                Request::builder()
                    .uri("/error")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(COUNTER.load(Ordering::SeqCst), 2); // Counter incremented again.
        assert_eq!(response2.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
