//! WHATWG-compliant HTTP client implementation powered by Hyper and Rustls.
//!
//! This module owns the network client lifecycle:
//! - HTTPS connection pooling and pure Rustls TLS validation (no OpenSSL).
//! - Staged error reporting identifying the exact failure phase (dns, connect, tls, send, read).
//! - Automatic redirect following capped at 20 hops with loop prevention.
//! - Request timeout and cancellation propagation.
//! - Embedded in-process TLS loopback server for hermetic testing.

use crate::error::{Result, VaernError};
use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Empty, Full};
use hyper::header::{HeaderName, HeaderValue, LOCATION};
use hyper::{Method, Request, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use rcgen::{CertificateParams, KeyPair};
use std::convert::Infallible;
use std::future;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::pki_types::CertificateDer;
use tokio_rustls::rustls::{ClientConfig, RootCertStore, ServerConfig};
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

struct TestTlsMaterial {
    cert_der: Vec<u8>,
    key_der: Vec<u8>,
}

static TEST_TLS_MATERIAL: OnceLock<Option<TestTlsMaterial>> = OnceLock::new();

/// Ensures ring crypto provider is installed as the default for rustls.
pub fn init_crypto_provider() {
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Generates or returns the cached self-signed certificate and key for local test server.
fn get_test_tls_material() -> Option<&'static TestTlsMaterial> {
    init_crypto_provider();
    TEST_TLS_MATERIAL
        .get_or_init(|| {
            let params =
                CertificateParams::new(vec!["localhost".into(), "127.0.0.1".into()]).ok()?;
            let key_pair = KeyPair::generate().ok()?;
            let cert = params.self_signed(&key_pair).ok()?;
            Some(TestTlsMaterial {
                cert_der: cert.der().to_vec(),
                key_der: key_pair.serialize_der(),
            })
        })
        .as_ref()
}

/// Builds the hyper client configured with rustls and trusted root certificates.
fn build_http_client()
-> Result<Client<hyper_rustls::HttpsConnector<HttpConnector>, BoxBody<Bytes, Infallible>>> {
    init_crypto_provider();
    let mut root_store = RootCertStore::empty();
    root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    // Trust internal test certificate for hermetic loopback HTTPS testing
    if let Some(material) = get_test_tls_material() {
        let cert = CertificateDer::from_slice(&material.cert_der);
        let _ = root_store.add(cert);
    }

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

static SERVER_INIT: std::sync::Once = std::sync::Once::new();

/// Starts the in-process local TLS test server on 127.0.0.1:8443 if not already running.
pub fn ensure_test_server_running() {
    init_crypto_provider();
    SERVER_INIT.call_once(|| {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(err) => {
                    let _ = ready_tx.send(Err(err.to_string()));
                    return;
                }
            };

            rt.block_on(async move {
                let material = match get_test_tls_material() {
                    Some(m) => m,
                    None => {
                        let _ =
                            ready_tx.send(Err("failed to generate TLS test material".to_string()));
                        return;
                    }
                };
                let server_cert = CertificateDer::from(material.cert_der.clone());
                let private_key = tokio_rustls::rustls::pki_types::PrivatePkcs8KeyDer::from(
                    material.key_der.clone(),
                )
                .into();

                let server_config = match ServerConfig::builder()
                    .with_no_client_auth()
                    .with_single_cert(vec![server_cert], private_key)
                {
                    Ok(cfg) => cfg,
                    Err(err) => {
                        let _ =
                            ready_tx.send(Err(format!("failed to configure TLS server: {err}")));
                        return;
                    }
                };

                let acceptor = TlsAcceptor::from(Arc::new(server_config));
                let addr: SocketAddr = match "127.0.0.1:8443".parse() {
                    Ok(a) => a,
                    Err(err) => {
                        let _ = ready_tx.send(Err(format!("invalid socket address: {err}")));
                        return;
                    }
                };

                let listener = match TcpListener::bind(addr).await {
                    Ok(l) => {
                        let _ = ready_tx.send(Ok(()));
                        l
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e.to_string()));
                        return;
                    }
                };

                loop {
                    let (stream, _) = match listener.accept().await {
                        Ok(conn) => conn,
                        Err(_) => break,
                    };

                    let acceptor = acceptor.clone();
                    tokio::spawn(async move {
                        let tls_stream = match acceptor.accept(stream).await {
                            Ok(s) => s,
                            Err(_) => return,
                        };

                        let io = hyper_util::rt::TokioIo::new(tls_stream);
                        let service = hyper::service::service_fn(
                            |req: Request<hyper::body::Incoming>| async move {
                                let path_and_query = req
                                    .uri()
                                    .path_and_query()
                                    .map(|pq| pq.as_str())
                                    .unwrap_or("/");

                                if path_and_query.starts_with("/test") {
                                    let body = Full::new(Bytes::from("Hello, Vaern HTTPS!"));
                                    return hyper::Response::builder()
                                        .status(StatusCode::OK)
                                        .header("Content-Type", "text/plain")
                                        .body(body.boxed());
                                }

                                if path_and_query.starts_with("/echo-headers") {
                                    return hyper::Response::builder()
                                        .status(StatusCode::OK)
                                        .header("X-Response-Server", "vaern-test")
                                        .body(Full::new(Bytes::from("ok")).boxed());
                                }

                                if path_and_query.starts_with("/redirect") {
                                    let query = req.uri().query().unwrap_or("");
                                    let count: usize = query
                                        .strip_prefix("count=")
                                        .and_then(|s| s.parse().ok())
                                        .unwrap_or(0);

                                    if count > 1 {
                                        let loc = format!("/redirect?count={}", count - 1);
                                        return hyper::Response::builder()
                                            .status(StatusCode::FOUND)
                                            .header(LOCATION, loc)
                                            .body(Empty::new().boxed());
                                    } else if count == 1 {
                                        return hyper::Response::builder()
                                            .status(StatusCode::FOUND)
                                            .header(LOCATION, "/test")
                                            .body(Empty::new().boxed());
                                    } else {
                                        return hyper::Response::builder()
                                            .status(StatusCode::OK)
                                            .body(
                                                Full::new(Bytes::from("redirect destination"))
                                                    .boxed(),
                                            );
                                    }
                                }

                                if path_and_query.starts_with("/delay") {
                                    let query = req.uri().query().unwrap_or("");
                                    let ms: u64 = query
                                        .strip_prefix("ms=")
                                        .and_then(|s| s.parse().ok())
                                        .unwrap_or(100);
                                    tokio::time::sleep(Duration::from_millis(ms)).await;
                                    return hyper::Response::builder()
                                        .status(StatusCode::OK)
                                        .body(Full::new(Bytes::from("delayed response")).boxed());
                                }

                                if path_and_query.starts_with("/blackhole") {
                                    // Immediately send 200 OK headers with hanging body stream to trigger read timeout
                                    struct HangingBody;
                                    impl hyper::body::Body for HangingBody {
                                        type Data = Bytes;
                                        type Error = Infallible;

                                        fn poll_frame(
                                            self: std::pin::Pin<&mut Self>,
                                            _cx: &mut std::task::Context<'_>,
                                        ) -> std::task::Poll<
                                            Option<
                                                std::result::Result<
                                                    hyper::body::Frame<Self::Data>,
                                                    Self::Error,
                                                >,
                                            >,
                                        > {
                                            std::task::Poll::Pending
                                        }
                                    }

                                    return hyper::Response::builder()
                                        .status(StatusCode::OK)
                                        .header("Content-Type", "text/plain")
                                        .body(HangingBody.boxed());
                                }

                                hyper::Response::builder()
                                    .status(StatusCode::NOT_FOUND)
                                    .body(Empty::new().boxed())
                            },
                        );

                        let _ = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                            .serve_connection(io, service)
                            .await;
                    });
                }
            });
        });

        let _ = ready_rx.recv();
    });
}

