#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::body::{Body, to_bytes};
    use axum::extract::Request;
    use axum::http::{HeaderName, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::{get, post};
    use axum_idempotent::{IdempotentLayer, IdempotentOptions};
    use ruts::store::Ttl;
    use ruts::store::moka::MokaStore;
    use ruts::{CookieOptions, Session, SessionLayer};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;
    use tower::ServiceExt;
    use tower_cookies::CookieManagerLayer;

    fn create_test_app(idempotent_options: IdempotentOptions) -> (Router, Arc<AtomicU64>) {
        create_test_app_with_cookie_max_age(idempotent_options, 10)
    }

    fn create_test_app_with_cookie_max_age(
        idempotent_options: IdempotentOptions,
        cookie_max_age: u64,
    ) -> (Router, Arc<AtomicU64>) {
        let store = Arc::new(MokaStore::builder().build());
        let cookie_options = CookieOptions::build()
            .name("session")
            .max_age(cookie_max_age)
            .path("/");
        let session_layer = SessionLayer::new(store.clone()).with_cookie_options(cookie_options);
        let idempotent_layer = IdempotentLayer::<MokaStore>::new(idempotent_options);

        let counter = Arc::new(AtomicU64::new(0));

        let test_counter = counter.clone();
        let error_counter = counter.clone();
        let validate_counter = counter.clone();

        let app = Router::new()
            .route(
                "/test",
                post(move || {
                    let counter = test_counter.clone();
                    async move { format!("Response #{}", counter.fetch_add(1, Ordering::SeqCst)) }
                }),
            )
            .route(
                "/error",
                get(move || {
                    let counter = error_counter.clone();
                    async move {
                        counter.fetch_add(1, Ordering::SeqCst);
                        (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
                    }
                }),
            )
            // Rejects one body and accepts any other, so a client can "fix" its payload.
            .route(
                "/validate",
                post(move |body: String| {
                    let counter = validate_counter.clone();
                    async move {
                        counter.fetch_add(1, Ordering::SeqCst);
                        if body == "bad" {
                            (StatusCode::UNPROCESSABLE_ENTITY, "invalid").into_response()
                        } else {
                            (StatusCode::OK, "accepted").into_response()
                        }
                    }
                }),
            )
            // Writes and reads an ordinary application session field, to check that
            // the middleware never addresses the same namespace.
            .route(
                "/app-field",
                post(|session: Session<MokaStore>| async move {
                    session
                        .set("user", &String::from("alice"), Ttl::new(60).unwrap(), None)
                        .await
                        .unwrap();
                    "stored"
                })
                .get(|session: Session<MokaStore>| async move {
                    match session.get::<String>("user").await {
                        Ok(Some(user)) => user,
                        Ok(None) => String::from("<missing>"),
                        Err(_) => String::from("<corrupt>"),
                    }
                }),
            )
            .layer(idempotent_layer)
            .layer(session_layer)
            .layer(CookieManagerLayer::new());

        (app, counter)
    }

    async fn establish_session(
        app: &Router,
        idempotency_key: Option<&str>,
    ) -> axum::http::HeaderValue {
        let mut builder = Request::builder().uri("/app-field").method("POST");
        if let Some(key) = idempotency_key {
            builder = builder.header("idempotency-key", key);
        }

        let response = app
            .clone()
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap();

        get_session_cookie(&response)
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
        let options = IdempotentOptions::default().expire_after(3);
        let (app, _counter) = create_test_app(options);
        let session_cookie = establish_session(&app, None).await;

        let response1 = app
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
        let options =
            IdempotentOptions::default().use_idempotency_key_header(Some("idempotency-key"), true);
        let (app, counter) = create_test_app(options);
        let session_cookie = establish_session(&app, Some("setup")).await;

        let response1 = app
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

        assert_eq!(counter.load(Ordering::SeqCst), 1);
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
        assert_eq!(counter.load(Ordering::SeqCst), 1); // Counter did not increment.
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
        assert_eq!(counter.load(Ordering::SeqCst), 2); // Counter incremented.
    }

    #[tokio::test]
    async fn test_ignore_body_mode() {
        let options = IdempotentOptions::default().ignore_body(true);
        let (app, counter) = create_test_app(options);
        let session_cookie = establish_session(&app, None).await;

        // First request executes handler.
        app.clone()
            .oneshot(
                Request::builder()
                    .uri("/test")
                    .method("POST")
                    .header("cookie", session_cookie.clone())
                    .body(Body::from("body A"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 1);

        // Second request with a different body should be treated as identical
        // and return a cached response.
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
        assert_eq!(counter.load(Ordering::SeqCst), 1); // Counter did not increment.
        let body = to_bytes(response2.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body[..], b"Response #0");
    }

    #[tokio::test]
    async fn test_ignore_header_mode() {
        let options =
            IdempotentOptions::default().ignore_header(HeaderName::from_static("x-request-id"));
        let (app, counter) = create_test_app(options);
        let session_cookie = establish_session(&app, None).await;

        app.clone()
            .oneshot(
                Request::builder()
                    .uri("/test")
                    .method("POST")
                    .header("cookie", session_cookie.clone())
                    .header("x-request-id", "123")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 1);

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
        assert_eq!(counter.load(Ordering::SeqCst), 1); // Counter did not increment.
        let body = to_bytes(response2.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body[..], b"Response #0");
    }

    #[tokio::test]
    async fn test_idempotency_key_cannot_address_application_session_fields() {
        let options =
            IdempotentOptions::default().use_idempotency_key_header(Some("idempotency-key"), true);
        let (app, counter) = create_test_app(options);

        // Establish a session holding an application field named "user".
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/app-field")
                    .method("POST")
                    .header("idempotency-key", "setup")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let session_cookie = get_session_cookie(&response);

        // A request whose idempotency key collides with that field name.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/test")
                    .method("POST")
                    .header("cookie", session_cookie.clone())
                    .header("idempotency-key", "user")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        assert!(
            response.headers().get("idempotency-replayed").is_none(),
            "an application field must not be served as a cached response"
        );

        // The application field must be untouched.
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/app-field")
                    .method("GET")
                    .header("cookie", session_cookie)
                    .header("idempotency-key", "read-back")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(
            &body[..],
            b"alice",
            "the idempotency key overwrote an application session field"
        );
    }

    /// A rejected request has done no work, so a corrected retry under the same key must
    /// reach the handler rather than be served the stale rejection for the whole TTL.
    #[tokio::test]
    async fn test_a_corrected_retry_is_not_served_the_cached_rejection() {
        let options =
            IdempotentOptions::default().use_idempotency_key_header(Some("idempotency-key"), true);
        let (app, counter) = create_test_app(options);

        // A rejected response is not cached, so it establishes no session of its own.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/app-field")
                    .method("POST")
                    .header("idempotency-key", "setup")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let session_cookie = get_session_cookie(&response);

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/validate")
                    .method("POST")
                    .header("cookie", session_cookie.clone())
                    .header("idempotency-key", "key-1")
                    .body(Body::from("bad"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(counter.load(Ordering::SeqCst), 1);

        // Same key, corrected payload: standard client retry behaviour.
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/validate")
                    .method("POST")
                    .header("cookie", session_cookie)
                    .header("idempotency-key", "key-1")
                    .body(Body::from("good"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            counter.load(Ordering::SeqCst),
            2,
            "the corrected retry never reached the handler"
        );
    }

    #[tokio::test]
    async fn test_cached_response_does_not_outlive_the_cookie() {
        let options = IdempotentOptions::default().expire_after(60);
        let (app, counter) = create_test_app_with_cookie_max_age(options, 1);
        let session_cookie = establish_session(&app, None).await;

        let request = || {
            Request::builder()
                .uri("/test")
                .method("POST")
                .header("cookie", session_cookie.clone())
                .body(Body::from("same"))
                .unwrap()
        };

        app.clone().oneshot(request()).await.unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 1);

        tokio::time::sleep(Duration::from_secs(2)).await;

        let response = app.clone().oneshot(request()).await.unwrap();
        assert!(response.headers().get("idempotency-replayed").is_none());
        assert_eq!(
            counter.load(Ordering::SeqCst),
            2,
            "the response should have expired with the cookie, not after `expire_after`"
        );
    }

    #[tokio::test]
    async fn test_a_request_without_a_session_is_not_cached() {
        let (app, counter) = create_test_app(IdempotentOptions::default());

        for _ in 0..2 {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/test")
                        .method("POST")
                        .body(Body::from("same"))
                        .unwrap(),
                )
                .await
                .unwrap();

            assert!(
                response
                    .headers()
                    .get_all("set-cookie")
                    .iter()
                    .next()
                    .is_none(),
                "no session should be created just to cache a response"
            );
        }

        assert_eq!(
            counter.load(Ordering::SeqCst),
            2,
            "nothing should have been cached to replay"
        );
    }

    #[tokio::test]
    async fn test_ignored_status_code() {
        let options = IdempotentOptions::default();
        let (app, counter) = create_test_app(options);

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
        assert_eq!(counter.load(Ordering::SeqCst), 1);
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

        assert_eq!(counter.load(Ordering::SeqCst), 2); // Counter incremented again.
        assert_eq!(response2.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
