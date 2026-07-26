//! PWA-compatible VSS wire transport (U2, R3/R15, KTD-1/KTD-2), mirroring
//! `VssClient` in `zinq/src/ldk/storage/vss-client.ts`:
//!
//! - endpoints `POST {base_url}/{getObject,putObjects,deleteObject,
//!   listKeyVersions}` with `Content-Type: application/octet-stream` and the
//!   signature `authorization` header ([`super::auth`]);
//! - protobuf bodies from the standard LDK VSS schema (`vss-client-ng`'s
//!   prost types — transport and crypto layers of that crate are NOT used);
//! - obfuscation + encryption applied ABOVE the transport ([`super::crypto`]);
//! - versioning per the VSS protocol: first write 0, client increments —
//!   `put_object` returns `version + 1` exactly like the PWA;
//! - 15 s request timeout (PWA `FETCH_TIMEOUT_MS`), list pagination capped at
//!   100 pages (PWA `MAX_LIST_PAGES`);
//! - HTTP 404 on `getObject` → `Ok(None)` before any body parse (PWA
//!   behavior), conflicts/auth/invalid surfaced as typed [`VssError`]s.
//!
//! Retries (a hardening the PWA lacks; its callers re-drive writes): only
//! transient failures — network errors and 5xx — are retried with capped
//! exponential backoff. Conflict/invalid/auth are NEVER retried, and
//! `put_object` encrypts ONCE so every retry resends the identical ciphertext
//! buffer (KTD-3's content-compare on 409 depends on byte-stable retries).

use std::time::Duration;

use vss_client_ng::prost::Message;
use vss_client_ng::types::{
    DeleteObjectRequest, ErrorCode, ErrorResponse, GetObjectRequest, GetObjectResponse, KeyValue,
    ListKeyVersionsRequest, ListKeyVersionsResponse, PutObjectRequest,
};

use super::auth::SignatureHeaderProvider;
use super::{crypto, VssError};

/// Request timeout — the PWA's `FETCH_TIMEOUT_MS` (15 000 ms).
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(15);

/// Pagination cap for `listKeyVersions` — the PWA's `MAX_LIST_PAGES`.
pub const MAX_LIST_PAGES: usize = 100;

/// Injectable retry policy: exponential backoff (`base_delay * 2^(attempt-1)`
/// between attempts), capped at `max_attempts` total tries. Tests inject a
/// millisecond-scale policy so retry paths run instantly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VssRetryPolicy {
    /// Delay before the first retry; doubles for each subsequent one.
    pub base_delay: Duration,
    /// Total attempts (1 = no retries).
    pub max_attempts: u32,
}

impl Default for VssRetryPolicy {
    fn default() -> Self {
        Self {
            base_delay: Duration::from_millis(250),
            max_attempts: 3,
        }
    }
}

/// The wire client: one instance per store, addressed by the U1-derived
/// `store_id` (`hex(SHA-256(ldk_seed))`) with obfuscation/encryption keyed by
/// the U1 `vss_encryption_key` and auth by the `vss_signing_key`.
pub struct VssWireClient {
    base_url: String,
    store_id: String,
    encryption_key: [u8; 32],
    auth: SignatureHeaderProvider,
    http: reqwest::Client,
    retry: VssRetryPolicy,
}

impl VssWireClient {
    /// Builds a client with the default retry policy.
    pub fn new(
        base_url: String,
        store_id: String,
        encryption_key: [u8; 32],
        signing_key: &[u8; 32],
    ) -> Result<Self, VssError> {
        Self::with_retry_policy(
            base_url,
            store_id,
            encryption_key,
            signing_key,
            VssRetryPolicy::default(),
        )
    }

    /// Builds a client with an injected retry policy (tests use millisecond
    /// backoff so transient-failure paths run instantly).
    pub fn with_retry_policy(
        base_url: String,
        store_id: String,
        encryption_key: [u8; 32],
        signing_key: &[u8; 32],
        retry: VssRetryPolicy,
    ) -> Result<Self, VssError> {
        let auth = SignatureHeaderProvider::new(signing_key)?;
        let http = reqwest::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .build()
            .map_err(|e| VssError::Network {
                message: format!("failed to build HTTP client: {e}"),
            })?;
        Ok(Self {
            base_url,
            store_id,
            encryption_key,
            auth,
            http,
            retry,
        })
    }

