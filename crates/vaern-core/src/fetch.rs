//! WHATWG-compliant HTTP client implementation powered by Hyper and Rustls.
//!
//! This module owns the network client lifecycle:
//! - Pure Rustls TLS validation (no OpenSSL), and a client that callers keep
//!   across requests so connections and TLS sessions are reused.
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
use hyper_util::rt::{TokioExecutor, TokioTimer};
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

/// How long a pooled connection may sit idle before the pool closes it.
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// Idle connections kept per host. Without a bound, a runtime that fetches
/// from many hosts holds sockets open for its whole lifetime.
const POOL_MAX_IDLE_PER_HOST: usize = 8;

/// Builds a client that trusts exactly `root_store`.
pub(crate) fn build_client_with_roots(root_store: RootCertStore) -> Result<HttpClient> {
    build_client(root_store, POOL_IDLE_TIMEOUT)
}

fn build_client(root_store: RootCertStore, idle_timeout: Duration) -> Result<HttpClient> {
    init_crypto_provider();
    let tls_config = ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();

    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_tls_config(tls_config)
        .https_or_http()
        .enable_http1()
        .build();

    // The pool timer is what makes the idle timeout run in the background;
    // without it expired connections are found only when a host is reused.
    let client = Client::builder(TokioExecutor::new())
        .pool_timer(TokioTimer::new())
        .pool_idle_timeout(idle_timeout)
        .pool_max_idle_per_host(POOL_MAX_IDLE_PER_HOST)
        .build(https);
    Ok(client)
}

/// Executes a native fetch request on a client of its own, following up to 20
/// redirects and enforcing timeouts. Nothing is reused between calls.
pub async fn execute_fetch(
    initial_url: &str,
    options: FetchOptions,
    abort_rx: Option<broadcast::Receiver<()>>,
) -> Result<FetchResponse> {
    let client = build_http_client()?;
    execute_fetch_pooled(&client, initial_url, options, abort_rx).await
}

/// Executes a fetch on `pooled`, a production client the caller keeps across
/// requests so keep-alive connections and TLS sessions carry over.
pub async fn execute_fetch_pooled(
    pooled: &HttpClient,
    initial_url: &str,
    options: FetchOptions,
    abort_rx: Option<broadcast::Receiver<()>>,
) -> Result<FetchResponse> {
    let url = parse_fetch_url(initial_url)?;
    let client = client_for(&url, pooled)?;
    execute_fetch_with(&client, url, options, abort_rx).await
}

// The pooled client only ever holds the public roots: the test origin gets a
// separate client, so the test certificate cannot enter a shared pool.
#[cfg(any(test, feature = "test-tls"))]
fn client_for(url: &Url, pooled: &HttpClient) -> Result<HttpClient> {
    if crate::test_tls::is_test_origin(url) {
        crate::test_tls::ensure_server_running();
        return crate::test_tls::client();
    }
    Ok(pooled.clone())
}

