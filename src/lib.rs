//! Middleware for handling idempotent requests in axum applications.
//!
//! This crate provides middleware that deduplicates repeated HTTP requests. When a request
//! repeats, the response cached from the first is returned instead of re-executing the
//! handler, absorbing client retries and accidental double submissions.
//!
//! ## How it Works
//!
//! The middleware operates in one of two modes.
//!
//! 1.  **Direct Key Mode:** configured with [`use_idempotency_key_header`], the middleware
//!     takes a client-provided header (e.g. `Idempotency-Key`) as the cache key. Nothing is
//!     hashed, so the body is never buffered, and the key is the same identifier on both
//!     sides.
//!
//! 2.  **Hashing Mode:** the default. The key is derived from the request's method, target,
//!     headers (configurable) and body. Nothing is asked of the client and nothing can be
//!     opted out of, so a repeated request is deduplicated whether the sender wanted
//!     it.
//!
//! Prefer direct keys when the callers are yours, or are API consumers you can ask to send
//! one. Prefer hashing when you cannot rely on the caller — browser form posts, third-party
//! integrations — and want the protection applied regardless.
//!
//! If a key is found in the session store, the cached response is returned immediately.
//! If not, the request is processed by the handler, and the response is cached before
//! being sent to the client.
//!
//! [`use_idempotency_key_header`]: IdempotentOptions::use_idempotency_key_header
//!
//! ## Features
//!
//! - Request deduplication using either a direct client-provided key or automatic request hashing.
//! - Configurable response caching duration.
//! - Fine-grained controls for hashing, including ignoring the request body or specific headers.
//! - Observability through a replay header (default: `idempotency-replayed`) on cached responses.
//! - Seamless integration with session-based storage via the `ruts` crate.
//!
//! ## Example
//!
//! ```rust,no_run
//! use std::sync::Arc;
//! use axum::{Router, routing::post};
//! use ruts::{CookieOptions, SessionLayer};
//! use axum_idempotent::{IdempotentLayer, IdempotentOptions};
//! use tower_cookies::CookieManagerLayer;
//! use ruts::store::moka::MokaStore;
//!
//! #[tokio::main]
//! async fn main() {
//! // Your session store
//! let store = Arc::new(MokaStore::builder().build());
//!
//! // Configure the idempotency layer to use the "Idempotency-Key" header
//! let idempotent_options = IdempotentOptions::default()
//!     .use_idempotency_key_header(Some("Idempotency-Key"), true)
//!     .expire_after(60 * 5); // Cache responses for 5 minutes
//!
//! // Create the router
//! let app = Router::new()
//!     .route("/payments", post(process_payment))
//!     .layer(IdempotentLayer::<MokaStore>::new(idempotent_options))
//!     .layer(SessionLayer::new(store)
//!         .with_cookie_options(CookieOptions::build().name("session")))
//!     .layer(CookieManagerLayer::new());
//!
//! let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
//!     axum::serve(listener, app).await.unwrap();
//! }
//!
//! async fn process_payment() -> &'static str {
//!     "Payment processed"
//! }
//! ```
//!
//! ## Default Behavior
//!
//! `axum-idempotent` is configured with safe defaults to prevent common issues.
//!
//! ### Ignored Status Codes
//!
//! To avoid caching transient server errors or certain client errors, responses with
//! the following HTTP status codes are **not cached** by default:
//! - `400 Bad Request`
//! - `401 Unauthorized`
//! - `403 Forbidden`
//! - `405 Method Not Allowed`
//! - `408 Request Timeout`
//! - `411 Length Required`
//! - `413 Payload Too Large`
//! - `414 URI Too Long`
//! - `415 Unsupported Media Type`
//! - `422 Unprocessable Entity`
//! - `429 Too Many Requests`
//! - `431 Request Header Fields Too Large`
//! - `500 Internal Server Error`
//! - `502 Bad Gateway`
//! - `503 Service Unavailable`
//! - `504 Gateway Timeout`
//!
//! The `4xx` entries above describe the request envelope rather than the outcome of an
//! operation: the handler never ran, so there is nothing to replay. `404` and `409` are
//! absent, since both could be outcomes of a handler that did run.
//!
//! This list is a preset, not a policy: add to it with `ignore_response_status_code` and
//! take from it with `cache_response_status_code`.
//!
//! ### Ignored Headers
//!
//! In hashing mode, common, request-specific headers
//! are ignored by default to ensure that requests from different clients are treated as
//! identical if the core parameters are the same. This does not apply when using
//! `use_idempotency_key_header`.
//!
//! `accept`, `accept-encoding` and `accept-language` are **not** on this list: they choose
//! which representation a handler returns, so a response cached for one client would
//! otherwise be replayed to a client that asked for a different one.
//!
//! - user-agent,
//! - cache-control,
//! - connection,
//! - cookie,
//! - host,
//! - pragma,
//! - referer,
//! - sec-fetch-dest,
//! - sec-fetch-mode,
//! - sec-fetch-site,
//! - sec-ch-ua,
//! - sec-ch-ua-mobile,
//! - sec-ch-ua-platform

use axum::RequestExt;
use axum::extract::Request;
use axum::http::HeaderValue;
use axum::response::Response;
use ruts::Session;
use ruts::store::{SessionStore, Ttl};
use serde_bytes::ByteBuf;
use std::error::Error;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tower_layer::Layer;
use tower_service::Service;

mod utils;

mod config;
pub use crate::config::IdempotentOptions;
use crate::utils::{bytes_to_response, hash_request, response_to_bytes};