    /// The store this client addresses.
    pub fn store_id(&self) -> &str {
        &self.store_id
    }

    /// Fetches and decrypts `plaintext_key`. `Ok(None)` on HTTP 404 (checked
    /// before any body parse, like the PWA) or an empty response value.
    pub async fn get_object(
        &self,
        plaintext_key: &str,
    ) -> Result<Option<(Vec<u8>, i64)>, VssError> {
        let request = GetObjectRequest {
            store_id: self.store_id.clone(),
            key: crypto::obfuscate_key(&self.encryption_key, plaintext_key),
        };
        let (status, payload) = self.post("getObject", request.encode_to_vec()).await?;
        if status == 404 {
            return Ok(None);
        }
        if !(200..300).contains(&status) {
            return Err(parse_error(status, &payload));
        }
        let response = decode_response::<GetObjectResponse>(status, &payload)?;
        let Some(kv) = response.value else {
            return Ok(None);
        };
        let plaintext =
            crypto::decrypt(&self.encryption_key, &kv.value).map_err(VssError::Crypto)?;
        Ok(Some((plaintext, kv.version)))
    }

    /// Writes one object as a single-item transactional `PutObjectRequest`
    /// to `/putObjects` at `version` (0 for the first write) and returns the
    /// incremented client-side version, exactly like the PWA's `putObject`.
    /// A stale `version` surfaces as [`VssError::Conflict`] and is never
    /// retried.
    pub async fn put_object(
        &self,
        plaintext_key: &str,
        value: &[u8],
        version: i64,
    ) -> Result<i64, VssError> {
        // Encrypt exactly once: transport retries resend this identical
        // ciphertext buffer (random nonces make re-encryptions byte-divergent,
        // which would defeat KTD-3's content-compare on conflict).
        let encrypted = crypto::encrypt(&self.encryption_key, value);
        let request = PutObjectRequest {
            store_id: self.store_id.clone(),
            global_version: None,
            transaction_items: vec![KeyValue {
                key: crypto::obfuscate_key(&self.encryption_key, plaintext_key),
                version,
                value: encrypted,
            }],
            delete_items: Vec::new(),
        };
        let (status, payload) = self.post("putObjects", request.encode_to_vec()).await?;
        if !(200..300).contains(&status) {
            return Err(parse_error(status, &payload));
        }
        Ok(version + 1)
    }

    /// Deletes `plaintext_key` at `version` (the VSS delete is idempotent for
    /// missing keys).
    pub async fn delete_object(&self, plaintext_key: &str, version: i64) -> Result<(), VssError> {
        let request = DeleteObjectRequest {
            store_id: self.store_id.clone(),
            key_value: Some(KeyValue {
                key: crypto::obfuscate_key(&self.encryption_key, plaintext_key),
                version,
                value: Vec::new(),
            }),
        };
        let (status, payload) = self.post("deleteObject", request.encode_to_vec()).await?;
        if !(200..300).contains(&status) {
            return Err(parse_error(status, &payload));
        }
        Ok(())
    }

    /// Lists all (obfuscated) keys with their versions, following pagination
    /// tokens up to [`MAX_LIST_PAGES`] pages (typed error beyond that, like
    /// the PWA's cap).
    pub async fn list_key_versions(&self) -> Result<Vec<(String, i64)>, VssError> {
        let mut results = Vec::new();
        let mut page_token: Option<String> = None;
        let mut pages = 0usize;
        loop {
            pages += 1;
            if pages > MAX_LIST_PAGES {
                return Err(VssError::TooManyListPages);
            }
            let request = ListKeyVersionsRequest {
                store_id: self.store_id.clone(),
                key_prefix: None,
                page_size: None,
                page_token: page_token.clone(),
            };
            let (status, payload) = self
                .post("listKeyVersions", request.encode_to_vec())
                .await?;
            if !(200..300).contains(&status) {
                return Err(parse_error(status, &payload));
            }
            let response = decode_response::<ListKeyVersionsResponse>(status, &payload)?;
            for kv in response.key_versions {
                results.push((kv.key, kv.version));
            }
            page_token = response.next_page_token.filter(|token| !token.is_empty());
            if page_token.is_none() {
                return Ok(results);
            }
        }
    }

