//! HTTP server implementation and request dispatching abstractions for Vaern.
//!
//! Owns TCP listening, connection serving with Hyper, bridging requests to the
//! JavaScript runtime, in-flight request tracking, and graceful shutdown draining.

use crate::error::{Result, VaernError};
use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::{HeaderName, HeaderValue};
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use std::convert::Infallible;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};

/// Validates that a hostname is either 'localhost' or a valid IPv4/IPv6 address.
pub fn validate_hostname(hostname: &str, port: u16) -> Result<IpAddr> {
    if hostname == "localhost" {
        return Ok(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));
    }
    IpAddr::from_str(hostname).map_err(|err| VaernError::Server {
        hostname: hostname.to_string(),
        port,
        message: format!("invalid hostname '{hostname}': {err}"),
        action: "verify the hostname is 'localhost' or a valid IP address (e.g. 127.0.0.1)"
            .to_string(),
    })
}

/// Incoming HTTP request bridged from Tokio Hyper listener to QuickJS runtime.
pub struct ServerIncomingRequest {
    pub server_id: u32,
    pub req_id: u64,
    pub method: String,
    pub uri: String,
    pub hostname: String,
    pub port: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub resp_sender: oneshot::Sender<ServerResponse>,
}

impl std::fmt::Debug for ServerIncomingRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerIncomingRequest")
            .field("server_id", &self.server_id)
            .field("req_id", &self.req_id)
            .field("method", &self.method)
            .field("uri", &self.uri)
            .field("hostname", &self.hostname)
            .field("port", &self.port)
            .field("headers", &self.headers)
            .field("body_len", &self.body.len())
            .finish()
    }
}

/// Response payload variant: either a full byte buffer or an asynchronous chunked stream.
pub enum ServerResponseBody {
    Full(Vec<u8>),
    Stream(mpsc::Receiver<std::result::Result<Bytes, std::io::Error>>),
}

/// Response data sent from QuickJS runtime back to the waiting Hyper connection.
pub struct ServerResponse {
    pub status: u16,
    pub status_text: String,
    pub headers: Vec<(String, String)>,
    pub body: ServerResponseBody,
}

/// Custom hyper body implementation supporting asynchronous chunked streams.
pub struct StreamReceiverBody {
    rx: mpsc::Receiver<std::result::Result<Bytes, std::io::Error>>,
}

impl StreamReceiverBody {
    pub fn new(rx: mpsc::Receiver<std::result::Result<Bytes, std::io::Error>>) -> Self {
        Self { rx }
    }
}