/// Service that handles idempotent request processing.
#[derive(Clone, Debug)]
pub struct IdempotentService<S, T> {
    inner: S,
    config: Arc<IdempotentOptions>,
    phantom: PhantomData<T>,
}

impl<S, T> IdempotentService<S, T> {
    pub fn new(inner: S, config: IdempotentOptions) -> Self {
        Self::from_shared(inner, Arc::new(config))
    }

    fn from_shared(inner: S, config: Arc<IdempotentOptions>) -> Self {
        IdempotentService::<S, T> {
            inner,
            config,
            phantom: PhantomData,
        }
    }
}

impl<S, T> Service<Request> for IdempotentService<S, T>
where
    S: Service<Request, Response = Response> + Clone + Send + 'static,
    S::Error: Send,
    S::Future: Send + 'static,
    T: SessionStore,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, mut req: Request) -> Self::Future {
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        let config = self.config.clone();

        Box::pin(async move {
            let session = match req.extract_parts::<Session<T>>().await {
                Ok(session) => session,
                Err(err) => {
                    tracing::error!("Failed to extract Session from request: {err:?}");
                    // Forward the request to the inner service without idempotency
                    return inner.call(req).await;
                }
            };

            let (req, hash) = match hash_request(req, &config).await {
                Ok(request_and_key) => request_and_key,
                // The body was consumed while being read
                Err(res) => return Ok(res),
            };

            if let Some(hash) = &hash {
                match check_cached_response(hash, &session).await {
                    Ok(Some(mut res)) => {
                        res.headers_mut().insert(
                            config.replay_header_name.clone(),
                            HeaderValue::from_static("true"),
                        );
                        return Ok(res);
                    }
                    Ok(None) => {} // No cached response, continue
                    Err(err) => {
                        tracing::error!("Failed to check idempotent cached response: {err:?}");
                        // Continue without cache
                    }
                }
            }

            let res = inner.call(req).await?;
            let status_code = res.status();
            if !config.ignored_res_status_codes.contains(&status_code) {
                if let Some(hash) = &hash {
                    if session.id().is_none() {
                        return Ok(res);
                    }

                    let (res, response_bytes) =
                        response_to_bytes(res, config.max_cached_response_size).await;

                    let Some(response_bytes) = response_bytes else {
                        return Ok(res);
                    };

                    let response_bytes = ByteBuf::from(response_bytes);
                    
                    let cookie_ttl = session
                        .cookie_max_age()
                        .and_then(|secs| Ttl::new(i64::try_from(secs).ok()?).ok());
                    let field_ttl = cookie_ttl
                        .map_or(config.body_cache_ttl, |ttl| ttl.min(config.body_cache_ttl));

                    #[cfg(feature = "layered-store")]
                    let hot_cache_ttl = config.layered_hot_cache_ttl.map(|ttl| ttl.min(field_ttl));
                    #[cfg(not(feature = "layered-store"))]
                    let hot_cache_ttl = None;

                    let result = session
                        .set(hash, &response_bytes, field_ttl, hot_cache_ttl)
                        .await;

                    if let Err(err) = result {
                        tracing::error!("Failed to cache idempotent response: {err:?}");
                    }

                    return Ok(res);
                }
            }

            Ok(res)
        })
    }
}

/// Layer to apply [`IdempotentService`] middleware in `axum`.
///
/// This layer caches responses in a session store and returns the cached response
/// for identical requests within the configured expiration time.
///
/// # Example
/// ```rust,no_run
/// use std::sync::Arc;
/// use axum::Router;
/// use axum::routing::get;
/// use ruts::{CookieOptions, SessionLayer};
/// use axum_idempotent::{IdempotentLayer, IdempotentOptions};
/// use tower_cookies::CookieManagerLayer;
///
/// #[tokio::main]
/// async fn main() {
/// use ruts::store::moka::MokaStore;
/// let store = Arc::new(MokaStore::builder().build());
///
/// let idempotent_options = IdempotentOptions::default().expire_after(3);
/// let idempotent_layer = IdempotentLayer::<MokaStore>::new(idempotent_options);
///
/// let app = Router::new()
///     .route("/test", get(|| async { "Hello, World!"}))
///     .layer(idempotent_layer)
///     .layer(SessionLayer::new(store.clone())
///         .with_cookie_options(CookieOptions::build().name("session").max_age(10).path("/")))
///     .layer(CookieManagerLayer::new());
/// let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
/// axum::serve(listener, app).await.unwrap();
/// }
/// ```
#[derive(Clone, Debug)]
pub struct IdempotentLayer<T> {
    config: Arc<IdempotentOptions>,
    phantom_data: PhantomData<T>,
}

impl<T> IdempotentLayer<T> {
    pub fn new(config: IdempotentOptions) -> Self {
        IdempotentLayer {
            config: Arc::new(config),
            phantom_data: PhantomData,
        }
    }
}

impl<S, T> Layer<S> for IdempotentLayer<T> {
    type Service = IdempotentService<S, T>;

    fn layer(&self, service: S) -> Self::Service {
        IdempotentService::from_shared(service, self.config.clone())
    }
}

async fn check_cached_response<T: SessionStore>(
    hash: impl AsRef<str>,
    session: &Session<T>,
) -> Result<Option<Response>, Box<dyn Error + Send + Sync>> {
    let response_bytes = session.get::<ByteBuf>(hash.as_ref()).await?;

    let res = if let Some(bytes) = response_bytes {
        let response = bytes_to_response(bytes.into_vec())?;

        Some(response)
    } else {
        None
    };

    Ok(res)
}
