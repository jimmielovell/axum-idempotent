use crate::config::{CacheKeySource, IdempotentOptions};
use axum::body::{Body, HttpBody, to_bytes};
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::Response;
use blake3::Hasher;
use std::error::Error;

/// Prefix applied to every session field this middleware writes.
const SESSION_FIELD_PREFIX: &str = "idem:";

/// Upper bound on an accepted client-supplied idempotency key.
const MAX_IDEMPOTENCY_KEY_LEN: usize = 255;

/// Layout of a cached response:
///
/// ```text
/// u8            format version
/// u16           status code
/// u32           header count
/// per header:   u32 name length, name, u32 value length, value
/// body:         u32 length, body
/// ```
///
/// Header names and values are length-prefixed rather than text-delimited so that
/// repeated names survive and values are not required to be UTF-8.
const CACHE_FORMAT_VERSION: u8 = 1;

const VERSION_LEN: usize = 1;
const STATUS_LEN: usize = 2;
const COUNT_LEN: usize = 4;
const LENGTH_LEN: usize = 4;

/// Feeds one length-prefixed field into the hash.
fn hash_field(hasher: &mut Hasher, field: &[u8]) {
    hasher.update(&(field.len() as u64).to_be_bytes());
    hasher.update(field);
}

fn is_usable_idempotency_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= MAX_IDEMPOTENCY_KEY_LEN
        && key.bytes().all(|byte| byte.is_ascii_graphic())
}

fn bad_request(message: String) -> Response {
    let mut res = Response::new(Body::from(message));
    *res.status_mut() = StatusCode::BAD_REQUEST;

    res
}

/// Builds the response for a request body that could not be buffered.
fn body_rejection(err: &axum::Error) -> Response {
    let over_limit =
        Error::source(err).is_some_and(|source| source.is::<http_body_util::LengthLimitError>());

    let mut res = Response::new(Body::empty());
    *res.status_mut() = if over_limit {
        StatusCode::PAYLOAD_TOO_LARGE
    } else {
        StatusCode::BAD_REQUEST
    };

    res
}

/// Computes the cache key for a request.
///
/// The rejection is boxed because it is the rare path, while every request pays for the
/// size of the `Result`.
pub(crate) async fn hash_request(
    mut req: Request,
    options: &IdempotentOptions,
) -> Result<(Request, Option<String>), Box<Response>> {
    if let CacheKeySource::Header(header_name) = &options.key_source {
        // Absence is ambiguous: plenty of requests to a layer applied router-wide are
        // not meant to be idempotent.
        let Some(value) = req.headers().get(header_name) else {
            return if options.require_idempotency_key {
                Err(Box::new(bad_request(format!(
                    "Missing `{header_name}` header"
                ))))
            } else {
                Ok((req, None))
            };
        };

        // A key that is present but unusable is not ambiguous. The client asked for
        // idempotency, and forwarding the request would withhold it without saying so.
        let Some(key) = value
            .to_str()
            .ok()
            .filter(|key| is_usable_idempotency_key(key))
        else {
            return Err(Box::new(bad_request(format!(
                "`{header_name}` must be 1-{MAX_IDEMPOTENCY_KEY_LEN} printable ascii characters"
            ))));
        };

        let key = format!("{SESSION_FIELD_PREFIX}{key}");
        return Ok((req, Some(key)));
    }

    let mut hasher = Hasher::new();
    hash_field(&mut hasher, req.method().as_str().as_bytes());

    let uri = req.uri();
    let target = uri.path_and_query().map_or(uri.path(), |pq| pq.as_str());
    hash_field(&mut hasher, target.as_bytes());

    if !options.ignore_all_headers {
        // Collect and sort headers for consistent ordering
        let mut headers: Vec<_> = req
            .headers()
            .iter()
            .filter(|(name, value)| {
                if options.ignored_req_headers.contains(*name) {
                    return false;
                }
                if let Some(ignored_value) = options.ignored_header_values.get(*name) {
                    return value != ignored_value;
                }
                true
            })
            .collect();

        headers.sort_by(|(a_name, _), (b_name, _)| a_name.as_str().cmp(b_name.as_str()));

        for (name, value) in headers {
            hash_field(&mut hasher, name.as_str().as_bytes());
            hash_field(&mut hasher, value.as_bytes());
        }
    }

    if !options.ignore_body {
        let (parts, body) = req.into_parts();

        // A body already known to be over the limit is forwarded untouched
        if body
            .size_hint()
            .upper()
            .is_some_and(|size| size > options.max_body_size as u64)
        {
            return Ok((Request::from_parts(parts, body), None));
        }

        let body_bytes = match to_bytes(body, options.max_body_size).await {
            Ok(bytes) => bytes,
            Err(err) => return Err(Box::new(body_rejection(&err))),
        };

        hash_field(&mut hasher, &body_bytes);
        req = Request::from_parts(parts, Body::from(body_bytes));
    }

    Ok((
        req,
        Some(format!("{}{}", SESSION_FIELD_PREFIX, hasher.finalize())),
    ))
}

