//! Accepts client connections, runs traffic through the interceptor chain and
//! forwards it upstream.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::{self, HeaderMap, HeaderName};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, StatusCode, Uri};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

use crate::Error;
use crate::ca::CertificateAuthority;
use crate::interceptor::{Chain, Request, Response};

type ClientRequest = hyper::Request<Incoming>;
type ClientResponse = hyper::Response<Full<Bytes>>;

pub struct Proxy {
    chain: Chain,
    ca: CertificateAuthority,
    client: Client<HttpsConnector<HttpConnector>, Full<Bytes>>,
}

impl Proxy {
    pub fn new(chain: Chain, ca: CertificateAuthority) -> Self {
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_or_http()
            .enable_http1()
            .build();
        Self {
            chain,
            ca,
            client: Client::builder(TokioExecutor::new()).build(https),
        }
    }

    /// Serves proxy connections on `addr` until an accept error occurs.
    pub async fn run(self: Arc<Self>, addr: SocketAddr) -> Result<(), Error> {
        let listener = TcpListener::bind(addr).await?;
        eprintln!("rflow listening on {addr}");
        loop {
            let (stream, _) = listener.accept().await?;
            let proxy = self.clone();
            tokio::spawn(async move {
                let service = service_fn(move |req| proxy.clone().handle(req));
                if let Err(e) = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .with_upgrades()
                    .await
                {
                    eprintln!("connection error: {e}");
                }
            });
        }
    }

    async fn handle(self: Arc<Self>, req: ClientRequest) -> Result<ClientResponse, Infallible> {
        if req.method() == Method::CONNECT {
            return Ok(self.connect(req));
        }
        if req.uri().scheme().is_none() {
            return Ok(error_response(StatusCode::BAD_REQUEST, "request URI must be absolute"));
        }
        Ok(self.forward(req).await)
    }

    /// Accepts a CONNECT tunnel and intercepts the TLS traffic sent through it.
    fn connect(self: Arc<Self>, req: ClientRequest) -> ClientResponse {
        let Some(authority) = req.uri().authority().cloned() else {
            return error_response(StatusCode::BAD_REQUEST, "CONNECT requires host:port");
        };
        let host = authority.host().trim_start_matches('[').trim_end_matches(']').to_string();
        let config = match self.ca.server_config(&host) {
            Ok(config) => config,
            Err(e) => {
                eprintln!("certificate error for {host}: {e}");
                return error_response(StatusCode::INTERNAL_SERVER_ERROR, "certificate error");
            }
        };

        let target = authority.to_string();
        tokio::spawn(async move {
            let result: Result<(), Error> = async {
                let upgraded = hyper::upgrade::on(req).await?;
                let tls = TlsAcceptor::from(config).accept(TokioIo::new(upgraded)).await?;
                let service = service_fn(move |mut req: ClientRequest| {
                    let proxy = self.clone();
                    let authority = authority.clone();
                    async move {
                        // Requests inside the tunnel are origin-form, so restore the full URI.
                        let path = req.uri().path_and_query().map_or("/", |p| p.as_str());
                        let origin = match authority.port_u16() {
                            Some(443) => authority.host(),
                            _ => authority.as_str(),
                        };
                        match Uri::try_from(format!("https://{origin}{path}")) {
                            Ok(uri) => *req.uri_mut() = uri,
                            Err(_) => return Ok(error_response(StatusCode::BAD_REQUEST, "invalid URI")),
                        }
                        Ok::<_, Infallible>(proxy.forward(req).await)
                    }
                });
                http1::Builder::new().serve_connection(TokioIo::new(tls), service).await?;
                Ok(())
            }
            .await;
            if let Err(e) = result {
                eprintln!("tunnel error for {target}: {e}");
            }
        });

        hyper::Response::new(Full::default())
    }

    /// Passes a request through the chain, sends it upstream and passes the
    /// response back through the chain.
    async fn forward(&self, req: ClientRequest) -> ClientResponse {
        match self.exchange(req).await {
            Ok(resp) => resp,
            Err(e) => {
                eprintln!("upstream error: {e}");
                error_response(StatusCode::BAD_GATEWAY, &e.to_string())
            }
        }
    }

    async fn exchange(&self, req: ClientRequest) -> Result<ClientResponse, Error> {
        let (parts, body) = req.into_parts();
        let mut req = Request {
            method: parts.method,
            uri: parts.uri,
            version: parts.version,
            headers: parts.headers,
            body: body.collect().await?.to_bytes(),
        };
        remove_hop_by_hop(&mut req.headers);
        if let Err(e) = self.chain.request(&mut req) {
            eprintln!("dropped {} {}: {e}", req.method, req.uri);
            return Ok(error_response(StatusCode::BAD_GATEWAY, &format!("request dropped: {e}")));
        }

        let mut upstream = hyper::Request::builder()
            .method(req.method.clone())
            .uri(req.uri.clone())
            .body(Full::new(req.body.clone()))?;
        *upstream.headers_mut() = req.headers.clone();

        let (parts, body) = self.client.request(upstream).await?.into_parts();
        let mut resp = Response {
            status: parts.status,
            version: parts.version,
            headers: parts.headers,
            body: body.collect().await?.to_bytes(),
        };
        remove_hop_by_hop(&mut resp.headers);
        if let Err(e) = self.chain.response(&req, &mut resp) {
            eprintln!("dropped response to {} {}: {e}", req.method, req.uri);
            return Ok(error_response(StatusCode::BAD_GATEWAY, &format!("response dropped: {e}")));
        }

        let mut out = hyper::Response::new(Full::new(resp.body));
        *out.status_mut() = resp.status;
        *out.headers_mut() = resp.headers;
        Ok(out)
    }
}

/// Removes headers that apply to a single connection and must not be forwarded.
fn remove_hop_by_hop(headers: &mut HeaderMap) {
    let listed: Vec<HeaderName> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|name| HeaderName::try_from(name.trim()).ok())
        .collect();
    for name in listed {
        headers.remove(name);
    }
    for name in [
        header::CONNECTION,
        header::PROXY_AUTHENTICATE,
        header::PROXY_AUTHORIZATION,
        header::TE,
        header::TRAILER,
        header::TRANSFER_ENCODING,
        header::UPGRADE,
        HeaderName::from_static("keep-alive"),
        HeaderName::from_static("proxy-connection"),
    ] {
        headers.remove(name);
    }
}

fn error_response(status: StatusCode, message: &str) -> ClientResponse {
    let mut resp = hyper::Response::new(Full::new(Bytes::from(format!("rflow: {message}\n"))));
    *resp.status_mut() = status;
    resp
}