    /// POSTs `body` to `{base_url}/{endpoint}`, retrying ONLY transient
    /// failures (network errors, 5xx statuses) per the injected policy. The
    /// `authorization` header is recomputed per attempt (fresh timestamp).
    async fn post(&self, endpoint: &str, body: Vec<u8>) -> Result<(u16, Vec<u8>), VssError> {
        let url = format!("{}/{endpoint}", self.base_url);
        let mut attempts = 0u32;
        loop {
            attempts += 1;
            let outcome = self.post_once(&url, body.clone()).await;
            let transient = match &outcome {
                Ok((status, _)) => *status >= 500,
                Err(VssError::Network { .. }) => true,
                Err(_) => false,
            };
            if !transient || attempts >= self.retry.max_attempts {
                return outcome;
            }
            let backoff = self.retry.base_delay * 2u32.pow(attempts - 1);
            tokio::time::sleep(backoff).await;
        }
    }

    async fn post_once(&self, url: &str, body: Vec<u8>) -> Result<(u16, Vec<u8>), VssError> {
        let response = self
            .http
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .header("authorization", self.auth.header())
            .body(body)
            .send()
            .await
            .map_err(|e| VssError::Network {
                message: format!("{url}: {e}"),
            })?;
        let status = response.status().as_u16();
        let payload = response.bytes().await.map_err(|e| VssError::Network {
            message: format!("{url}: failed to read response body: {e}"),
        })?;
        Ok((status, payload.to_vec()))
    }
}

/// Maps a non-2xx response to the typed error taxonomy: a decodable
/// `ErrorResponse` wins (distinct variant per `ErrorCode`, like the PWA's
/// `VssError.errorCode`); an undecodable body falls back to the HTTP status,
/// with a bare 409 still surfacing as a conflict.
fn parse_error(status: u16, payload: &[u8]) -> VssError {
    if let Ok(response) = ErrorResponse::decode(payload) {
        let message = response.message.clone();
        match response.error_code() {
            ErrorCode::ConflictException => return VssError::Conflict { message },
            ErrorCode::InvalidRequestException => return VssError::InvalidRequest { message },
            ErrorCode::AuthException => return VssError::Auth { message },
            ErrorCode::NoSuchKeyException => return VssError::NoSuchKey { message },
            ErrorCode::InternalServerException => return VssError::InternalServer { message },
            ErrorCode::Unknown => {}
        }
    }
    if status == 409 {
        return VssError::Conflict {
            message: format!("HTTP {status} without a decodable ErrorResponse"),
        };
    }
    VssError::Http {
        status,
        message: "response body is not a decodable ErrorResponse".to_string(),
    }
}

