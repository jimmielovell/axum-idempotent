# axum-idempotent

[![Documentation](https://docs.rs/axum-idempotent/badge.svg)](https://docs.rs/axum-idempotent)
[![Crates.io](https://img.shields.io/crates/v/axum-idempotent.svg)](https://crates.io/crates/axum-idempotent)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![Rust](https://img.shields.io/badge/rust-1.85.0%2B-blue.svg?maxAge=3600)](https://github.com/jimmielovell/axum-idempotent)

Middleware for handling idempotent requests in axum applications.

This crate provides middleware that ensures idempotency of HTTP requests. When an identical request is made, a cached response is returned instead of re-executing the handler, preventing duplicate operations like accidental double payments.

## How it Works

The middleware operates in one of two modes:

1.  **Direct Key Mode (Recommended):** By configuring `use_idempotency_key_header()`, the middleware uses a client-provided header (e.g., `Idempotency-Key`) value directly as the cache key. This is the most performant and observable method, as it avoids server-side hashing and uses an identifier known to both the client and server.

2.  **Hashing Mode:** If not using a direct key, a unique hash is generated from the request's method, path, headers (configurable), and body. This hash is then used as the cache key.

If a key is found in the session store, the cached response is returned immediately. If not, the request is processed by the handler, and the response is cached before being sent to the client.

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

## Usage

Add this to your `Cargo.toml`:

```toml
[dependencies]
axum-idempotent = "0.1.6"
```

## Example

```rust
use std::sync::Arc;
use axum::{Router, routing::post};
use ruts::{CookieOptions, SessionLayer};
use axum_idempotent::{IdempotentLayer, IdempotentOptions};
use tower_cookies::CookieManagerLayer;
use ruts::store::memory::MemoryStore;

#[tokio::main]
async fn main() {
    // Your session store
    let store = Arc::new(MemoryStore::new());

    // Configure the idempotency layer to use the "Idempotency-Key" header
    let idempotent_options = IdempotentOptions::default()
        .use_idempotency_key_header(Some("Idempotency-Key"))
        .expire_after(60 * 5); // Cache responses for 5 minutes

    // Create the router with the correct layer order
    let app = Router::new()
        .route("/payments", post(process_payment))
        .layer(IdempotentLayer::<MemoryStore>::new(idempotent_options))
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
- 408 Request Timeout
- 429 Too Many Requests
- 500 Internal Server Error
- 502 Bad Gateway
- 503 Service Unavailable
- 504 Gateway Timeout

### Ignored Headers

In hashing mode, common, the following request-specific headers  are ignored by default to ensure that requests from different clients are treated as identical if the core parameters are the same. This does not apply when using use_idempotency_key_header.

- user-agent,
- accept,
- accept-encoding,
- accept-language,
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