impl hyper::body::Body for StreamReceiverBody {
    type Data = Bytes;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<std::result::Result<hyper::body::Frame<Self::Data>, Self::Error>>> {
        match self.rx.poll_recv(cx) {
            Poll::Ready(Some(Ok(bytes))) => Poll::Ready(Some(Ok(hyper::body::Frame::data(bytes)))),
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(Box::new(e)))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Binds a TCP listener on the specified hostname and port.
pub async fn bind_listener(hostname: &str, port: u16) -> Result<TcpListener> {
    let ip = validate_hostname(hostname, port)?;
    let addr = SocketAddr::new(ip, port);
    TcpListener::bind(addr).await.map_err(|err| {
        let is_addr_in_use = err.kind() == std::io::ErrorKind::AddrInUse;
        VaernError::Server {
            hostname: hostname.to_string(),
            port,
            message: format!("failed to bind TCP listener on {addr}: {err}"),
            action: if is_addr_in_use {
                "select a different port or terminate the conflicting process".to_string()
            } else {
                "verify network permissions and firewall settings".to_string()
            },
        }
    })
}

/// Handle representing a running HTTP server instance.
pub struct HttpServer {
    pub server_id: u32,
    pub hostname: String,
    pub port: u16,
    shutdown_notify: Arc<tokio::sync::Notify>,
    in_flight: Arc<AtomicUsize>,
    is_closed: Arc<AtomicBool>,
}

impl HttpServer {
    /// Starts accepting connections on the provided TCP listener in the background.
    pub fn start(
        server_id: u32,
        hostname: String,
        port: u16,
        listener: TcpListener,
        request_tx: std::sync::mpsc::Sender<ServerIncomingRequest>,
        tokio_handle: tokio::runtime::Handle,
    ) -> Self {
        let shutdown_notify = Arc::new(tokio::sync::Notify::new());
        let in_flight = Arc::new(AtomicUsize::new(0));
        let is_closed = Arc::new(AtomicBool::new(false));

        let shutdown_rx = shutdown_notify.clone();
        let in_flight_server = in_flight.clone();
        let is_closed_server = is_closed.clone();
        let server_hostname = hostname.clone();

        let tokio_handle_loop = tokio_handle.clone();
        tokio_handle.spawn(async move {
            let next_req_id = Arc::new(AtomicU64::new(1));

            loop {
                tokio::select! {
                    biased;
                    _ = shutdown_rx.notified() => {
                        break;
                    }
                    accept_res = listener.accept() => {
                        let (stream, _) = match accept_res {
                            Ok(conn) => conn,
                            Err(_) => {
                                if is_closed_server.load(Ordering::SeqCst) {
                                    break;
                                }
                                continue;
                            }
                        };

                        let req_tx = request_tx.clone();
                        let in_flight_conn = in_flight_server.clone();
                        let next_req_id_conn = next_req_id.clone();
                        let conn_hostname = server_hostname.clone();

                        tokio_handle_loop.spawn(async move {
                            let io = TokioIo::new(stream);
                            let service = service_fn(move |req: Request<Incoming>| {
                                let req_tx = req_tx.clone();
                                let in_flight_req = in_flight_conn.clone();
                                let req_id = next_req_id_conn.fetch_add(1, Ordering::SeqCst);
                                let req_hostname = conn_hostname.clone();

                                async move {
                                    let method = req.method().to_string();
                                    let uri = req.uri().to_string();

                                    let mut headers = Vec::new();
                                    for (k, v) in req.headers() {
                                        if let Ok(v_str) = v.to_str() {
                                            headers.push((k.as_str().to_string(), v_str.to_string()));
                                        }
                                    }

                                    let body_bytes = match req.into_body().collect().await {
                                        Ok(collected) => collected.to_bytes().to_vec(),
                                        Err(_) => Vec::new(),
                                    };

                                    let (resp_tx, resp_rx) = oneshot::channel();
                                    in_flight_req.fetch_add(1, Ordering::SeqCst);

                                    let incoming = ServerIncomingRequest {
                                        server_id,
                                        req_id,
                                        method,
                                        uri,
                                        hostname: req_hostname,
                                        port,
                                        headers,
                                        body: body_bytes,
                                        resp_sender: resp_tx,
                                    };

                                    if req_tx.send(incoming).is_err() {
                                        in_flight_req.fetch_sub(1, Ordering::SeqCst);
                                        let unavailable = Response::builder()
                                            .status(StatusCode::SERVICE_UNAVAILABLE)
                                            .body(Full::new(Bytes::from("Runtime unavailable\n")).map_err(|never| -> Box<dyn std::error::Error + Send + Sync> { match never {} }).boxed());
                                        let res = match unavailable {
                                            Ok(r) => r,
                                            Err(_) => Response::new(Full::new(Bytes::new()).map_err(|never| -> Box<dyn std::error::Error + Send + Sync> { match never {} }).boxed()),
                                        };
                                        return Ok::<_, Infallible>(res);
                                    }

                                    let server_resp = match resp_rx.await {
                                        Ok(r) => r,
                                        Err(_) => {
                                            in_flight_req.fetch_sub(1, Ordering::SeqCst);
                                            let error_res = Response::builder()
                                                .status(StatusCode::INTERNAL_SERVER_ERROR)
                                                .body(Full::new(Bytes::from("Internal Server Error\n")).map_err(|never| -> Box<dyn std::error::Error + Send + Sync> { match never {} }).boxed());
                                            let res = match error_res {
                                                Ok(r) => r,
                                                Err(_) => Response::new(Full::new(Bytes::new()).map_err(|never| -> Box<dyn std::error::Error + Send + Sync> { match never {} }).boxed()),
                                            };
                                            return Ok::<_, Infallible>(res);
                                        }
                                    };

                                    in_flight_req.fetch_sub(1, Ordering::SeqCst);

                                    let status_code = match StatusCode::from_u16(server_resp.status) {
                                        Ok(sc) => sc,
                                        Err(_) => StatusCode::OK,
                                    };
                                    let mut resp_builder = Response::builder().status(status_code);

                                    for (k, v) in server_resp.headers {
                                        if let (Ok(hname), Ok(hval)) = (HeaderName::from_str(&k), HeaderValue::from_str(&v)) {
                                            resp_builder = resp_builder.header(hname, hval);
                                        }
                                    }

                                    let boxed_body: BoxBody<Bytes, Box<dyn std::error::Error + Send + Sync>> = match server_resp.body {
                                        ServerResponseBody::Full(bytes) => {
                                            Full::new(Bytes::from(bytes))
                                                .map_err(|never| -> Box<dyn std::error::Error + Send + Sync> { match never {} })
                                                .boxed()
                                        }
                                        ServerResponseBody::Stream(rx) => {
                                            StreamReceiverBody::new(rx).boxed()
                                        }
                                    };

                                    let resp = match resp_builder.body(boxed_body) {
                                        Ok(r) => r,
                                        Err(_) => Response::new(
                                            Full::new(Bytes::new())
                                                .map_err(|never| -> Box<dyn std::error::Error + Send + Sync> { match never {} })
                                                .boxed(),
                                        ),
                                    };

                                    Ok::<_, Infallible>(resp)
                                }
                            });

                            let _ = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                                .serve_connection(io, service)
                                .await;
                        });
                    }
                }
            }
        });

        Self {
            server_id,
            hostname,
            port,
            shutdown_notify,
            in_flight,
            is_closed,
        }
    }

    /// Stops accepting new connections and drains in-flight requests with a 5-second cap.
    pub async fn close(&self) {
        if self.is_closed.swap(true, Ordering::SeqCst) {
            return;
        }
        self.shutdown_notify.notify_waiters();

        let start = Instant::now();
        while self.in_flight.load(Ordering::SeqCst) > 0 {
            if start.elapsed() >= Duration::from_secs(5) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_server_hostname_validation() {
        assert_eq!(
            validate_hostname("localhost", 8000).unwrap(),
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))
        );
        assert_eq!(
            validate_hostname("127.0.0.1", 8000).unwrap(),
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))
        );
        assert_eq!(
            validate_hostname("0.0.0.0", 8000).unwrap(),
            IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0))
        );

        match validate_hostname("invalid:::hostname", 8000) {
            Err(VaernError::Server {
                hostname,
                port,
                message,
                action,
            }) => {
                assert_eq!(hostname, "invalid:::hostname");
                assert_eq!(port, 8000);
                assert!(message.contains("invalid hostname"));
                assert!(action.contains("verify the hostname"));
            }
            _ => panic!("expected typed server error for invalid hostname"),
        }

        match validate_hostname("999.999.999.999", 8080) {
            Err(VaernError::Server {
                hostname,
                port,
                message,
                ..
            }) => {
                assert_eq!(hostname, "999.999.999.999");
                assert_eq!(port, 8080);
                assert!(message.contains("invalid hostname"));
            }
            _ => panic!("expected typed server error for invalid IP"),
        }
    }

    #[tokio::test]
    async fn test_server_port_in_use_error() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind initial port failed");
        let bound_addr = listener.local_addr().expect("local_addr failed");
        let port = bound_addr.port();

        let result = bind_listener("127.0.0.1", port).await;
        match result {
            Err(VaernError::Server {
                hostname,
                port: err_port,
                message,
                action,
            }) => {
                assert_eq!(hostname, "127.0.0.1");
                assert_eq!(err_port, port);
                assert!(message.contains("failed to bind TCP listener"));
                assert_eq!(
                    action,
                    "select a different port or terminate the conflicting process"
                );
            }
            _ => panic!("expected typed port-in-use error"),
        }
    }
}