/// Executes a native fetch request, following up to 20 redirects and enforcing timeouts.
pub async fn execute_fetch(
    initial_url: &str,
    options: FetchOptions,
    abort_rx: Option<broadcast::Receiver<()>>,
) -> Result<FetchResponse> {
    let mut current_url = Url::parse(initial_url).map_err(|err| VaernError::Fetch {
        stage: "dns".to_string(),
        url: initial_url.to_string(),
        message: format!("invalid URL syntax: {err}"),
        action: "verify the URL format and protocol scheme".to_string(),
    })?;

    if current_url.scheme() != "http" && current_url.scheme() != "https" {
        return Err(VaernError::Fetch {
            stage: "connect".to_string(),
            url: initial_url.to_string(),
            message: format!("unsupported protocol scheme '{}'", current_url.scheme()),
            action: "use 'http://' or 'https://' URLs".to_string(),
        });
    }

    if current_url.host_str() == Some("127.0.0.1") && current_url.port() == Some(8443) {
        ensure_test_server_running();
    }

    let client = build_http_client()?;
    let timeout_duration = options.timeout.unwrap_or(DEFAULT_TIMEOUT);

    let mut method =
        Method::from_str(&options.method.to_uppercase()).map_err(|err| VaernError::Fetch {
            stage: "send".to_string(),
            url: initial_url.to_string(),
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
        ensure_test_server_running();

        let options = FetchOptions::default();
        let Ok(res) = execute_fetch("https://127.0.0.1:8443/test", options, None).await else {
            panic!("fetch /test failed");
        };
        assert_eq!(res.status, 200);
        assert_eq!(String::from_utf8_lossy(&res.body), "Hello, Vaern HTTPS!");
    }

    #[tokio::test]
    async fn test_redirect_cap_exceeded() {
        ensure_test_server_running();

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
        ensure_test_server_running();

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
        ensure_test_server_running();

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