/// Serializes a response for caching, returning it alongside the bytes to store.
///
/// `None` means the response should not be cached, either because its length is not
/// known before reading it or because it is over `max_size`.
pub(crate) async fn response_to_bytes(
    res: Response<Body>,
    max_size: usize,
) -> (Response, Option<Vec<u8>>) {
    let (parts, body) = res.into_parts();

    // Only a response of known length is read. Reading a stream here would withhold it
    // from the client until it ended, which for an open-ended stream never happens.
    if !body
        .size_hint()
        .upper()
        .is_some_and(|size| size <= max_size as u64)
    {
        return (Response::from_parts(parts, body), None);
    }

    let body_bytes = match to_bytes(body, max_size).await {
        Ok(bytes) => bytes,
        // The body is partially consumed, so there is nothing left to send.
        Err(err) => {
            tracing::error!("Failed to read response body for caching: {err:?}");

            let mut res = Response::new(Body::empty());
            *res.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
            return (res, None);
        }
    };

    let headers = &parts.headers;
    let mut result = Vec::with_capacity(
        VERSION_LEN
            + STATUS_LEN
            + COUNT_LEN
            + headers
                .iter()
                .map(|(name, value)| 2 * LENGTH_LEN + name.as_str().len() + value.len())
                .sum::<usize>()
            + LENGTH_LEN
            + body_bytes.len(),
    );

    result.push(CACHE_FORMAT_VERSION);
    result.extend_from_slice(&parts.status.as_u16().to_be_bytes());
    result.extend_from_slice(&(headers.len() as u32).to_be_bytes());

    for (name, value) in headers {
        write_field(&mut result, name.as_str().as_bytes());
        write_field(&mut result, value.as_bytes());
    }

    write_field(&mut result, &body_bytes);

    (
        Response::from_parts(parts, Body::from(body_bytes)),
        Some(result),
    )
}

fn write_field(out: &mut Vec<u8>, field: &[u8]) {
    out.extend_from_slice(&(field.len() as u32).to_be_bytes());
    out.extend_from_slice(field);
}

/// Reads a cached response
struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8], Box<dyn Error + Send + Sync>> {
        let end = self
            .pos
            .checked_add(len)
            .ok_or("Cached response length overflowed")?;
        let field = self
            .bytes
            .get(self.pos..end)
            .ok_or("Cached response ended early")?;

        self.pos = end;
        Ok(field)
    }

    fn take_u8(&mut self) -> Result<u8, Box<dyn Error + Send + Sync>> {
        Ok(self.take(VERSION_LEN)?[0])
    }

    fn take_u16(&mut self) -> Result<u16, Box<dyn Error + Send + Sync>> {
        let bytes = self.take(STATUS_LEN)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn take_u32(&mut self) -> Result<u32, Box<dyn Error + Send + Sync>> {
        let bytes = self.take(LENGTH_LEN)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// Reads one length-prefixed field.
    fn take_field(&mut self) -> Result<&'a [u8], Box<dyn Error + Send + Sync>> {
        let len = self.take_u32()? as usize;
        self.take(len)
    }
}