#[cfg(not(any(test, feature = "test-tls")))]
fn client_for(_url: &Url, pooled: &HttpClient) -> Result<HttpClient> {
    Ok(pooled.clone())
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

/// Resolves once an abort has been sent on `abort_rx`, including one sent
/// before this call: the receiver keeps its own read position, so an abort
/// between redirect hops waits for the next hop instead of being missed.
/// A closed channel means nobody can abort any more, not an abort.
async fn aborted(abort_rx: &mut Option<broadcast::Receiver<()>>) {
    let Some(rx) = abort_rx else {
        return future::pending().await;
    };
    loop {
        match rx.recv().await {
            Ok(()) | Err(broadcast::error::RecvError::Lagged(_)) => return,
            Err(broadcast::error::RecvError::Closed) => future::pending::<()>().await,
        }
    }
}

/// Executes a fetch on `client`, following up to 20 redirects and enforcing timeouts.
async fn execute_fetch_with(
    client: &HttpClient,
    mut current_url: Url,
    options: FetchOptions,
    mut abort_rx: Option<broadcast::Receiver<()>>,
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
    let mut headers = options.headers;
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

        for (key, val) in &headers {
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

        // Race request execution against timeout and cancellation. The abort
        // arm is polled first, so an abort already sent stops this hop before
        // any of its bytes, credentials included, leave the process.
        let send_future = client.request(req);

        let resp = tokio::select! {
            biased;
            _ = aborted(&mut abort_rx) => {
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

                let next_url = current_url.join(loc_str).map_err(|err| VaernError::Fetch {
                    stage: "read".to_string(),
                    url: current_url.to_string(),
                    message: format!("failed to resolve redirect location '{loc_str}': {err}"),
                    action: "check remote server Location header value".to_string(),
                })?;

                // Following an https -> http redirect would resend the request,
                // and whatever it carries, in plain text.
                if current_url.scheme() == "https" && next_url.scheme() == "http" {
                    return Err(VaernError::Fetch {
                        stage: "connect".to_string(),
                        url: current_url.to_string(),
                        message: format!(
                            "redirect hop {redirect_count} to {next_url} would downgrade https to plain http"
                        ),
                        action: "fetch the http URL directly if plain HTTP is intended, or fix the server's Location header".to_string(),
                    });
                }

                // Credentials belong to the origin they were written for.
                if next_url.origin() != current_url.origin() {
                    headers.retain(|(name, _)| !is_credential_header(name));
                }

                if redirect_switches_to_get(status, &method) {
                    method = Method::GET;
                    body_bytes = None;
                    headers.retain(|(name, _)| !is_request_body_header(name));
                }

                current_url = next_url;
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
            _ = aborted(&mut abort_rx) => {
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

/// RFC 9110 section 15.4: 303 turns every method except HEAD into GET, 301
/// and 302 turn POST into GET, and 307 and 308 keep the method and body.
fn redirect_switches_to_get(status: StatusCode, method: &Method) -> bool {
    match status {
        StatusCode::SEE_OTHER => *method != Method::GET && *method != Method::HEAD,
        StatusCode::MOVED_PERMANENTLY | StatusCode::FOUND => *method == Method::POST,
        _ => false,
    }
}

/// Headers that carry credentials and must not cross to another origin.
fn is_credential_header(name: &str) -> bool {
    name.eq_ignore_ascii_case("authorization") || name.eq_ignore_ascii_case("cookie")
}

/// Headers that describe a request body, dropped together with the body.
fn is_request_body_header(name: &str) -> bool {
    [
        "content-type",
        "content-length",
        "content-encoding",
        "content-language",
        "content-location",
    ]
    .iter()
    .any(|h| name.eq_ignore_ascii_case(h))
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
    async fn test_pooled_client_reuses_one_connection() {
        let (addr, accepted) = crate::test_tls::start_counting_http_server().expect("server");
        let client = build_http_client().expect("client");
        for _ in 0..3 {
            let url = format!("http://{addr}/");
            let res = execute_fetch_pooled(&client, &url, FetchOptions::default(), None)
                .await
                .expect("fetch");
            assert_eq!(res.body, b"ok");
        }
        assert_eq!(accepted.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_pool_closes_idle_connections_without_further_requests() {
        let (addr, _, closed) = crate::test_tls::start_tracking_http_server().expect("server");
        let client =
            build_client(RootCertStore::empty(), Duration::from_millis(200)).expect("client");
        let res = execute_fetch_pooled(
            &client,
            &format!("http://{addr}/"),
            FetchOptions::default(),
            None,
        )
        .await
        .expect("fetch");
        assert_eq!(res.body, b"ok");
        // Nothing touches the client again; only the pool timer can close it.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while closed.load(std::sync::atomic::Ordering::SeqCst) == 0
            && std::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(closed.load(std::sync::atomic::Ordering::SeqCst), 1);
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

    /// Sends `method` through a `status` redirect to `location` and returns
    /// what the final hop reported.
    async fn redirect_report(
        method: &str,
        status: u16,
        location: &str,
        body: Option<&str>,
    ) -> Result<String> {
        let target = format!(
            "https://127.0.0.1:8443/redirect-to?status={status}&location={}",
            url::form_urlencoded::byte_serialize(location.as_bytes()).collect::<String>()
        );
        let options = FetchOptions {
            method: method.to_string(),
            headers: vec![
                ("Authorization".to_string(), "Bearer secret".to_string()),
                ("Cookie".to_string(), "session=abc".to_string()),
                ("Content-Type".to_string(), "text/plain".to_string()),
            ],
            body: body.map(|b| b.as_bytes().to_vec()),
            ..Default::default()
        };
        let res = execute_fetch(&target, options, None).await?;
        Ok(String::from_utf8_lossy(&res.body).into_owned())
    }

    #[tokio::test]
    async fn test_cross_origin_redirect_strips_credentials() {
        let Ok(report) =
            redirect_report("GET", 302, "https://localhost:8443/echo-request", None).await
        else {
            panic!("cross-origin redirect failed");
        };
        assert!(report.contains("auth= "), "{report}");
        assert!(report.contains("cookie= "), "{report}");
    }

    #[tokio::test]
    async fn test_same_origin_redirect_keeps_credentials() {
        let Ok(report) = redirect_report("GET", 302, "/echo-request", None).await else {
            panic!("same-origin redirect failed");
        };
        assert!(report.contains("auth=Bearer secret"), "{report}");
        assert!(report.contains("cookie=session=abc"), "{report}");
    }

    #[tokio::test]
    async fn test_303_turns_any_method_into_get_without_body() {
        let Ok(report) = redirect_report("PUT", 303, "/echo-request", Some("payload")).await else {
            panic!("303 redirect failed");
        };
        assert!(report.starts_with("GET "), "{report}");
        assert!(report.contains("content-type= "), "{report}");
        assert!(report.ends_with("body="), "{report}");
    }

    #[tokio::test]
    async fn test_302_turns_post_into_get_and_keeps_other_methods() {
        let Ok(post) = redirect_report("POST", 302, "/echo-request", Some("payload")).await else {
            panic!("302 POST redirect failed");
        };
        assert!(
            post.starts_with("GET ") && post.ends_with("body="),
            "{post}"
        );

        let Ok(delete) = redirect_report("DELETE", 301, "/echo-request", None).await else {
            panic!("301 DELETE redirect failed");
        };
        assert!(delete.starts_with("DELETE "), "{delete}");
    }

    #[tokio::test]
    async fn test_307_and_308_keep_method_and_body() {
        for status in [307, 308] {
            let Ok(report) =
                redirect_report("POST", status, "/echo-request", Some("payload")).await
            else {
                panic!("{status} redirect failed");
            };
            assert!(report.starts_with("POST "), "{status}: {report}");
            assert!(report.ends_with("body=payload"), "{status}: {report}");
        }
    }

    #[tokio::test]
    async fn test_https_to_http_redirect_is_blocked() {
        match redirect_report("GET", 302, "http://127.0.0.1:9/plain", None).await {
            Err(VaernError::Fetch {
                stage,
                url,
                message,
                ..
            }) => {
                assert_eq!(stage, "connect");
                assert!(url.starts_with("https://127.0.0.1:8443/"), "{url}");
                assert!(message.contains("http://127.0.0.1:9/plain"), "{message}");
                assert!(message.contains("downgrade"), "{message}");
            }
            other => panic!("expected a typed downgrade error, got {other:?}"),
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

    type Seen = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

    /// Loopback server recording "path authorization" per request. `/hop1`
    /// runs `on_hop1`, then redirects to `/hop2` on the same origin, so the
    /// client would carry the Authorization header to the second hop.
    async fn start_recording_server(
        on_hop1: impl Fn() + Clone + Send + Sync + 'static,
    ) -> (std::net::SocketAddr, Seen) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen: Seen = Default::default();
        let log = seen.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (log, on_hop1) = (log.clone(), on_hop1.clone());
                tokio::spawn(async move {
                    let service = hyper::service::service_fn(move |req: Request<_>| {
                        let auth = req
                            .headers()
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("-")
                            .to_string();
                        log.lock()
                            .unwrap()
                            .push(format!("{} {auth}", req.uri().path()));
                        let mut res = hyper::Response::new(Full::new(Bytes::from("ok")));
                        if req.uri().path() == "/hop1" {
                            on_hop1();
                            *res.status_mut() = StatusCode::FOUND;
                            res.headers_mut()
                                .insert(LOCATION, HeaderValue::from_static("/hop2"));
                        }
                        async move { Ok::<_, Infallible>(res) }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        (addr, seen)
    }

    fn with_credentials() -> FetchOptions {
        FetchOptions {
            headers: vec![("authorization".to_string(), "Bearer secret".to_string())],
            timeout: Some(Duration::from_secs(5)),
            ..Default::default()
        }
    }

    fn assert_aborted(result: Result<FetchResponse>) {
        match result {
            Err(VaernError::Fetch { message, .. }) => {
                assert!(message.contains("aborted by AbortSignal"), "{message}")
            }
            Err(other) => panic!("expected an abort, got {other}"),
            Ok(res) => panic!("expected an abort, got status {}", res.status),
        }
    }

    #[tokio::test]
    async fn test_abort_sent_before_the_request_starts_is_not_lost() {
        let (addr, seen) = start_recording_server(|| {}).await;
        let (tx, rx) = broadcast::channel(1);
        tx.send(()).unwrap();
        let result =
            execute_fetch(&format!("http://{addr}/hop1"), with_credentials(), Some(rx)).await;
        assert_aborted(result);
        assert!(
            seen.lock().unwrap().is_empty(),
            "{:?}",
            seen.lock().unwrap()
        );
    }

    #[tokio::test]
    async fn test_abort_during_a_redirect_keeps_credentials_from_the_next_hop() {
        let (tx, rx) = broadcast::channel(1);
        let (addr, seen) = start_recording_server(move || {
            let _ = tx.send(());
        })
        .await;
        let result =
            execute_fetch(&format!("http://{addr}/hop1"), with_credentials(), Some(rx)).await;
        assert_aborted(result);
        assert_eq!(
            *seen.lock().unwrap(),
            vec!["/hop1 Bearer secret".to_string()]
        );
    }
}
