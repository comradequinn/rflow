use std::fmt::Write as _;
use std::io::Write as _;

use bytes::Bytes;
use hyper::HeaderMap;

use super::{Interceptor, Request, Response, with_terminal};
use crate::Error;

/// Writes each request to stdout.
pub struct PrintRequest;

/// Writes each response to stdout.
pub struct PrintResponse;

impl Interceptor for PrintRequest {
    fn request(&self, req: &mut Request) -> Result<(), Error> {
        emit(&format_request(req));
        Ok(())
    }
}

impl Interceptor for PrintResponse {
    fn response(&self, req: &Request, resp: &mut Response) -> Result<(), Error> {
        emit(&format_response(req, resp));
        Ok(())
    }
}

fn format_request(req: &Request) -> String {
    let mut out = format!("> {} {} {:?}\n", req.method, req.uri, req.version);
    write_message(&mut out, &req.headers, &req.body);
    out
}

fn format_response(req: &Request, resp: &Response) -> String {
    let mut out = format!("< {:?} {} ({} {})\n", resp.version, resp.status, req.method, req.uri);
    write_message(&mut out, &resp.headers, &resp.body);
    out
}

fn write_message(out: &mut String, headers: &HeaderMap, body: &Bytes) {
    for (name, value) in headers {
        let _ = writeln!(out, "{}: {}", name, String::from_utf8_lossy(value.as_bytes()));
    }
    if body.is_empty() {
        return;
    }
    out.push('\n');
    match std::str::from_utf8(body) {
        Ok(text) => out.push_str(text),
        Err(_) => {
            let _ = write!(out, "[{} bytes of binary data]", body.len());
        }
    }
    out.push('\n');
}

fn emit(out: &str) {
    with_terminal(|| {
        let _ = writeln!(std::io::stdout().lock(), "{out}");
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interceptor::testing::{request, response};

    // --- PrintRequest ---

    #[test]
    fn print_request_shows_the_request_line_headers_and_body() {
        let req = request("POST", "http://example.com/login", &[("content-type", "text/plain")], "user=bob");

        assert_eq!(
            format_request(&req),
            "> POST http://example.com/login HTTP/1.1\n\
             content-type: text/plain\n\
             \n\
             user=bob\n"
        );
    }

    #[test]
    fn print_request_leaves_out_the_body_section_when_there_is_no_body() {
        let req = request("GET", "http://example.com/", &[("accept", "*/*")], "");

        assert_eq!(
            format_request(&req),
            "> GET http://example.com/ HTTP/1.1\n\
             accept: */*\n"
        );
    }

    #[test]
    fn print_request_summarises_a_binary_body() {
        let mut req = request("POST", "http://example.com/upload", &[], "");
        req.body = Bytes::from_static(&[0xff, 0xfe, 0x00]);

        assert_eq!(
            format_request(&req),
            "> POST http://example.com/upload HTTP/1.1\n\
             \n\
             [3 bytes of binary data]\n"
        );
    }

    #[test]
    fn print_request_does_not_change_the_request() {
        let mut req = request("POST", "http://example.com/", &[("x-test", "yes")], "body");

        PrintRequest.request(&mut req).unwrap();

        assert_eq!(req.method, "POST");
        assert_eq!(req.uri, "http://example.com/");
        assert_eq!(req.headers["x-test"], "yes");
        assert_eq!(req.body, "body");
    }

    // --- PrintResponse ---

    #[test]
    fn print_response_shows_the_status_headers_body_and_which_request_it_answers() {
        let req = request("GET", "http://example.com/data", &[], "");
        let resp = response(404, &[("content-type", "text/plain")], "not here");

        assert_eq!(
            format_response(&req, &resp),
            "< HTTP/1.1 404 Not Found (GET http://example.com/data)\n\
             content-type: text/plain\n\
             \n\
             not here\n"
        );
    }

    #[test]
    fn print_response_does_not_change_the_response() {
        let req = request("GET", "http://example.com/", &[], "");
        let mut resp = response(200, &[("x-test", "yes")], "body");

        PrintResponse.response(&req, &mut resp).unwrap();

        assert_eq!(resp.status, 200);
        assert_eq!(resp.headers["x-test"], "yes");
        assert_eq!(resp.body, "body");
    }
}
