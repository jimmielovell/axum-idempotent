# Changelog

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
