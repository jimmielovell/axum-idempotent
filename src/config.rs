use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use std::collections::HashSet;

/// Default header read in direct-key mode.
const DEFAULT_IDEMPOTENCY_KEY_HEADER: &str = "idempotency-key";

/// Where the cache key comes from.
///
/// A single value rather than a set of flags, so that the mode cannot be changed as a
/// side effect of setting an unrelated option.
#[derive(Clone, Debug)]
pub(crate) enum KeySource {
    /// Hash the request's method, target, headers and body.
    Hash,
    /// Take the value of this request header as the key.
    Header(String),
}

/// Configuration options for the idempotency layer.
///
/// Configure:
/// - How long responses should be cached
/// - Which headers should be ignored when calculating the request hash
/// - Whether to ignore all headers entirely
///
/// # Example
/// ```rust
/// use axum_idempotent::IdempotentOptions;
/// use axum::http::HeaderName;
///
/// let options_1 = IdempotentOptions::default()
///     .expire_after(60) // Cache for 60 seconds
///     .ignore_header(HeaderName::from_static("x-request-id"))
///     .ignore_all_headers();
///
/// let options_2 = IdempotentOptions::new(60);
/// ```
#[derive(Clone, Debug)]
pub struct IdempotentOptions {
    pub(crate) key_source: KeySource,
    pub(crate) require_idempotency_key: bool,
    pub(crate) replay_header_name: HeaderName,
    pub(crate) ignore_body: bool,
    pub(crate) ignored_req_headers: HashSet<HeaderName>,
    pub(crate) ignored_res_status_codes: HashSet<StatusCode>,
    pub(crate) ignored_header_values: HeaderMap,
    pub(crate) ignore_all_headers: bool,
    pub(crate) max_body_size: usize,
    pub(crate) max_cached_response_size: usize,
    pub(crate) body_cache_ttl_secs: i64,
    #[cfg(feature = "layered-store")]
    pub(crate) layered_hot_cache_ttl_secs: Option<i64>,
}

impl IdempotentOptions {
    pub fn new(body_cache_ttl_secs: i64) -> Self {
        Self {
            body_cache_ttl_secs,
            ..Default::default()
        }
    }

    /// Sets the expiration time in seconds for cached responses.
    pub fn expire_after(mut self, seconds: i64) -> Self {
        self.body_cache_ttl_secs = seconds;
        self
    }

    /// Whether the request body should be ignored when calculating the idempotency key.
    ///
    /// By default, the request body is included in the key. If you set this to `true`,
    /// only the request method, path, and headers will be used.
    ///
    /// **NOTE:** Setting this to `true` can significantly improve performance as it avoids
    /// reading the entire request body into memory. However, it also means that two requests
    /// with different bodies will be treated as identical if their method, path, and headers
    /// are the same, which may not be the desired behavior.
    pub fn ignore_body(mut self, ignore: bool) -> Self {
        self.ignore_body = ignore;
        self
    }

    /// Sets the largest request body the middleware will read in order to hash it.
    ///
    /// Defaults to 2 MB, matching axum's own [`DefaultBodyLimit`]. Ignored when
    /// [`ignore_body`](Self::ignore_body) is set, since the body is never read then.
    ///
    /// A request whose length is known in advance to exceed this is forwarded to the handler
    /// untouched, without idempotency handling, leaving the handler's own body limit to decide
    /// whether to accept it. A request whose length is not known in advance (a chunked body
    /// with no `content-length`) is read up to this limit, and rejected with
    /// `413 Payload Too Large` if it exceeds it — by that point the body has been consumed and
    /// can no longer be handed to the handler.
    ///
    /// [`DefaultBodyLimit`]: https://docs.rs/axum/latest/axum/extract/struct.DefaultBodyLimit.html
    pub fn max_body_size(mut self, bytes: usize) -> Self {
        self.max_body_size = bytes;
        self
    }

