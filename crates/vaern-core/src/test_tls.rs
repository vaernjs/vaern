//! Loopback HTTPS server and matching client for hermetic fetch tests.
//!
//! The server listens on 127.0.0.1:8443 with a self-signed certificate that
//! is generated per process. Only the client built here trusts it; the
//! production client in `fetch` never does. This module is compiled for unit
//! tests and for the opt-in `test-tls` feature only, so a default build,
//! including every release binary, carries no test certificate and no test
//! server.

use crate::error::Result;
use crate::fetch::HttpClient;
use bytes::Bytes;
use http_body_util::{BodyExt, Empty, Full};
use hyper::header::LOCATION;
use hyper::{Request, StatusCode};
use hyper_util::rt::TokioExecutor;
use rcgen::{CertificateParams, KeyPair};
use rustls::RootCertStore;
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use url::Url;

/// Address the loopback test server binds to.
pub const TEST_SERVER_ADDR: &str = "127.0.0.1:8443";

struct TestTlsMaterial {
    cert_der: Vec<u8>,
    key_der: Vec<u8>,
}

static TEST_TLS_MATERIAL: OnceLock<Option<TestTlsMaterial>> = OnceLock::new();

/// Generates, once per process, the self-signed certificate and key.
fn material() -> Option<&'static TestTlsMaterial> {
    crate::fetch::init_crypto_provider();
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

/// True when `url` targets the loopback test server.
pub fn is_test_origin(url: &Url) -> bool {
    url.scheme() == "https" && url.host_str() == Some("127.0.0.1") && url.port() == Some(8443)
}

/// Builds a client that trusts the public web roots plus the test certificate.
pub fn client() -> Result<HttpClient> {
    let mut root_store = RootCertStore::empty();
    root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Some(material) = material() {
        let _ = root_store.add(CertificateDer::from_slice(&material.cert_der));
    }
    crate::fetch::build_client_with_roots(root_store)
}

static SERVER_INIT: std::sync::Once = std::sync::Once::new();

/// Starts the loopback TLS test server if it is not already running.
pub fn ensure_server_running() {
    crate::fetch::init_crypto_provider();
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
                let material = match material() {
                    Some(m) => m,
                    None => {
                        let _ =
                            ready_tx.send(Err("failed to generate TLS test material".to_string()));
                        return;
                    }
                };
                let server_cert = CertificateDer::from(material.cert_der.clone());
                let private_key = PrivatePkcs8KeyDer::from(material.key_der.clone()).into();

                let server_config = match rustls::ServerConfig::builder()
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
                let addr: SocketAddr = match TEST_SERVER_ADDR.parse() {
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
                            |req: Request<hyper::body::Incoming>| async move { route(req).await },
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

type TestBody = http_body_util::combinators::BoxBody<Bytes, Infallible>;

async fn route(
    req: Request<hyper::body::Incoming>,
) -> std::result::Result<hyper::Response<TestBody>, hyper::http::Error> {
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
                .body(Full::new(Bytes::from("redirect destination")).boxed());
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
        // Headers go out at once and the body never completes, so the
        // client's read timeout is what ends the request.
        struct HangingBody;
        impl hyper::body::Body for HangingBody {
            type Data = Bytes;
            type Error = Infallible;

            fn poll_frame(
                self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<
                Option<std::result::Result<hyper::body::Frame<Self::Data>, Self::Error>>,
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
}
