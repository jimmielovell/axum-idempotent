# Changelog

## [0.4.0] - 2026-09-23

### Breaking

- Requires `ruts` 0.11. `MemoryStore` is gone from `ruts`; use `MokaStore` behind its
  `moka-store` feature.
- A response is cached only when the request already carries a session. Routes reached
  before the application establishes one lose idempotency.
- `IdempotentOptions::new`, `expire_after` and `layered_cache_config` panic on a TTL
  outside `0..=i32::MAX` seconds, rather than handing it to the store on every request.

### Security

- Caching a response no longer creates a session. A cache write called into `ruts`'s
  `get_or_set_id`, so unauthenticated traffic could add a session, and a stored
  response body, for every key it invented.

### Fixed

- A cached response is stored for no longer than the session cookie's `max_age`. Past
  that the client cannot present the session to replay it, so a longer `expire_after`
  only kept the session's data alive in the store. A `layered_cache_config` hot TTL is
  held to the same bound.

## [0.3.0] - 2026-08-19

### Breaking

- Entries cached by an earlier version are no longer read back: keys are namespaced
  and the stored format changed. Affected handlers run once more per key, which is the
  case idempotency exists to make safe.
- `use_idempotency_key_header` takes a second argument, whether to require the header.
- `replay_header_name` takes `&str` rather than `&'static str`.
- `IdempotentLayer::new` and `IdempotentService::new` are no longer `const fn`. They
  could never be used in a const context anyway, since building `IdempotentOptions`
  allocates.
- In hashing mode, cache keys now vary by `accept`, `accept-encoding` and
  `accept-language`, and by the query string. Expect a lower hit rate, and a correct one.

### Security

- Cache keys are namespaced with an `idem:` prefix. In direct-key mode the key is the
  raw client header value and was used verbatim as a `ruts` session field name, so
  `Idempotency-Key: user` read, and then overwrote, the application's `user` session
  field. A client-supplied key is now also limited to 1-255 printable ASCII characters.
- Request bodies are read up to `max_body_size` (2 MB, matching axum's
  `DefaultBodyLimit`) instead of `usize::MAX`. Every handler behind the layer had lost
  its body limit, because that limit is applied by extractors and not by a `to_bytes`
  call in a tower service.
- A request body that fails to read returns `400` instead of panicking the middleware.
  A client could trigger it at will by disconnecting mid-upload.

### Fixed

- `422`, along with `405`, `411`, `413`, `414`, `415` and `431`, is no longer cached by
  default. These describe the request envelope rather than the outcome of an operation:
  the handler never ran, so there is nothing to replay. In direct-key mode a client that fixed its payload and retried under the same key was served the stale rejection for the whole TTL. `404` and `409` remain cacheable, since both can be outcomes of a handler that did run.
- Repeated response headers survive a replay. The cached format was parsed with
  `HeaderMap::insert`, so a response carrying two `set-cookie` headers came back with
  one.
- Header values that are not UTF-8 no longer break the cached entry. `HeaderValue`
  permits obs-text, and a failed decode silently disabled idempotency for that key.
- A truncated or corrupt cached entry is an error rather than a panic or a short body.
- The query string is part of the cache key. `POST /search?q=a` and `?q=b` hashed
  identically.
- Hash fields are length-prefixed, so `x-a: bc` and `x-ab: c` no longer collide.
- `accept`, `accept-encoding` and `accept-language` are no longer ignored when hashing.
  They select which representation a handler returns, so ignoring them replayed a
  response cached for one client to a client that asked for another.
- `replay_header_name` accepts any capitalisation. It used `HeaderName::from_static`,
  which panics unless the name is lowercase.
- Direct-key mode is set by one value rather than three interacting flags, so a later
  unrelated option can no longer switch the layer back to hashing.
- A present-but-unusable idempotency key returns `400` rather than silently forwarding
  the request without idempotency handling.

### Added

- `max_body_size`, the largest request body read in order to hash it.
- `max_cached_response_size`, the largest response stored in the session.
- `cache_response_status_code`, to take a status back off the ignore list. The defaults
  are a preset, not a policy.
- A `require_header` argument on `use_idempotency_key_header`, answering `400` when a
  request carries no key at all.

### Changed

- Responses whose length is not known before reading them are never cached. Reading one
  withheld it from the client until the stream ended, which for an open-ended stream
  never happened.
- Responses larger than `max_cached_response_size` (1 MB) are returned but not cached.
- A request body already known to exceed `max_body_size` is forwarded untouched without
  idempotency handling. One of unknown length is read up to the limit and answered with
  `413` if it exceeds it, since by then it can no longer be given to the handler.

### Performance

- Options are shared behind an `Arc` instead of being deep-cloned on every request.
- Cached responses are stored as a byte string rather than a sequence of integers.
  bincode's serde path wrote one `u8` at a time: 168us to encode a 256 KiB response
  against 5.6us for a bulk copy.
- One fewer `HeaderName` clone per header per request, and one fewer header-map clone
  per cached response.

## [0.1.6] - 2025-09-08

### Added

- Added `use_idempotency_key_header()` to allow using a client-provided header (e.g., `Idempotency-Key`) directly as the cache key. This is a major performance and observability improvement as it bypasses all server-side hashing.
- The middleware now adds an `idempotency-replayed: true` header to responses served from the cache, improving debuggability. The header name is configurable via `replay_header_name()`.
- Added the `ignore_body()` option to explicitly exclude the request body from the hash calculation.
- Added layered_cache_config() for fine-grained control over caching strategies when using ruts's LayeredStore.

### Changed

- Expanded the list of status codes that are not cached by default to include `403`, `408`, `429`, and `503` to better handle transient errors.


## [0.1.2] - 2025-02-14

### Changed

- Shorten session field (request) by hashing with blake3.

## [0.1.1] - 2025-02-07

### Fixed

Proper error handling when:
  1. `Session` extractor not found in the request.
  2. Failed to cache response
  3. Failed to check cached response

# [0.1.0] - 2025-02-07
- Initial Release