    /// Sets the largest response body the middleware will store in the session.
    ///
    /// Defaults to 1 MB. A larger response is returned to the client but not cached, so a
    /// repeat of the same request re-runs the handler.
    ///
    /// Responses whose length is not known before reading them; a stream, or anything else
    /// sent without a `content-length` are never cached, at any limit.
    pub fn max_cached_response_size(mut self, bytes: usize) -> Self {
        self.max_cached_response_size = bytes;
        self
    }

    /// Adds a header to the list of headers that should be ignored when calculating the request hash.
    pub fn ignore_header(mut self, name: HeaderName) -> Self {
        self.ignored_req_headers.insert(name);
        self
    }

    /// Adds a header with a specific value to be ignored when calculating the request hash.
    ///
    /// If the header exists with a different value, it will still be included in the hash.
    pub fn ignore_header_with_value(mut self, name: HeaderName, value: HeaderValue) -> Self {
        self.ignored_header_values.insert(name, value);
        self
    }

    /// Configures the layer to ignore all headers when calculating the request hash.
    ///
    /// When enabled, only the method, path, and body will be used to determine idempotency.
    pub fn ignore_all_headers(mut self) -> Self {
        self.ignore_all_headers = true;
        self
    }

    /// Adds a status code to the list of responses that are not cached.
    pub fn ignore_response_status_code(mut self, status_code: StatusCode) -> Self {
        self.ignored_res_status_codes.insert(status_code);
        self
    }

    /// Removes a status code from the list of responses that are not cached.
    ///
    /// The defaults are a preset, not a policy: use this to cache a status they exclude.
    pub fn cache_response_status_code(mut self, status_code: StatusCode) -> Self {
        self.ignored_res_status_codes.remove(&status_code);
        self
    }

    /// Configures the middleware to use a request header's value directly as the idempotency key.
    ///
    /// When this option is enabled, the middleware will **not** hash any part of the request.
    /// Instead, it will look for the specified header (default: "idempotency-key") and use its
    /// value as the unique key for cache lookups.
    ///
    /// This is the most performant method and improves debuggability, as the client-provided key
    /// is the same key used in the cache.
    ///
    /// **NOTE:** As a consequence, all other parts of the request, including other headers and the
    /// request body, are ignored for the purpose of the idempotency check. This mode is a
    /// property of the layer, so [`ignore_body`](Self::ignore_body) and
    /// [`ignore_all_headers`](Self::ignore_all_headers) have no effect once it is set.
    ///
    /// The key must be 1 to 255 printable ASCII characters (no spaces). A request with a
    /// key outside that range is rejected with `400 Bad Request`: the client asked for
    /// idempotency, so silently withholding it would surface later as a duplicate operation.
    ///
    /// When `require_header` is `false`, a request carrying no key is passed through without
    /// idempotency handling. Requiring the header also turns away requests a browser can be
    /// induced to make cross-origin, which cannot set custom headers without a CORS preflight.
    ///
    /// Keys are namespaced internally, so they cannot collide with the session fields your
    /// application stores.
    ///
    /// Note that in this mode deduplication is advisory: the client chooses the key, so it
    /// also chooses whether two requests are treated as the same operation. Hashing mode
    /// derives the key from the request itself, which a sender cannot opt out of.
    pub fn use_idempotency_key_header(
        mut self,
        header_name: Option<&str>,
        require_header: bool,
    ) -> Self {
        self.require_idempotency_key = require_header;
        self.key_source = KeySource::Header(
            header_name
                .unwrap_or(DEFAULT_IDEMPOTENCY_KEY_HEADER)
                .to_string(),
        );
        self
    }

    /// Sets the name of the header added to a response to indicate it was served from the cache.
    ///
    /// The default header is `idempotency-replayed: true`. The name is case-insensitive.
    ///
    /// # Panics
    ///
    /// If `name` is not a valid header name.
    pub fn replay_header_name(mut self, name: &str) -> Self {
        self.replay_header_name = HeaderName::from_bytes(name.as_bytes())
            .unwrap_or_else(|_| panic!("`{name}` is not a valid header name"));
        self
    }

