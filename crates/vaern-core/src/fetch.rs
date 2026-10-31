//! WHATWG-compliant HTTP client implementation powered by Hyper and Rustls.
//!
//! This module owns the network client lifecycle:
//! - HTTPS connection pooling and pure Rustls TLS validation (no OpenSSL).
//! - Staged error reporting identifying the exact failure phase (dns, connect, tls, send, read).
//! - Automatic redirect following capped at 20 hops with loop prevention.
//! - Request timeout and cancellation propagation.
//!
//! The production client trusts the public web roots only. The loopback test
//! server and the client that trusts its certificate live in `test_tls`,
//! which default builds do not compile.

use crate::error::{Result, VaernError};
use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Empty, Full};
use hyper::header::{HeaderName, HeaderValue, LOCATION};
use hyper::{Method, Request, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use rustls::{ClientConfig, RootCertStore};
use std::convert::Infallible;
use std::future;
use std::str::FromStr;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::sync::broadcast;
use url::Url;

/// Default timeout applied to requests when unspecified (30 seconds).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum number of redirects followed before returning a typed error.
pub const MAX_REDIRECTS: usize = 20;

/// Parsed HTTP request input passed from the JavaScript runtime.
#[derive(Debug, Clone)]
pub struct FetchOptions {
    pub method: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    pub timeout: Option<Duration>,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            method: "GET".to_string(),
            headers: Vec::new(),
            body: None,
            timeout: Some(DEFAULT_TIMEOUT),
        }
    }
}

/// Structured response returned from native fetch to JavaScript.
#[derive(Debug, Clone)]
pub struct FetchResponse {
    pub status: u16,
    pub status_text: String,
    pub headers: Vec<(String, String)>,
    pub url: String,
    pub body: Vec<u8>,
}

/// Ensures ring crypto provider is installed as the default for rustls.
pub fn init_crypto_provider() {
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Hyper client over rustls, as used by every fetch.
pub type HttpClient =
    Client<hyper_rustls::HttpsConnector<HttpConnector>, BoxBody<Bytes, Infallible>>;

/// Builds the production client, which trusts the public web roots only.
pub fn build_http_client() -> Result<HttpClient> {
    let mut root_store = RootCertStore::empty();
    root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    build_client_with_roots(root_store)
}

/// Builds a client that trusts exactly `root_store`.
pub(crate) fn build_client_with_roots(root_store: RootCertStore) -> Result<HttpClient> {
    init_crypto_provider();
    let tls_config = ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();

    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_tls_config(tls_config)
        .https_or_http()
        .enable_http1()
        .build();

    let client = Client::builder(TokioExecutor::new()).build(https);
    Ok(client)
}

/// Executes a native fetch request, following up to 20 redirects and enforcing timeouts.
pub async fn execute_fetch(
    initial_url: &str,
    options: FetchOptions,
    abort_rx: Option<broadcast::Receiver<()>>,
) -> Result<FetchResponse> {
    let url = parse_fetch_url(initial_url)?;
    let client = client_for(&url)?;
    execute_fetch_with(&client, url, options, abort_rx).await
}

#[cfg(any(test, feature = "test-tls"))]
fn client_for(url: &Url) -> Result<HttpClient> {
    if crate::test_tls::is_test_origin(url) {
        crate::test_tls::ensure_server_running();
        return crate::test_tls::client();
    }
    build_http_client()
}

#[cfg(not(any(test, feature = "test-tls")))]
fn client_for(_url: &Url) -> Result<HttpClient> {
    build_http_client()
}

/// Parses a fetch URL and rejects schemes other than http and https.
fn parse_fetch_url(initial_url: &str) -> Result<Url> {
    let url = Url::parse(initial_url).map_err(|err| VaernError::Fetch {
        stage: "dns".to_string(),
        url: initial_url.to_string(),
        message: format!("invalid URL syntax: {err}"),
        action: "verify the URL format and protocol scheme".to_string(),
    })?;

    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(VaernError::Fetch {
            stage: "connect".to_string(),
            url: initial_url.to_string(),
            message: format!("unsupported protocol scheme '{}'", url.scheme()),
            action: "use 'http://' or 'https://' URLs".to_string(),
        });
    }
    Ok(url)
}