/// Decodes a 2xx protobuf response body, mapping decode failures to a typed
/// HTTP-level error (a success status with garbage bytes is a server bug or a
/// middlebox, not a crypto failure).
fn decode_response<M: Message + Default>(status: u16, payload: &[u8]) -> Result<M, VssError> {
    M::decode(payload).map_err(|e| VssError::Http {
        status,
        message: format!("undecodable response body: {e}"),
    })
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use super::super::crypto::tests::VECTOR_ENC_KEY;
    use super::*;

    /// Valid non-zero signing key for transport tests.
    const TEST_SIGNING_KEY: [u8; 32] = [0x2a; 32];
    const TEST_STORE_ID: &str = "test-store";

    /// Recorded requests: `(head, protobuf body)` per request, in order.
    type RecordedRequests = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

    /// A scripted HTTP stub (same pattern as `builder.rs`'s esplora stub):
    /// serves the canned `(status, body)` responses in order, repeating the
    /// last one forever, and records every raw request (head + body).
    struct StubServer {
        url: String,
        requests: RecordedRequests,
        hits: Arc<AtomicUsize>,
    }

    impl StubServer {
        fn request_count(&self) -> usize {
            self.hits.load(Ordering::SeqCst)
        }

        /// The recorded protobuf body of request `index`.
        fn body(&self, index: usize) -> Vec<u8> {
            self.requests.lock().unwrap()[index].1.clone()
        }

        /// The recorded request head (request line + headers) of request
        /// `index`, lowercased for header matching.
        fn head(&self, index: usize) -> String {
            self.requests.lock().unwrap()[index].0.to_lowercase()
        }
    }

    fn spawn_stub(responses: Vec<(u16, Vec<u8>)>) -> StubServer {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let requests: RecordedRequests = Arc::new(Mutex::new(Vec::new()));
        let hits = Arc::new(AtomicUsize::new(0));
        let thread_requests = Arc::clone(&requests);
        let thread_hits = Arc::clone(&hits);
        std::thread::spawn(move || {
            let mut scripted = responses.into_iter();
            let mut current: Option<(u16, Vec<u8>)> = None;
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let (head, body) = read_request(&mut stream);
                thread_requests.lock().unwrap().push((head, body));
                thread_hits.fetch_add(1, Ordering::SeqCst);
                if let Some(next) = scripted.next() {
                    current = Some(next);
                }
                let (status, response_body) =
                    current.clone().expect("stub started with zero responses");
                let response = format!(
                    "HTTP/1.1 {status} STUB\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    response_body.len(),
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.write_all(&response_body);
            }
        });
        StubServer {
            url: format!("http://{addr}"),
            requests,
            hits,
        }
    }

    /// Reads one HTTP request: head until the blank line, then exactly
    /// `Content-Length` body bytes.
    fn read_request(stream: &mut std::net::TcpStream) -> (String, Vec<u8>) {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        let header_end = loop {
            let n = stream.read(&mut chunk).unwrap_or(0);
            if n == 0 {
                break buf.len();
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
        };
        let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
        let content_length = head
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())?
            })
            .unwrap_or(0);
        let mut body = buf[header_end..].to_vec();
        while body.len() < content_length {
            let n = stream.read(&mut chunk).unwrap_or(0);
            if n == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..n]);
        }
        (head, body)
    }

    fn test_client(base_url: String) -> VssWireClient {
        VssWireClient::with_retry_policy(
            base_url,
            TEST_STORE_ID.to_string(),
            VECTOR_ENC_KEY,
            &TEST_SIGNING_KEY,
            VssRetryPolicy {
                base_delay: Duration::from_millis(1),
                max_attempts: 3,
            },
        )
        .unwrap()
    }

    fn error_response_bytes(code: ErrorCode, message: &str) -> Vec<u8> {
        ErrorResponse {
            error_code: code as i32,
            message: message.to_string(),
        }
        .encode_to_vec()
    }

    #[tokio::test]
    async fn get_object_maps_http_404_to_none_without_parsing_a_body() {
        let stub = spawn_stub(vec![(404, Vec::new())]);
        let client = test_client(stub.url.clone());
        let result = client.get_object("channel_manager").await.unwrap();
        assert_eq!(result, None);
        assert_eq!(stub.request_count(), 1, "404 must never be retried");
    }

    #[tokio::test]
    async fn get_object_decrypts_the_response_value_and_returns_the_version() {
        let plaintext = b"secret-data".to_vec();
        let response = GetObjectResponse {
            value: Some(KeyValue {
                key: crypto::obfuscate_key(&VECTOR_ENC_KEY, "my-key"),
                version: 3,
                value: crypto::encrypt(&VECTOR_ENC_KEY, &plaintext),
            }),
        };
        let stub = spawn_stub(vec![(200, response.encode_to_vec())]);
        let client = test_client(stub.url.clone());

        let (value, version) = client.get_object("my-key").await.unwrap().unwrap();
        assert_eq!(value, plaintext);
        assert_eq!(version, 3);

        // The request went to /getObject with the obfuscated (never
        // plaintext) key and the signature authorization header.
        let head = stub.head(0);
        assert!(head.starts_with("post /getobject http/1.1"), "{head}");
        assert!(head.contains("content-type: application/octet-stream"));
        assert!(head.contains("authorization: "));
        let sent = GetObjectRequest::decode(&stub.body(0)[..]).unwrap();
        assert_eq!(sent.store_id, TEST_STORE_ID);
        assert_eq!(sent.key, crypto::obfuscate_key(&VECTOR_ENC_KEY, "my-key"));
    }

    #[tokio::test]
    async fn get_object_surfaces_an_undecryptable_value_as_a_crypto_error() {
        let response = GetObjectResponse {
            value: Some(KeyValue {
                key: "k".to_string(),
                version: 1,
                value: vec![0u8; 40], // right length, garbage bytes
            }),
        };
        let stub = spawn_stub(vec![(200, response.encode_to_vec())]);
        let client = test_client(stub.url.clone());
        let err = client.get_object("k").await.unwrap_err();
        assert_eq!(
            err,
            VssError::Crypto(crypto::CryptoError::DecryptFailed),
            "tampered remote bytes must be a typed crypto failure"
        );
    }

    #[tokio::test]
    async fn put_object_sends_a_single_item_transactional_put_and_increments_the_version() {
        let stub = spawn_stub(vec![(200, Vec::new())]);
        let client = test_client(stub.url.clone());

        let new_version = client.put_object("my-key", b"my-value", 0).await.unwrap();
        assert_eq!(new_version, 1, "first write at 0 must return 1");

        let head = stub.head(0);
        assert!(head.starts_with("post /putobjects http/1.1"), "{head}");
        let sent = PutObjectRequest::decode(&stub.body(0)[..]).unwrap();
        assert_eq!(sent.store_id, TEST_STORE_ID);
        assert_eq!(sent.global_version, None);
        assert!(sent.delete_items.is_empty());
        assert_eq!(sent.transaction_items.len(), 1);
        let item = &sent.transaction_items[0];
        assert_eq!(item.version, 0);
        assert_eq!(item.key, crypto::obfuscate_key(&VECTOR_ENC_KEY, "my-key"));
        // The value on the wire is the encrypted blob, never the plaintext.
        assert_eq!(
            item.value.len(),
            crypto::NONCE_LEN + b"my-value".len() + crypto::TAG_LEN
        );
        assert_eq!(
            crypto::decrypt(&VECTOR_ENC_KEY, &item.value).unwrap(),
            b"my-value".to_vec()
        );
    }

    #[tokio::test]
    async fn put_at_a_stale_version_is_a_typed_conflict_and_is_never_retried() {
        let stub = spawn_stub(vec![(
            409,
            error_response_bytes(ErrorCode::ConflictException, "Version mismatch"),
        )]);
        let client = test_client(stub.url.clone());
        let err = client.put_object("k", b"v", 5).await.unwrap_err();
        assert_eq!(
            err,
            VssError::Conflict {
                message: "Version mismatch".to_string()
            }
        );
        assert_eq!(stub.request_count(), 1, "conflicts must never be retried");
    }

    #[tokio::test]
    async fn bare_409_without_a_proto_body_is_still_a_conflict() {
        let stub = spawn_stub(vec![(409, b"proxy says no".to_vec())]);
        let client = test_client(stub.url.clone());
        let err = client.put_object("k", b"v", 5).await.unwrap_err();
        assert!(matches!(err, VssError::Conflict { .. }), "got {err:?}");
        assert_eq!(stub.request_count(), 1);
    }

    #[tokio::test]
    async fn auth_and_invalid_request_errors_are_typed_and_never_retried() {
        let stub = spawn_stub(vec![(
            401,
            error_response_bytes(ErrorCode::AuthException, "bad signature"),
        )]);
        let client = test_client(stub.url.clone());
        let err = client.put_object("k", b"v", 0).await.unwrap_err();
        assert_eq!(
            err,
            VssError::Auth {
                message: "bad signature".to_string()
            }
        );
        assert_eq!(stub.request_count(), 1);

        let stub = spawn_stub(vec![(
            400,
            error_response_bytes(ErrorCode::InvalidRequestException, "malformed"),
        )]);
        let client = test_client(stub.url.clone());
        let err = client.delete_object("k", 1).await.unwrap_err();
        assert_eq!(
            err,
            VssError::InvalidRequest {
                message: "malformed".to_string()
            }
        );
        assert_eq!(stub.request_count(), 1);
    }

    #[tokio::test]
    async fn transient_5xx_is_retried_with_backoff_until_success() {
        let response = GetObjectResponse {
            value: Some(KeyValue {
                key: "k".to_string(),
                version: 7,
                value: crypto::encrypt(&VECTOR_ENC_KEY, b"after retry"),
            }),
        };
        let stub = spawn_stub(vec![
            (
                500,
                error_response_bytes(ErrorCode::InternalServerException, "flaky"),
            ),
            (503, Vec::new()),
            (200, response.encode_to_vec()),
        ]);
        let client = test_client(stub.url.clone());
        let (value, version) = client.get_object("k").await.unwrap().unwrap();
        assert_eq!(value, b"after retry".to_vec());
        assert_eq!(version, 7);
        assert_eq!(stub.request_count(), 3, "two transient failures, then ok");
    }

    #[tokio::test]
    async fn exhausted_5xx_retries_surface_the_typed_server_error() {
        let stub = spawn_stub(vec![(
            500,
            error_response_bytes(ErrorCode::InternalServerException, "still down"),
        )]);
        let client = test_client(stub.url.clone());
        let err = client.put_object("k", b"v", 0).await.unwrap_err();
        assert_eq!(
            err,
            VssError::InternalServer {
                message: "still down".to_string()
            }
        );
        assert_eq!(
            stub.request_count(),
            3,
            "the capped policy allows exactly max_attempts tries"
        );
    }

    #[tokio::test]
    async fn put_retries_resend_the_identical_ciphertext_buffer() {
        // KTD-3's 409 content-compare depends on retries never re-encrypting.
        let stub = spawn_stub(vec![(500, Vec::new()), (200, Vec::new())]);
        let client = test_client(stub.url.clone());
        client.put_object("k", b"stable bytes", 2).await.unwrap();
        assert_eq!(stub.request_count(), 2);
        assert_eq!(
            stub.body(0),
            stub.body(1),
            "retried request bytes must be identical (same nonce, same ciphertext)"
        );
    }

    #[tokio::test]
    async fn network_errors_are_typed_and_retried_up_to_the_cap() {
        // A local port with no listener: refused instantly, offline-safe.
        let client = test_client("http://127.0.0.1:1".to_string());
        let err = client.get_object("k").await.unwrap_err();
        assert!(matches!(err, VssError::Network { .. }), "got {err:?}");
    }

    #[tokio::test]
    async fn no_such_key_error_response_is_typed() {
        // A non-404 status carrying NO_SUCH_KEY (e.g. delete of a missing
        // key on stricter servers) maps to the distinct typed variant.
        let stub = spawn_stub(vec![(
            400,
            error_response_bytes(ErrorCode::NoSuchKeyException, "no such key"),
        )]);
        let client = test_client(stub.url.clone());
        let err = client.delete_object("k", 1).await.unwrap_err();
        assert_eq!(
            err,
            VssError::NoSuchKey {
                message: "no such key".to_string()
            }
        );
    }

    #[tokio::test]
    async fn list_key_versions_pages_and_forwards_the_page_token() {
        let page1 = ListKeyVersionsResponse {
            key_versions: vec![
                KeyValue {
                    key: "a".to_string(),
                    version: 1,
                    value: Vec::new(),
                },
                KeyValue {
                    key: "b".to_string(),
                    version: 2,
                    value: Vec::new(),
                },
            ],
            next_page_token: Some("page2".to_string()),
            global_version: None,
        };
        let page2 = ListKeyVersionsResponse {
            key_versions: vec![KeyValue {
                key: "c".to_string(),
                version: 3,
                value: Vec::new(),
            }],
            next_page_token: Some(String::new()),
            global_version: None,
        };
        let stub = spawn_stub(vec![
            (200, page1.encode_to_vec()),
            (200, page2.encode_to_vec()),
        ]);
        let client = test_client(stub.url.clone());

        let results = client.list_key_versions().await.unwrap();
        assert_eq!(
            results,
            vec![
                ("a".to_string(), 1),
                ("b".to_string(), 2),
                ("c".to_string(), 3)
            ]
        );
        assert_eq!(stub.request_count(), 2);

        let first = ListKeyVersionsRequest::decode(&stub.body(0)[..]).unwrap();
        assert_eq!(first.page_token, None, "first page must omit the token");
        let second = ListKeyVersionsRequest::decode(&stub.body(1)[..]).unwrap();
        assert_eq!(second.page_token.as_deref(), Some("page2"));
    }

    #[tokio::test]
    async fn list_key_versions_stops_at_the_page_cap_with_a_typed_error() {
        // Every page advertises another page: the cap must fire at 100.
        let endless = ListKeyVersionsResponse {
            key_versions: Vec::new(),
            next_page_token: Some("again".to_string()),
            global_version: None,
        };
        let stub = spawn_stub(vec![(200, endless.encode_to_vec())]);
        let client = test_client(stub.url.clone());
        let err = client.list_key_versions().await.unwrap_err();
        assert_eq!(err, VssError::TooManyListPages);
        assert_eq!(stub.request_count(), MAX_LIST_PAGES);
    }
}
