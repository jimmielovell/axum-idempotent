use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use std::collections::HashSet;

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
    pub(crate) use_idempotency_key: bool,
    pub(crate) idempotency_key_header: String,
    pub(crate) replay_header_name: HeaderName,
    pub(crate) ignore_body: bool,
    pub(crate) ignored_req_headers: HashSet<HeaderName>,
    pub(crate) ignored_res_status_codes: HashSet<StatusCode>,
    pub(crate) ignored_header_values: HeaderMap,
    pub(crate) ignore_all_headers: bool,
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

    /// Adds a StatusCode to the list of status coded that should be ignored when
    /// determining whether to cache the response or not.
    pub fn ignore_response_status_code(mut self, status_code: StatusCode) -> Self {
        self.ignored_res_status_codes.insert(status_code);
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
    /// request body, are ignored for the purpose of the idempotency check.
    pub fn use_idempotency_key_header(mut self, header_name: Option<&str>) -> Self {
        self.ignore_all_headers = true;
        self.ignore_body = true;
        self.use_idempotency_key = true;
        header_name.map(|n| {
            self.idempotency_key_header = n.to_string();
        });
        self
    }

    /// Sets the name of the header added to a response to indicate it was served from the cache.
    ///
    /// The default header is `idempotency-replayed: true`.
    pub fn replay_header_name(mut self, name: &'static str) -> Self {
        self.replay_header_name = HeaderName::from_static(name);
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
            use_idempotency_key: false,
            idempotency_key_header: String::from("idempotency-key"),
            replay_header_name: HeaderName::from_static("idempotency-replayed"),
            body_cache_ttl_secs: 60 * 5, // 5 mins default
            ignore_body: false,
            ignored_req_headers: HashSet::new(),
            ignored_header_values: HeaderMap::new(),
            ignored_res_status_codes: HashSet::new(),
            ignore_all_headers: false,
            #[cfg(feature = "layered-store")]
            layered_hot_cache_ttl_secs: None
        };

        let default_ignored_headers = [
            "user-agent",
            "accept",
            "accept-encoding",
            "accept-language",
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
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::REQUEST_TIMEOUT,
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::UNAUTHORIZED
        ];

        for status_code in default_ignored_status_codes {
            options
                .ignored_res_status_codes
                .insert(status_code);
        }

        options
    }
}
