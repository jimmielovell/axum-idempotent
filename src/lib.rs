//! Middleware for handling idempotent requests in axum applications.
//!
//! This crate provides middleware that ensures idempotency of HTTP requests. When an
//! identical request is made, a cached response is returned instead of re-executing
//! the handler, preventing duplicate operations like accidental double payments.
//!
//! ## How it Works
//!
//! The middleware operates in one of two modes:
//!
//! 1.  **Direct Key Mode (Recommended):** By configuring `use_idempotency_key_header()`, the
//!     middleware uses a client-provided header (e.g., `Idempotency-Key`) value directly
//!     as the cache key. This is the most performant and observable method, as it avoids
//!     server-side hashing and uses an identifier known to both the client and server.
//!
//! 2.  **Hashing Mode:** If not using a direct key, a unique hash is generated
//!     from the request's method, path, headers (configurable), and body. This hash is
//!     then used as the cache key.
//!
//! If a key is found in the session store, the cached response is returned immediately.
//! If not, the request is processed by the handler, and the response is cached before
//! being sent to the client.
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
//! use ruts::store::memory::MemoryStore;
//!
//! # #[tokio::main]
//! # async fn main() {
//! // Your session store
//! let store = Arc::new(MemoryStore::new());
//!
//! // Configure the idempotency layer to use the "Idempotency-Key" header
//! let idempotent_options = IdempotentOptions::default()
//!     .use_idempotency_key_header(Some("Idempotency-Key"))
//!     .expire_after(60 * 5); // Cache responses for 5 minutes
//!
//! // Create the router
//! let app = Router::new()
//!     .route("/payments", post(process_payment))
//!     .layer(IdempotentLayer::<MemoryStore>::new(idempotent_options))
//!     .layer(SessionLayer::new(store)
//!         .with_cookie_options(CookieOptions::build().name("session")))
//!     .layer(CookieManagerLayer::new());
//!
//! # let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
//! # axum::serve(listener, app).await.unwrap();
//! # }
//! #
//! # async fn process_payment() -> &'static str {
//! #     "Payment processed"
//! # }
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
//! - `408 Request Timeout`
//! - `429 Too Many Requests`
//! - `500 Internal Server Error`
//! - `502 Bad Gateway`
//! - `503 Service Unavailable`
//! - `504 Gateway Timeout`
//!
//! ### Ignored Headers
//!
//! In hashing mode, common, request-specific headers
//! are ignored by default to ensure that requests from different clients are treated as
//! identical if the core parameters are the same. This does not apply when using
//! `use_idempotency_key_header`.
//!
//! - user-agent,
//! - accept,
//! - accept-encoding,
//! - accept-language,
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

use axum::extract::Request;
use axum::response::Response;
use axum::RequestExt;
use ruts::store::SessionStore;
use ruts::Session;
use std::error::Error;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};
use tower_layer::Layer;
use tower_service::Service;

mod utils;

mod config;
pub use crate::config::IdempotentOptions;
use crate::utils::{bytes_to_response, hash_request, response_to_bytes};

#[cfg(feature = "layered-store")]
pub use crate::config::LayeredCacheConfig;

/// Service that handles idempotent request processing.
#[derive(Clone, Debug)]
pub struct IdempotentService<S, T> {
    inner: S,
    config: IdempotentOptions,
    phantom: PhantomData<T>,
}

impl<S, T> IdempotentService<S, T> {
    pub const fn new(inner: S, config: IdempotentOptions) -> Self {
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

            let (req, hash) = hash_request(req, &config).await;

            if let Some(hash) = &hash {
                match check_cached_response(hash, &session).await {
                    Ok(Some(mut res)) => {
                        res.headers_mut()
                            .insert(config.replay_header_name, "true".parse().unwrap());
                        return Ok(res)
                    },
                    Ok(None) => {}  // No cached response, continue
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
                    let (res, response_bytes) = response_to_bytes(res).await;

                    #[cfg(feature = "layered-store")]
                    let result = {
                        use ruts::store::layered::LayeredWriteStrategy;
                        if let Some(hot_cache_ttl_secs) = config.layered_hot_cache_ttl_secs {
                            session
                                .update(&hash, &LayeredWriteStrategy(response_bytes, hot_cache_ttl_secs), Some(config.body_cache_ttl_secs))
                                .await
                        } else {
                            session
                                .update(&hash, &response_bytes, Some(config.body_cache_ttl_secs))
                                .await
                        }
                    };
                    #[cfg(not(feature = "layered-store"))]
                    let result = session
                        .update(&hash, &response_bytes, Some(config.body_cache_ttl_secs))
                        .await;

                    if let Err(err) = result {
                        tracing::error!("Failed to cache idempotent response: {err:?}");
                    }

                    return Ok(res)
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
/// # use std::sync::Arc;
/// # use axum::Router;
/// # use axum::routing::get;
/// # use ruts::{CookieOptions, SessionLayer};
/// # use axum_idempotent::{IdempotentLayer, IdempotentOptions};
/// # use tower_cookies::CookieManagerLayer;
///
/// # #[tokio::main]
/// # async fn main() {
/// # use ruts::store::memory::MemoryStore;
/// let store = Arc::new(MemoryStore::new());
///
/// let idempotent_options = IdempotentOptions::default().expire_after(3);
/// let idempotent_layer = IdempotentLayer::<MemoryStore>::new(idempotent_options);
///
/// let app = Router::new()
///     .route("/test", get(|| async { "Hello, World!"}))
///     .layer(idempotent_layer)
///     .layer(SessionLayer::new(store.clone())
///         .with_cookie_options(CookieOptions::build().name("session").max_age(10).path("/")))
///     .layer(CookieManagerLayer::new());
/// # let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
/// # axum::serve(listener, app).await.unwrap();
/// # }
/// ```
#[derive(Clone, Debug)]
pub struct IdempotentLayer<T> {
    config: IdempotentOptions,
    phantom_data: PhantomData<T>,
}

impl<T> IdempotentLayer<T> {
    pub const fn new(config: IdempotentOptions) -> Self {
        IdempotentLayer {
            config,
            phantom_data: PhantomData,
        }
    }
}

impl<S, T> Layer<S> for IdempotentLayer<T> {
    type Service = IdempotentService<S, T>;

    fn layer(&self, service: S) -> Self::Service {
        IdempotentService::new(service, self.config.clone())
    }
}

async fn check_cached_response<T: SessionStore>(
    hash: impl AsRef<str>,
    session: &Session<T>,
) -> Result<Option<Response>, Box<dyn Error + Send + Sync>> {
    let response_bytes = session.get::<Vec<u8>>(hash.as_ref()).await?;

    let res = if let Some(bytes) = response_bytes {
        let response = bytes_to_response(bytes)?;

        Some(response)
    } else {
        None
    };

    Ok(res)
}