    /// When used with `ruts`'s `LayeredStore`, this sets the desired caching
    /// strategy for the idempotent response.
    ///
    /// This requires the `layered-store` feature.
    #[cfg(feature = "layered-store")]
    pub fn layered_cache_config(mut self, hot_cache_ttl_secs: i64) -> Self {
        self.layered_hot_cache_ttl_secs = Some(hot_cache_ttl_secs);
        self
    }
}

impl Default for IdempotentOptions {
    fn default() -> Self {
        let mut options = Self {
            key_source: KeySource::Hash,
            require_idempotency_key: false,
            replay_header_name: HeaderName::from_static("idempotency-replayed"),
            body_cache_ttl_secs: 60 * 5, // 5 mins default
            ignore_body: false,
            ignored_req_headers: HashSet::new(),
            ignored_header_values: HeaderMap::new(),
            ignored_res_status_codes: HashSet::new(),
            ignore_all_headers: false,
            max_body_size: 2 * 1024 * 1024,
            max_cached_response_size: 1024 * 1024,
            #[cfg(feature = "layered-store")]
            layered_hot_cache_ttl_secs: None,
        };

        let default_ignored_headers = [
            "user-agent",
            "cache-control",
            "connection",
            "cookie",
            "host",
            "pragma",
            "referer",
            "sec-fetch-dest",
            "sec-fetch-mode",
            "sec-fetch-site",
            "sec-ch-ua",
            "sec-ch-ua-mobile",
            "sec-ch-ua-platform",
        ];

        for header in default_ignored_headers {
            options
                .ignored_req_headers
                .insert(HeaderName::from_static(header));
        }

        let default_ignored_status_codes = [
            StatusCode::BAD_GATEWAY,
            StatusCode::BAD_REQUEST,
            StatusCode::FORBIDDEN,
            StatusCode::GATEWAY_TIMEOUT,
            StatusCode::LENGTH_REQUIRED,
            StatusCode::METHOD_NOT_ALLOWED,
            StatusCode::PAYLOAD_TOO_LARGE,
            StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
            StatusCode::UNPROCESSABLE_ENTITY,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            StatusCode::URI_TOO_LONG,
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::REQUEST_TIMEOUT,
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::UNAUTHORIZED,
        ];

        for status_code in default_ignored_status_codes {
            options.ignored_res_status_codes.insert(status_code);
        }

        options
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_cached(options: &IdempotentOptions, status_code: StatusCode) -> bool {
        !options.ignored_res_status_codes.contains(&status_code)
    }

    #[test]
    fn test_request_envelope_rejections_are_not_cached_by_default() {
        let options = IdempotentOptions::default();

        for status_code in [
            StatusCode::BAD_REQUEST,
            StatusCode::METHOD_NOT_ALLOWED,
            StatusCode::LENGTH_REQUIRED,
            StatusCode::PAYLOAD_TOO_LARGE,
            StatusCode::URI_TOO_LONG,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            StatusCode::UNPROCESSABLE_ENTITY,
            StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
        ] {
            assert!(!is_cached(&options, status_code), "{status_code}");
        }
    }

    #[test]
    fn test_not_found_and_conflict_are_still_cached_by_default() {
        let options = IdempotentOptions::default();

        assert!(is_cached(&options, StatusCode::NOT_FOUND));
        assert!(is_cached(&options, StatusCode::CONFLICT));
        assert!(is_cached(&options, StatusCode::OK));
        assert!(is_cached(&options, StatusCode::CREATED));
    }

    #[test]
    fn test_a_default_can_be_taken_back_off_the_list() {
        let options = IdempotentOptions::default()
            .cache_response_status_code(StatusCode::UNPROCESSABLE_ENTITY);

        assert!(is_cached(&options, StatusCode::UNPROCESSABLE_ENTITY));
    }
}
