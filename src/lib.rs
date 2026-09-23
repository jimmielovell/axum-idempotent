#![doc = include_str!("../README.md")]

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
                Err(res) => return Ok(*res),
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
