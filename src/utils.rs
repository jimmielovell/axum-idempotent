use crate::config::IdempotentOptions;
use axum::body::{Body, to_bytes};
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderName, StatusCode};
use axum::response::Response;
use blake3::Hasher;
use std::error::Error;
use std::str::FromStr;

pub(crate) async fn hash_request(
    mut req: Request,
    options: &IdempotentOptions,
) -> (Request, Option<String>) {
    if options.use_idempotency_key && options.ignore_body && options.ignore_all_headers {
        let value = req.headers().get(&options.idempotency_key_header);
        let value = value.and_then(|v| v.to_str().ok().map(|v| v.to_string()));
        return (req, value);
    }

    let mut hasher = Hasher::new();
    hasher.update(req.method().as_str().as_bytes());
    hasher.update(req.uri().path().as_bytes());

    if !options.ignore_all_headers {
        // Collect and sort headers for consistent ordering
        let mut headers: Vec<_> = req
            .headers()
            .iter()
            .filter(|(name, value)| {
                if options.ignored_req_headers.contains(*name) {
                    return false;
                }
                if let Some(ignored_value) = options.ignored_header_values.get(name.to_owned()) {
                    return value != ignored_value;
                }
                true
            })
            .collect();

        headers.sort_by(|(a_name, _), (b_name, _)| a_name.as_str().cmp(b_name.as_str()));

        for (name, value) in headers {
            hasher.update(name.as_str().as_bytes());
            hasher.update(value.as_bytes());
        }
    }

    if !options.ignore_body {
        let (parts, body) = req.into_parts();
        let body_bytes = to_bytes(body, usize::MAX).await.unwrap();
        hasher.update(&body_bytes);

        req = Request::from_parts(parts, Body::from(body_bytes));
    }

    (req, Some(hasher.finalize().to_string()))
}

/// Serialize
pub(crate) async fn response_to_bytes(res: Response<Body>) -> (Response, Vec<u8>) {
    let (parts, body) = res.into_parts();

    let body_bytes = to_bytes(body, usize::MAX).await.unwrap();

    let mut result = Vec::new();
    // Serialize status code
    result.extend_from_slice(&parts.status.as_u16().to_be_bytes());

    let headers = parts.headers.clone();
    let len = headers.len();
    for (i, (name, value)) in headers.iter().enumerate() {
        result.extend_from_slice(name.as_str().as_bytes());
        result.extend_from_slice(b": ");
        result.extend_from_slice(value.as_bytes());

        if i < len - 1 {
            result.extend_from_slice(b"\r\n");
        }
    }

    // headers/body separator (double CRLF)
    result.extend_from_slice(b"\r\n\r\n");
    result.extend_from_slice(&body_bytes);

    (Response::from_parts(parts, Body::from(body_bytes)), result)
}

/// Deserialize bytes back into a `axum::response::Response`.
pub(crate) fn bytes_to_response(bytes: Vec<u8>) -> Result<Response, Box<dyn Error + Send + Sync>> {
    // Split the bytes into status code, headers, and body
    let status_code_bytes = &bytes[0..2];
    let status_code = u16::from_be_bytes([status_code_bytes[0], status_code_bytes[1]]);
    let status_code = StatusCode::from_u16(status_code)?;

    // End of headers (double CRLF: \r\n\r\n)
    let header_end = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or("Invalid header format: missing double CRLF")?;

    let header_bytes = &bytes[2..header_end];
    let headers = parse_headers(header_bytes)?;

    // Skip both CRLFs after the header section (skip header_end + 4)
    let body_bytes = &bytes[(header_end + 4)..];
    let body = Body::from(body_bytes.to_vec());

    let mut response = Response::new(body);
    *response.status_mut() = status_code;
    *response.headers_mut() = headers;

    Ok(response)
}

/// Parse headers from bytes.
fn parse_headers(header_bytes: &[u8]) -> Result<HeaderMap, Box<dyn Error + Send + Sync>> {
    let mut headers = HeaderMap::new();
    let header_str = std::str::from_utf8(header_bytes)?;

    for line in header_str.split("\r\n") {
        if line.is_empty() {
            continue;
        }

        let parts: Vec<&str> = line.splitn(2, ": ").collect();
        if parts.len() != 2 {
            return Err("Invalid header format".into());
        }

        let name = parts[0];
        let value = parts[1];
        headers.insert(HeaderName::from_str(name)?, value.parse()?);
    }

    Ok(headers)
}

#[cfg(test)]
mod tests {
    use super::*;
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

        let (new_req, hash) = hash_request(req, &IdempotentOptions::default()).await;

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

        let (_, hash2) = hash_request(req2, &IdempotentOptions::default()).await;
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

        let (_, hash3) = hash_request(req3, &IdempotentOptions::default()).await;
        assert_ne!(hash, hash3, "Different body should produce different hash");
    }

    #[tokio::test]
    async fn test_response_to_bytes() {
        // Create a response with known values
        let response = Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "text/plain")
            .header("X-Custom", "test-value")
            .body(Body::from("test response body"))
            .unwrap();

        let (_new_res, bytes) = response_to_bytes(response).await;

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

        let (_new_res, bytes) = response_to_bytes(response).await;
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

            let (_, bytes) = response_to_bytes(response).await;
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

        let (_, bytes) = response_to_bytes(response).await;
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
            .body(Body::empty())
            .unwrap();

        let (_, bytes) = response_to_bytes(response).await;

        // Skip status code (2 bytes)
        let headers_and_body = &bytes[2..];
        let headers_str = std::str::from_utf8(headers_and_body).unwrap();

        // The header names are being normalized to lowercase by the http crate
        // Headers should be:
        // first: 1\r\n
        // second: 2\r\n
        // \r\n
        assert_eq!(
            headers_str, "first: 1\r\nsecond: 2\r\n\r\n",
            "Headers should be properly formatted with correct CRLF sequences"
        );
    }
}
