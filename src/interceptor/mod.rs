//! The request/response model and the interceptor chain they pass through.

mod edit;
mod print;

pub use edit::{EditRequest, EditResponse};
pub use print::{PrintRequest, PrintResponse};

use std::sync::{Mutex, PoisonError};

use bytes::Bytes;
use hyper::{HeaderMap, Method, StatusCode, Uri, Version};

use crate::Error;

/// A fully buffered HTTP request, as received from the client.
pub struct Request {
    pub method: Method,
    /// Always absolute, e.g. `https://example.com/path?query`.
    pub uri: Uri,
    pub version: Version,
    pub headers: HeaderMap,
    pub body: Bytes,
}

/// A fully buffered HTTP response, as received from the upstream server.
pub struct Response {
    pub status: StatusCode,
    pub version: Version,
    pub headers: HeaderMap,
    pub body: Bytes,
}

/// A capability that can view or modify traffic as it passes through the proxy.
pub trait Interceptor: Send + Sync {
    /// Called before the request is sent upstream. Returning an error drops
    /// the request.
    fn request(&self, _req: &mut Request) -> Result<(), Error> {
        Ok(())
    }

    /// Called before the response is returned to the client. Returning an
    /// error drops the response.
    fn response(&self, _req: &Request, _resp: &mut Response) -> Result<(), Error> {
        Ok(())
    }
}

/// An ordered set of interceptors. Requests pass through in order and
/// responses pass back through in reverse order.
#[derive(Default)]
pub struct Chain(Vec<Box<dyn Interceptor>>);

impl Chain {
    pub fn add(&mut self, interceptor: impl Interceptor + 'static) {
        self.0.push(Box::new(interceptor));
    }

    /// Stops at the first interceptor that drops the request.
    pub fn request(&self, req: &mut Request) -> Result<(), Error> {
        for i in &self.0 {
            i.request(req)?;
        }
        Ok(())
    }

    /// Stops at the first interceptor that drops the response.
    pub fn response(&self, req: &Request, resp: &mut Response) -> Result<(), Error> {
        for i in self.0.iter().rev() {
            i.response(req, resp)?;
        }
        Ok(())
    }
}

/// Runs `f` with exclusive use of the terminal, so output from other requests
/// doesn't interleave with it or draw over an open editor.
fn with_terminal<T>(f: impl FnOnce() -> T) -> T {
    static TERMINAL: Mutex<()> = Mutex::new(());
    // Waiting may block for as long as an edit takes, so let the runtime move
    // other tasks off this thread.
    tokio::task::block_in_place(|| {
        let _guard = TERMINAL.lock().unwrap_or_else(PoisonError::into_inner);
        f()
    })
}

/// Builders that keep the interceptor tests short.
#[cfg(test)]
mod testing {
    use super::*;
    use hyper::header::{HeaderName, HeaderValue};

    /// An HTTP/1.1 request.
    pub fn request(method: &str, uri: &str, headers: &[(&str, &str)], body: &str) -> Request {
        Request {
            method: method.parse().unwrap(),
            uri: uri.parse().unwrap(),
            version: Version::HTTP_11,
            headers: header_map(headers),
            body: Bytes::from(body.to_owned()),
        }
    }

    /// An HTTP/1.1 response.
    pub fn response(status: u16, headers: &[(&str, &str)], body: &str) -> Response {
        Response {
            status: StatusCode::from_u16(status).unwrap(),
            version: Version::HTTP_11,
            headers: header_map(headers),
            body: Bytes::from(body.to_owned()),
        }
    }

    fn header_map(headers: &[(&str, &str)]) -> HeaderMap {
        headers
            .iter()
            .map(|(name, value)| (HeaderName::try_from(*name).unwrap(), HeaderValue::try_from(*value).unwrap()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{request, response};
    use super::*;

    /// Adds its name to an `x-trace` header, to record the order it ran in.
    struct Trace(&'static str);

    impl Interceptor for Trace {
        fn request(&self, req: &mut Request) -> Result<(), Error> {
            req.headers.append("x-trace", self.0.parse().unwrap());
            Ok(())
        }

        fn response(&self, _req: &Request, resp: &mut Response) -> Result<(), Error> {
            resp.headers.append("x-trace", self.0.parse().unwrap());
            Ok(())
        }
    }

    /// Drops every request and response.
    struct Reject;

    impl Interceptor for Reject {
        fn request(&self, _req: &mut Request) -> Result<(), Error> {
            Err("dropped".into())
        }

        fn response(&self, _req: &Request, _resp: &mut Response) -> Result<(), Error> {
            Err("dropped".into())
        }
    }

    fn trace(headers: &HeaderMap) -> Vec<&str> {
        headers.get_all("x-trace").iter().map(|v| v.to_str().unwrap()).collect()
    }

    #[test]
    fn chain_passes_requests_through_interceptors_in_order() {
        let mut chain = Chain::default();
        chain.add(Trace("first"));
        chain.add(Trace("second"));
        let mut req = request("GET", "http://example.com/", &[], "");

        chain.request(&mut req).unwrap();

        assert_eq!(trace(&req.headers), ["first", "second"]);
    }

    #[test]
    fn chain_passes_responses_through_interceptors_in_reverse_order() {
        let mut chain = Chain::default();
        chain.add(Trace("first"));
        chain.add(Trace("second"));
        let req = request("GET", "http://example.com/", &[], "");
        let mut resp = response(200, &[], "");

        chain.response(&req, &mut resp).unwrap();

        assert_eq!(trace(&resp.headers), ["second", "first"]);
    }

    #[test]
    fn chain_stops_at_the_first_interceptor_that_drops_the_request() {
        let mut chain = Chain::default();
        chain.add(Trace("before"));
        chain.add(Reject);
        chain.add(Trace("after"));
        let mut req = request("GET", "http://example.com/", &[], "");

        assert!(chain.request(&mut req).is_err());
        assert_eq!(trace(&req.headers), ["before"]);
    }

    #[test]
    fn chain_stops_at_the_first_interceptor_that_drops_the_response() {
        let mut chain = Chain::default();
        chain.add(Trace("after"));
        chain.add(Reject);
        chain.add(Trace("before"));
        let req = request("GET", "http://example.com/", &[], "");
        let mut resp = response(200, &[], "");

        assert!(chain.response(&req, &mut resp).is_err());
        assert_eq!(trace(&resp.headers), ["before"]);
    }
}