/// Executes a fetch on `client`, following up to 20 redirects and enforcing timeouts.
async fn execute_fetch_with(
    client: &HttpClient,
    mut current_url: Url,
    options: FetchOptions,
    abort_rx: Option<broadcast::Receiver<()>>,
) -> Result<FetchResponse> {
    let timeout_duration = options.timeout.unwrap_or(DEFAULT_TIMEOUT);

    let mut method =
        Method::from_str(&options.method.to_uppercase()).map_err(|err| VaernError::Fetch {
            stage: "send".to_string(),
            url: current_url.to_string(),
            message: format!("invalid HTTP method: {err}"),
            action: "use a valid standard HTTP method name".to_string(),
        })?;

    let mut body_bytes = options.body;
    let mut redirect_count = 0;

    loop {
        let uri: hyper::Uri = current_url
            .as_str()
            .parse()
            .map_err(|err| VaernError::Fetch {
                stage: "send".to_string(),
                url: current_url.to_string(),
                message: format!("failed to format URI: {err}"),
                action: "verify the target URL characters".to_string(),
            })?;

        let mut req_builder = Request::builder().method(method.clone()).uri(uri);

        for (key, val) in &options.headers {
            if let (Ok(hname), Ok(hval)) = (HeaderName::from_str(key), HeaderValue::from_str(val)) {
                req_builder = req_builder.header(hname, hval);
            }
        }

        let body_payload: BoxBody<Bytes, Infallible> = if let Some(ref bytes) = body_bytes {
            Full::new(Bytes::copy_from_slice(bytes)).boxed()
        } else {
            Empty::new().boxed()
        };

        let req = req_builder
            .body(body_payload)
            .map_err(|err| VaernError::Fetch {
                stage: "send".to_string(),
                url: current_url.to_string(),
                message: format!("failed to build HTTP request: {err}"),
                action: "verify header names and values".to_string(),
            })?;

        // Race request execution against timeout and cancellation
        let send_future = client.request(req);
        let mut abort_channel = abort_rx.as_ref().map(|rx| rx.resubscribe());

        let resp = tokio::select! {
            biased;
            _ = async {
                if let Some(ref mut rx) = abort_channel {
                    let _ = rx.recv().await;
                } else {
                    future::pending::<()>().await;
                }
            } => {
                return Err(VaernError::Fetch {
                    stage: "send".to_string(),
                    url: current_url.to_string(),
                    message: "request aborted by AbortSignal".to_string(),
                    action: "handle AbortError or increase abort timeout".to_string(),
                });
            }
            res = tokio::time::timeout(timeout_duration, send_future) => {
                match res {
                    Ok(Ok(response)) => response,
                    Ok(Err(err)) => {
                        let stage = if err.is_connect() {
                            "connect"
                        } else if err.to_string().contains("certificate") || err.to_string().contains("tls") {
                            "tls"
                        } else {
                            "send"
                        };
                        return Err(VaernError::Fetch {
                            stage: stage.to_string(),
                            url: current_url.to_string(),
                            message: format!("{err}"),
                            action: "check network connection and host accessibility".to_string(),
                        });
                    }
                    Err(_) => {
                        return Err(VaernError::Fetch {
                            stage: "connect".to_string(),
                            url: current_url.to_string(),
                            message: "connection attempt timed out".to_string(),
                            action: "verify server responsiveness".to_string(),
                        });
                    }
                }
            }
        };

        let status = resp.status();

        // Follow HTTP redirects (301, 302, 303, 307, 308)
        if status.is_redirection() {
            redirect_count += 1;
            if redirect_count > MAX_REDIRECTS {
                return Err(VaernError::Fetch {
                    stage: "connect".to_string(),
                    url: current_url.to_string(),
                    message: format!("maximum redirect limit of {MAX_REDIRECTS} exceeded"),
                    action: "inspect redirect chain and check for redirection loops".to_string(),
                });
            }

            if let Some(location_val) = resp.headers().get(LOCATION) {
                let loc_str = location_val.to_str().map_err(|_| VaernError::Fetch {
                    stage: "read".to_string(),
                    url: current_url.to_string(),
                    message: "invalid non-UTF8 Location header in redirect response".to_string(),
                    action: "verify remote server redirect headers".to_string(),
                })?;

                current_url = current_url.join(loc_str).map_err(|err| VaernError::Fetch {
                    stage: "read".to_string(),
                    url: current_url.to_string(),
                    message: format!("failed to resolve redirect location '{loc_str}': {err}"),
                    action: "check remote server Location header value".to_string(),
                })?;

                // RFC 9110: 303 always switches to GET; 301/302 typically switch POST to GET
                if status == StatusCode::SEE_OTHER
                    || ((status == StatusCode::MOVED_PERMANENTLY || status == StatusCode::FOUND)
                        && method == Method::POST)
                {
                    method = Method::GET;
                    body_bytes = None;
                }

                continue;
            }
        }

        // Collect response headers
        let mut resp_headers = Vec::new();
        for (k, v) in resp.headers() {
            if let Ok(v_str) = v.to_str() {
                resp_headers.push((k.as_str().to_string(), v_str.to_string()));
            }
        }

        // Read response body with read timeout and abort protection
        let body_future = resp.into_body().collect();
        let body_bytes_result = tokio::select! {
            biased;
            _ = async {
                if let Some(ref mut rx) = abort_channel {
                    let _ = rx.recv().await;
                } else {
                    future::pending::<()>().await;
                }
            } => {
                return Err(VaernError::Fetch {
                    stage: "read".to_string(),
                    url: current_url.to_string(),
                    message: "response read aborted by AbortSignal".to_string(),
                    action: "handle AbortError or extend abort timeout".to_string(),
                });
            }
            res = tokio::time::timeout(timeout_duration, body_future) => {
                match res {
                    Ok(Ok(collected)) => collected.to_bytes().to_vec(),
                    Ok(Err(err)) => {
                        return Err(VaernError::Fetch {
                            stage: "read".to_string(),
                            url: current_url.to_string(),
                            message: format!("failed to read response body: {err}"),
                            action: "verify server data stream integrity".to_string(),
                        });
                    }
                    Err(_) => {
                        return Err(VaernError::Fetch {
                            stage: "read".to_string(),
                            url: current_url.to_string(),
                            message: "response body read timed out".to_string(),
                            action: "verify server responsiveness".to_string(),
                        });
                    }
                }
            }
        };

        return Ok(FetchResponse {
            status: status.as_u16(),
            status_text: status.canonical_reason().unwrap_or("").to_string(),
            headers: resp_headers,
            url: current_url.to_string(),
            body: body_bytes_result,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_url_parsing_error() {
        let options = FetchOptions::default();
        let result = execute_fetch("not-a-valid-url", options, None).await;
        match result {
            Err(VaernError::Fetch { stage, .. }) => {
                assert_eq!(stage, "dns");
            }
            _ => panic!("expected typed fetch dns error for invalid URL"),
        }
    }

    #[tokio::test]
    async fn test_unsupported_protocol_scheme() {
        let options = FetchOptions::default();
        let result = execute_fetch("ftp://example.com/file", options, None).await;
        match result {
            Err(VaernError::Fetch { stage, .. }) => {
                assert_eq!(stage, "connect");
            }
            _ => panic!("expected typed fetch connect error for unsupported protocol"),
        }
    }

    #[tokio::test]
    async fn test_embedded_server_basic_get() {
        crate::test_tls::ensure_server_running();

        let options = FetchOptions::default();
        let Ok(res) = execute_fetch("https://127.0.0.1:8443/test", options, None).await else {
            panic!("fetch /test failed");
        };
        assert_eq!(res.status, 200);
        assert_eq!(String::from_utf8_lossy(&res.body), "Hello, Vaern HTTPS!");
    }

    #[tokio::test]
    async fn test_production_client_rejects_test_certificate() {
        crate::test_tls::ensure_server_running();

        let Ok(client) = build_http_client() else {
            panic!("failed to build the production client");
        };
        let Ok(url) = parse_fetch_url("https://127.0.0.1:8443/test") else {
            panic!("failed to parse the test URL");
        };
        let result = execute_fetch_with(&client, url, FetchOptions::default(), None).await;
        match result {
            Err(VaernError::Fetch { stage, .. }) => assert_eq!(stage, "connect"),
            Ok(res) => panic!(
                "production client trusted the test certificate (status {})",
                res.status
            ),
            Err(other) => panic!("expected a typed fetch error, got {other}"),
        }
    }

    #[tokio::test]
    async fn test_redirect_cap_exceeded() {
        crate::test_tls::ensure_server_running();

        let options = FetchOptions::default();
        let result = execute_fetch("https://127.0.0.1:8443/redirect?count=25", options, None).await;
        match result {
            Err(VaernError::Fetch { stage, message, .. }) => {
                assert_eq!(stage, "connect");
                assert!(message.contains("maximum redirect limit"));
            }
            _ => panic!("expected redirect cap error"),
        }
    }

    #[tokio::test]
    async fn test_read_timeout_error() {
        crate::test_tls::ensure_server_running();

        let options = FetchOptions {
            timeout: Some(Duration::from_millis(250)),
            ..Default::default()
        };
        let result = execute_fetch("https://127.0.0.1:8443/blackhole", options, None).await;
        match result {
            Err(VaernError::Fetch { stage, message, .. }) => {
                assert_eq!(stage, "read");
                assert!(
                    message.contains("timed out")
                        || message.contains("failed to read response body")
                );
            }
            _ => panic!("expected read timeout error"),
        }
    }

    #[tokio::test]
    async fn test_abort_signal_cancels_request() {
        crate::test_tls::ensure_server_running();

        let (tx, rx) = broadcast::channel(1);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            let _ = tx.send(());
        });

        let options = FetchOptions {
            timeout: Some(Duration::from_secs(5)),
            ..Default::default()
        };
        let result = execute_fetch("https://127.0.0.1:8443/delay?ms=1000", options, Some(rx)).await;
        match result {
            Err(VaernError::Fetch { message, .. }) => {
                assert!(message.contains("aborted by AbortSignal"));
            }
            _ => panic!("expected aborted fetch error"),
        }
    }
}
