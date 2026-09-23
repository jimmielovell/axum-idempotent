# axum-idempotent

[![Documentation](https://docs.rs/axum-idempotent/badge.svg)](https://docs.rs/axum-idempotent)
[![Crates.io](https://img.shields.io/crates/v/axum-idempotent.svg)](https://crates.io/crates/axum-idempotent)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![Rust](https://img.shields.io/badge/rust-1.85.0%2B-blue.svg?maxAge=3600)](https://github.com/jimmielovell/axum-idempotent)

Middleware for handling idempotent requests in axum applications.

This crate provides middleware that deduplicates repeated HTTP requests. When a request repeats, the response cached from the first is returned instead of re-executing the handler, absorbing client retries and accidental double submissions.

## How it Works

The middleware operates in one of two modes. They differ in who controls deduplication, not only in cost, so neither is the right answer everywhere.

1.  **Direct Key Mode:** configured with `use_idempotency_key_header()`, the middleware takes a client-provided header (e.g. `Idempotency-Key`) as the cache key. Nothing is hashed, so the body is never buffered, and the key is the same identifier on both sides, which makes a replay easy to trace. Because the key stands for the *operation* rather than the bytes, a retry still deduplicates when the request is not byte-identical — re-serialized JSON, a changed header. The trade-off is that deduplication is advisory: the client picks the key, so the client also decides whether two requests count as the same operation.

2.  **Hashing Mode:** the default. The key is derived from the request's method, target, headers (configurable) and body. Nothing is asked of the client and nothing can be opted out of, so a repeated request is deduplicated whether or not the sender wanted it. The trade-off is that it only recognises byte-identical repeats: a client that re-serializes its body between attempts produces a different key and reaches the handler again.

Prefer direct keys when the callers are yours, or are API consumers you can ask to send one. Prefer hashing when you cannot rely on the caller — browser form posts, third-party integrations — and want the protection applied regardless.

If a key is found in the session store, the cached response is returned immediately. If not, the request is processed by the handler, and the response is cached before being sent to the client.

Both modes are best-effort. The middleware forwards a request without idempotency handling when the session or the store is unavailable, and two identical requests that arrive concurrently can both reach the handler. Treat it as a retry safety net, not as a guarantee that a handler runs at most once.

A response is cached only for a request that already carries a session. The middleware never creates one, so a route reachable before your application establishes a session is not covered.

## Features

-   Request deduplication using either a direct client-provided key or automatic request hashing.
-   Configurable response caching duration.
-   Fine-grained controls for hashing, including ignoring the request body or specific headers.
-   Observability through a replay header (default: `idempotency-replayed`) on cached responses.
-   Seamless integration with session-based storage via the [ruts](https://crates.io/crates/ruts) crate.

## Dependencies and Layer Ordering

This middleware requires a session layer, such as `SessionLayer` from the [ruts](https://crates.io/crates/ruts) crate. For the `IdempotentLayer` to access the session, it must be placed *inside* the `SessionLayer`.

The correct order is:
1.  `CookieManagerLayer` (Outermost)
2.  `SessionLayer`
3.  `IdempotentLayer` (Innermost)


## Example

```rust
use std::sync::Arc;
use axum::{Router, routing::post};
use ruts::{CookieOptions, SessionLayer};
use axum_idempotent::{IdempotentLayer, IdempotentOptions};
use tower_cookies::CookieManagerLayer;
use ruts::store::moka::MokaStore;

#[tokio::main]
async fn main() {
    // Your session store
    let store = Arc::new(MokaStore::builder().build());

    // Configure the idempotency layer to use the "Idempotency-Key" header
    let idempotent_options = IdempotentOptions::default()
        .use_idempotency_key_header(Some("Idempotency-Key"), true)
        .expire_after(60 * 5); // Cache responses for 5 minutes

    // Create the router with the correct layer order
    let app = Router::new()
        .route("/payments", post(process_payment))
        .layer(IdempotentLayer::<MokaStore>::new(idempotent_options))
        .layer(SessionLayer::new(store)
            .with_cookie_options(CookieOptions::build().name("session")))
        .layer(CookieManagerLayer::new());

    // Run the server
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn process_payment() -> &'static str {
    "Payment processed"
}
```

## Default Behavior

`axum-idempotent` is configured with safe defaults to prevent common issues.

### Ignored Status Codes

To avoid caching transient server errors or certain client errors, responses with the following HTTP status codes are not cached by default:

- 400 Bad Request
- 401 Unauthorized
- 403 Forbidden
- 405 Method Not Allowed
- 408 Request Timeout
- 411 Length Required
- 413 Payload Too Large
- 414 URI Too Long
- 415 Unsupported Media Type
- 422 Unprocessable Entity
- 429 Too Many Requests
- 431 Request Header Fields Too Large
- 500 Internal Server Error
- 502 Bad Gateway
- 503 Service Unavailable
- 504 Gateway Timeout

The `4xx` entries above describe the request envelope rather than the outcome of an operation: the handler never ran, so there is nothing to replay. `404` and `409` are deliberately absent, since both are plausible outcomes of an operation that did run.

This list is a preset, not a policy: add to it with `ignore_response_status_code` and take from it with `cache_response_status_code`.

### Ignored Headers

In hashing mode, common, the following request-specific headers  are ignored by default to ensure that requests from different clients are treated as identical if the core parameters are the same. This does not apply when using use_idempotency_key_header.

`accept`, `accept-encoding` and `accept-language` are **not** on this list: they choose which representation a handler returns, so a response cached for one client would otherwise be replayed to a client that asked for a different one.

- user-agent,
- cache-control,
- connection,
- cookie,
- host,
- pragma,
- referer,
- sec-fetch-dest,
- sec-fetch-mode,
- sec-fetch-site,
- sec-ch-ua,
- sec-ch-ua-mobile,
- sec-ch-ua-platform

## License

This project is licensed under the MIT License.