/// Deserialize bytes back into a `axum::response::Response`.
pub(crate) fn bytes_to_response(bytes: Vec<u8>) -> Result<Response, Box<dyn Error + Send + Sync>> {
    let mut reader = Reader {
        bytes: &bytes,
        pos: 0,
    };

    let version = reader.take_u8()?;
    if version != CACHE_FORMAT_VERSION {
        return Err(format!("Unsupported cached response format: {version}").into());
    }

    let status_code = StatusCode::from_u16(reader.take_u16()?)?;

    let header_count = reader.take_u32()?;
    let mut headers = HeaderMap::new();

    for _ in 0..header_count {
        let name = HeaderName::from_bytes(reader.take_field()?)?;
        let value = HeaderValue::from_bytes(reader.take_field()?)?;

        headers.append(name, value);
    }

    let mut response = Response::new(Body::from(reader.take_field()?.to_vec()));
    *response.status_mut() = status_code;
    *response.headers_mut() = headers;

    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Bytes;
    use axum::http::{Method, StatusCode};
    use std::default::Default;

    #[tokio::test]
    async fn test_hash_request() {
        // Create a request with a known body
        let req = Request::builder()
            .method(Method::POST)
            .uri("/test/endpoint")
            .body(Body::from("test body"))
            .unwrap();

        let (new_req, hash) = hash_request(req, &IdempotentOptions::default())
            .await
            .unwrap();

        // Verify the new request matches original
        assert_eq!(new_req.method(), Method::POST);
        assert_eq!(new_req.uri().path(), "/test/endpoint");

        // Verify body is preserved
        let body_bytes = to_bytes(new_req.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body_bytes[..], b"test body");

        // Verify hash is deterministic
        let req2 = Request::builder()
            .method(Method::POST)
            .uri("/test/endpoint")
            .body(Body::from("test body"))
            .unwrap();

        let (_, hash2) = hash_request(req2, &IdempotentOptions::default())
            .await
            .unwrap();
        assert_eq!(
            hash, hash2,
            "Hash should be deterministic for identical requests"
        );

        // Verify different body produces different hash
        let req3 = Request::builder()
            .method(Method::POST)
            .uri("/test/endpoint")
            .body(Body::from("different body"))
            .unwrap();

        let (_, hash3) = hash_request(req3, &IdempotentOptions::default())
            .await
            .unwrap();
        assert_ne!(hash, hash3, "Different body should produce different hash");
    }

    async fn hashed_key(uri: &str, headers: &[(&str, &str)]) -> Option<String> {
        let mut req = Request::builder()
            .method(Method::POST)
            .uri(uri)
            .body(Body::empty())
            .unwrap();

        for (name, value) in headers {
            req.headers_mut().insert(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }

        hash_request(req, &IdempotentOptions::default())
            .await
            .unwrap()
            .1
    }

    #[tokio::test]
    async fn test_query_string_is_part_of_the_key() {
        assert_ne!(
            hashed_key("/search?q=alice", &[]).await,
            hashed_key("/search?q=bob", &[]).await,
            "different queries are different requests"
        );
        assert_ne!(
            hashed_key("/search?q=alice", &[]).await,
            hashed_key("/search", &[]).await,
        );
        assert_eq!(
            hashed_key("/search?q=alice", &[]).await,
            hashed_key("/search?q=alice", &[]).await,
        );
    }

    /// Undelimited fields let one request's headers imitate another's.
    #[tokio::test]
    async fn test_header_boundaries_are_unambiguous() {
        assert_ne!(
            hashed_key("/test", &[("x-a", "bc")]).await,
            hashed_key("/test", &[("x-ab", "c")]).await,
        );
    }

    fn direct_key_options() -> IdempotentOptions {
        IdempotentOptions::default().use_idempotency_key_header(Some("idempotency-key"), true)
    }

    async fn key_for(
        header_value: &[u8],
        options: &IdempotentOptions,
    ) -> Result<Option<String>, Box<Response>> {
        let mut req = Request::builder()
            .method(Method::POST)
            .uri("/test")
            .body(Body::empty())
            .unwrap();

        if let Ok(value) = HeaderValue::from_bytes(header_value) {
            req.headers_mut().insert("idempotency-key", value);
        }

        hash_request(req, options).await.map(|(_, key)| key)
    }

    #[tokio::test]
    async fn test_hashed_key_is_namespaced() {
        let (_, key) = hash_request(
            Request::builder()
                .method(Method::POST)
                .uri("/test")
                .body(Body::from("body"))
                .unwrap(),
            &IdempotentOptions::default(),
        )
        .await
        .unwrap();

        let key = key.unwrap();
        assert!(key.starts_with(SESSION_FIELD_PREFIX));
        // blake3 hex digest following the prefix
        assert_eq!(key.len(), SESSION_FIELD_PREFIX.len() + 64);
    }

    #[tokio::test]
    async fn test_direct_key_is_namespaced() {
        let options = direct_key_options();
        assert_eq!(
            key_for(b"key-1", &options).await.unwrap(),
            Some(String::from("idem:key-1"))
        );
    }

    /// A key the client meant to use but got wrong is an error, not a silent downgrade.
    #[tokio::test]
    async fn test_unusable_direct_keys_are_rejected() {
        let options = direct_key_options();

        for (key, description) in [
            (b"".as_slice(), "empty key"),
            (b"   ", "whitespace-only key"),
            (b"has space", "embedded space"),
            (&[0xFF, 0xFE], "non-utf8 key"),
            ("naïve".as_bytes(), "non-ascii key"),
            (&[b'k'; MAX_IDEMPOTENCY_KEY_LEN + 1], "over-long key"),
        ] {
            let res = key_for(key, &options)
                .await
                .err()
                .unwrap_or_else(|| panic!("{description} should be rejected"));
            assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{description}");
        }

        assert!(
            key_for(&[b'k'; MAX_IDEMPOTENCY_KEY_LEN], &options)
                .await
                .unwrap()
                .is_some(),
            "a key at the length limit is accepted"
        );
    }

    /// Content negotiation headers select what the handler returns, so they belong in the key.
    #[tokio::test]
    async fn test_negotiation_headers_are_part_of_the_key() {
        for header in ["accept", "accept-encoding", "accept-language"] {
            assert_ne!(
                hashed_key("/test", &[(header, "a")]).await,
                hashed_key("/test", &[(header, "b")]).await,
                "{header} should affect the key"
            );
        }
    }

    /// Setting the mode used to be three flags that an unrelated option could undo.
    #[tokio::test]
    async fn test_direct_key_mode_survives_later_options() {
        let options = direct_key_options().ignore_body(false).ignore_all_headers();

        assert_eq!(
            key_for(b"key-1", &options).await.unwrap(),
            Some(String::from("idem:key-1")),
            "the key should still come from the header"
        );
    }

    #[tokio::test]
    async fn test_required_key_rejects_a_request_without_one() {
        let options = direct_key_options();

        let req = Request::builder()
            .method(Method::POST)
            .uri("/test")
            .body(Body::empty())
            .unwrap();

        let res = hash_request(req, &options)
            .await
            .expect_err("should be rejected");
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_required_key_accepts_a_usable_one() {
        let options = direct_key_options();

        assert_eq!(
            key_for(b"key-1", &options).await.unwrap(),
            Some(String::from("idem:key-1"))
        );
    }

    #[test]
    fn test_replay_header_name_accepts_any_case() {
        let options = IdempotentOptions::default().replay_header_name("Idempotency-Replayed");
        assert_eq!(options.replay_header_name.as_str(), "idempotency-replayed");
    }

    #[tokio::test]
    async fn test_missing_direct_key_disables_caching() {
        let options =
            IdempotentOptions::default().use_idempotency_key_header(Some("idempotency-key"), false);

        let (_, key) = hash_request(
            Request::builder()
                .method(Method::POST)
                .uri("/test")
                .body(Body::empty())
                .unwrap(),
            &options,
        )
        .await
        .unwrap();

        assert_eq!(key, None);
    }

    /// A body with no `content-length`, so its size is not known before reading it.
    fn chunked_body(chunks: Vec<Result<Bytes, std::io::Error>>) -> Body {
        let body = Body::from_stream(futures_util::stream::iter(chunks));
        assert_eq!(
            body.size_hint().upper(),
            None,
            "test body should have an unknown length"
        );
        body
    }

    fn post_with_body(body: Body) -> Request {
        Request::builder()
            .method(Method::POST)
            .uri("/test")
            .body(body)
            .unwrap()
    }

    #[tokio::test]
    async fn test_body_at_the_limit_is_hashed() {
        let options = IdempotentOptions::default().max_body_size(64);
        let req = post_with_body(Body::from(vec![b'x'; 64]));

        let (req, key) = hash_request(req, &options).await.unwrap();

        assert!(key.is_some());
        let body = to_bytes(req.into_body(), usize::MAX).await.unwrap();
        assert_eq!(body.len(), 64);
    }

    /// Over the limit but with a known length: nothing is buffered and the request
    /// reaches the handler intact, just without idempotency handling.
    #[tokio::test]
    async fn test_oversized_body_with_known_length_is_passed_through() {
        let options = IdempotentOptions::default().max_body_size(64);
        let req = post_with_body(Body::from(vec![b'x'; 65]));

        let (req, key) = hash_request(req, &options).await.unwrap();

        assert_eq!(key, None, "an oversized body must not be cached");
        let body = to_bytes(req.into_body(), usize::MAX).await.unwrap();
        assert_eq!(
            body.len(),
            65,
            "the body must still be readable by the handler"
        );
    }

    /// Over the limit with an unknown length: the body is consumed before the limit
    /// is known to be exceeded, so it can no longer be handed to the handler.
    #[tokio::test]
    async fn test_oversized_body_with_unknown_length_is_rejected() {
        let options = IdempotentOptions::default().max_body_size(64);
        let req = post_with_body(chunked_body(vec![
            Ok(Bytes::from(vec![b'x'; 40])),
            Ok(Bytes::from(vec![b'x'; 40])),
        ]));

        let res = hash_request(req, &options)
            .await
            .expect_err("should be rejected");

        assert_eq!(res.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    /// A client disconnecting mid-upload
    #[tokio::test]
    async fn test_body_stream_error_is_rejected() {
        let options = IdempotentOptions::default();
        let req = post_with_body(chunked_body(vec![
            Ok(Bytes::from("partial")),
            Err(std::io::Error::other("connection reset")),
        ]));

        let res = hash_request(req, &options)
            .await
            .expect_err("should be rejected");

        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_ignored_body_is_not_size_limited() {
        let options = IdempotentOptions::default()
            .ignore_body(true)
            .max_body_size(8);
        let req = post_with_body(Body::from(vec![b'x'; 4096]));

        let (_, key) = hash_request(req, &options).await.unwrap();

        assert!(key.is_some());
    }

    /// Serializes a response that is expected to be cacheable.
    async fn serialize(res: Response) -> (Response, Vec<u8>) {
        let (res, bytes) = response_to_bytes(res, usize::MAX).await;
        (res, bytes.expect("response should be cacheable"))
    }

    #[tokio::test]
    async fn test_oversized_response_is_not_cached() {
        let response = Response::new(Body::from(vec![b'x'; 65]));

        let (res, bytes) = response_to_bytes(response, 64).await;

        assert!(bytes.is_none(), "an oversized response must not be cached");
        let body = to_bytes(res.into_body(), usize::MAX).await.unwrap();
        assert_eq!(body.len(), 65, "the client must still get the response");
    }

    #[tokio::test]
    async fn test_response_at_the_limit_is_cached() {
        let response = Response::new(Body::from(vec![b'x'; 64]));

        let (_, bytes) = response_to_bytes(response, 64).await;

        assert!(bytes.is_some());
    }

    /// Reading a stream to cache it would hold the response until the stream ended.
    #[tokio::test]
    async fn test_streaming_response_is_not_cached() {
        let response = Response::new(chunked_body(vec![Ok(Bytes::from("streamed"))]));

        let (res, bytes) = response_to_bytes(response, usize::MAX).await;

        assert!(bytes.is_none(), "a stream must not be cached");
        let body = to_bytes(res.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body[..], b"streamed");
    }

    #[tokio::test]
    async fn test_response_to_bytes() {
        let response = Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "text/plain")
            .header("X-Custom", "test-value")
            .body(Body::from("test response body"))
            .unwrap();

        let (_new_res, bytes) = serialize(response).await;

        // Test the serialized response can be deserialized back
        let reconstructed = bytes_to_response(bytes).unwrap();

        // Verify status code
        assert_eq!(reconstructed.status(), StatusCode::OK);

        // Verify headers
        assert_eq!(
            reconstructed.headers().get("Content-Type").unwrap(),
            "text/plain"
        );
        assert_eq!(
            reconstructed.headers().get("X-Custom").unwrap(),
            "test-value"
        );

        // Verify body
        let body_bytes = to_bytes(reconstructed.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body_bytes[..], b"test response body");
    }

    #[tokio::test]
    async fn test_response_to_bytes_with_empty_body() {
        let response = Response::builder()
            .status(StatusCode::NO_CONTENT)
            .body(Body::empty())
            .unwrap();

        let (_new_res, bytes) = serialize(response).await;
        let reconstructed = bytes_to_response(bytes).unwrap();

        assert_eq!(reconstructed.status(), StatusCode::NO_CONTENT);
        let body_bytes = to_bytes(reconstructed.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(body_bytes.is_empty());
    }

    #[tokio::test]
    async fn test_different_status_codes() {
        for status in [
            StatusCode::OK,
            StatusCode::CREATED,
            StatusCode::ACCEPTED,
            StatusCode::NO_CONTENT,
            StatusCode::BAD_REQUEST,
            StatusCode::NOT_FOUND,
            StatusCode::INTERNAL_SERVER_ERROR,
        ] {
            let response = Response::builder()
                .status(status)
                .body(Body::empty())
                .unwrap();

            let (_, bytes) = serialize(response).await;
            let reconstructed = bytes_to_response(bytes).unwrap();
            assert_eq!(reconstructed.status(), status);
        }
    }

    #[tokio::test]
    async fn test_body_bytes_preservation() {
        let original_body = "test response body";
        let response = Response::builder()
            .status(StatusCode::OK)
            .body(Body::from(original_body))
            .unwrap();

        let (_, bytes) = serialize(response).await;
        let reconstructed = bytes_to_response(bytes).unwrap();

        let body_bytes = to_bytes(reconstructed.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body_bytes[..], original_body.as_bytes());
    }

    #[tokio::test]
    async fn test_header_serialization_format() {
        let response = Response::builder()
            .status(StatusCode::OK)
            .header("First", "1")
            .header("Second", "2")
            .body(Body::from("body"))
            .unwrap();

        let (_, bytes) = serialize(response).await;

        // Header names are normalized to lowercase by the http crate.
        let mut expected = vec![CACHE_FORMAT_VERSION];
        expected.extend_from_slice(&200u16.to_be_bytes());
        expected.extend_from_slice(&2u32.to_be_bytes());
        for (name, value) in [("first", "1"), ("second", "2")] {
            expected.extend_from_slice(&(name.len() as u32).to_be_bytes());
            expected.extend_from_slice(name.as_bytes());
            expected.extend_from_slice(&(value.len() as u32).to_be_bytes());
            expected.extend_from_slice(value.as_bytes());
        }
        expected.extend_from_slice(&4u32.to_be_bytes());
        expected.extend_from_slice(b"body");

        assert_eq!(bytes, expected);
    }

    /// Collapsing these to one value silently drops cookies on every replay.
    #[tokio::test]
    async fn test_repeated_header_names_survive() {
        let response = Response::builder()
            .status(StatusCode::OK)
            .header("set-cookie", "a=1")
            .header("set-cookie", "b=2")
            .body(Body::empty())
            .unwrap();

        let (_, bytes) = serialize(response).await;
        let reconstructed = bytes_to_response(bytes).unwrap();

        let cookies: Vec<_> = reconstructed
            .headers()
            .get_all("set-cookie")
            .iter()
            .collect();
        assert_eq!(cookies, ["a=1", "b=2"]);
    }

    /// `HeaderValue` permits obs-text, so a cached response must not assume UTF-8.
    #[tokio::test]
    async fn test_non_utf8_header_value_survives() {
        let value = HeaderValue::from_bytes(b"attachment; filename=\"caf\xE9.pdf\"").unwrap();
        let response = Response::builder()
            .status(StatusCode::OK)
            .header("content-disposition", value.clone())
            .body(Body::empty())
            .unwrap();

        let (_, bytes) = serialize(response).await;
        let reconstructed = bytes_to_response(bytes).unwrap();

        assert_eq!(
            reconstructed.headers().get("content-disposition"),
            Some(&value)
        );
    }

    #[tokio::test]
    async fn test_truncated_cached_response_is_an_error() {
        let response = Response::builder()
            .status(StatusCode::OK)
            .header("x-test", "value")
            .body(Body::from("body"))
            .unwrap();

        let (_, bytes) = serialize(response).await;

        for len in 0..bytes.len() - 1 {
            assert!(
                bytes_to_response(bytes[..len].to_vec()).is_err(),
                "a cached response truncated to {len} bytes should not decode"
            );
        }
    }

    #[test]
    fn test_unknown_format_version_is_an_error() {
        let mut bytes = vec![CACHE_FORMAT_VERSION + 1];
        bytes.extend_from_slice(&200u16.to_be_bytes());
        bytes.extend_from_slice(&0u32.to_be_bytes());

        assert!(bytes_to_response(bytes).is_err());
    }
}
