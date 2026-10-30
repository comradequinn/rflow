use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use bytes::Bytes;
use hyper::header::{self, HeaderName, HeaderValue};
use hyper::{HeaderMap, Method, StatusCode, Uri, Version};

use super::{Interceptor, Request, Response, with_terminal};
use crate::{Error, write_private};

/// Shows a serialised message to the user and returns their edited version, or
/// `None` to keep the original unchanged.
type Editor = Box<dyn Fn(&[u8]) -> Result<Option<Vec<u8>>, Error> + Send + Sync>;

/// Pauses requests whose URI contains `pattern` and opens them in an editor so
/// they can be changed before being sent.
pub struct EditRequest {
    pattern: String,
    editor: Editor,
}

/// Pauses responses to requests whose URI contains `pattern` and opens them in
/// an editor so they can be changed before being returned to the client.
pub struct EditResponse {
    pattern: String,
    editor: Editor,
}

impl EditRequest {
    /// `path` is the file the request is written to while it is edited.
    pub fn new(pattern: String, path: PathBuf) -> Self {
        Self::with_editor(pattern, move |contents| edit_in_editor(&path, contents))
    }

    fn with_editor(
        pattern: impl Into<String>,
        editor: impl Fn(&[u8]) -> Result<Option<Vec<u8>>, Error> + Send + Sync + 'static,
    ) -> Self {
        Self { pattern: pattern.into(), editor: Box::new(editor) }
    }
}

impl EditResponse {
    /// `path` is the file the response is written to while it is edited.
    pub fn new(pattern: String, path: PathBuf) -> Self {
        Self::with_editor(pattern, move |contents| edit_in_editor(&path, contents))
    }

    fn with_editor(
        pattern: impl Into<String>,
        editor: impl Fn(&[u8]) -> Result<Option<Vec<u8>>, Error> + Send + Sync + 'static,
    ) -> Self {
        Self { pattern: pattern.into(), editor: Box::new(editor) }
    }
}

impl Interceptor for EditRequest {
    fn request(&self, req: &mut Request) -> Result<(), Error> {
        if !req.uri.to_string().contains(&self.pattern) {
            return Ok(());
        }
        let start = format!("{} {} {:?}", req.method, req.uri, req.version);
        let original = serialise(&start, &req.headers, &req.body);
        if let Some(edited) = open_editor(&self.editor, &original)? {
            *req = parse_request(&edited)?;
        }
        Ok(())
    }
}

impl Interceptor for EditResponse {
    fn response(&self, req: &Request, resp: &mut Response) -> Result<(), Error> {
        if !req.uri.to_string().contains(&self.pattern) {
            return Ok(());
        }
        let start = format!("{:?} {}", resp.version, resp.status);
        let original = serialise(&start, &resp.headers, &resp.body);
        if let Some(edited) = open_editor(&self.editor, &original)? {
            *resp = parse_response(&edited)?;
        }
        Ok(())
    }
}

/// Shows `original` in the editor. Most editors add a newline to the end of
/// the file when saving, so one is removed if the original didn't have it, to
/// stop it being added to the body.
fn open_editor(editor: &Editor, original: &[u8]) -> Result<Option<Vec<u8>>, Error> {
    let mut edited = with_terminal(|| editor(original))?;
    if let Some(edited) = &mut edited
        && !original.ends_with(b"\n")
        && edited.ends_with(b"\n")
    {
        edited.pop();
    }
    Ok(edited)
}

/// Opens `contents` in `$EDITOR`, or vim if it is unset, and returns the saved
/// result, or `None` if the editor exited with an error (e.g. vim's `:cq`) to
/// keep the original unchanged.
fn edit_in_editor(path: &Path, contents: &[u8]) -> Result<Option<Vec<u8>>, Error> {
    write_private(path, contents)?;
    let status = editor_command().arg(path).status();
    let edited = fs::read(path);
    let _ = fs::remove_file(path);
    if !status?.success() {
        return Ok(None);
    }
    Ok(Some(edited?))
}

/// `$EDITOR` may include arguments, e.g. `code --wait`.
fn editor_command() -> Command {
    match std::env::var("EDITOR") {
        Ok(editor) if !editor.trim().is_empty() => {
            let mut parts = editor.split_whitespace();
            let mut command = Command::new(parts.next().unwrap_or_default());
            command.args(parts);
            command
        }
        _ => Command::new("vim"),
    }
}

/// Writes the start line, then headers, a blank line and the body.
fn serialise(start: &str, headers: &HeaderMap, body: &Bytes) -> Vec<u8> {
    let mut out = format!("{start}\n").into_bytes();
    for (name, value) in headers {
        out.extend_from_slice(name.as_str().as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(value.as_bytes());
        out.push(b'\n');
    }
    out.push(b'\n');
    out.extend_from_slice(body);
    out
}

/// Parses a request written by `serialise`: `METHOD URI VERSION`.
fn parse_request(data: &[u8]) -> Result<Request, Error> {
    let (start, headers, body) = parse_message(data)?;
    let mut parts = start.split_whitespace();
    let (Some(method), Some(uri), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err("first line must be: METHOD URI VERSION".into());
    };
    let uri: Uri = uri.parse()?;
    if uri.scheme().is_none() || uri.authority().is_none() {
        return Err("URI must be absolute".into());
    }
    Ok(Request {
        method: Method::from_bytes(method.as_bytes())?,
        uri,
        version: parse_version(version)?,
        headers,
        body,
    })
}

/// Parses a response written by `serialise`: `VERSION STATUS [REASON]`.
fn parse_response(data: &[u8]) -> Result<Response, Error> {
    let (start, headers, body) = parse_message(data)?;
    let mut parts = start.split_whitespace();
    let (Some(version), Some(status)) = (parts.next(), parts.next()) else {
        return Err("first line must be: VERSION STATUS".into());
    };
    Ok(Response {
        status: StatusCode::from_bytes(status.as_bytes())?,
        version: parse_version(version)?,
        headers,
        body,
    })
}

/// Splits a message into its start line, headers and body.
fn parse_message(data: &[u8]) -> Result<(&str, HeaderMap, Bytes), Error> {
    let (head, body) = match data.windows(2).position(|w| w == b"\n\n") {
        Some(i) => (&data[..i], &data[i + 2..]),
        None => (data, &[][..]),
    };
    let mut lines = std::str::from_utf8(head)?.lines();
    let start = lines.next().unwrap_or_default();

    let mut headers = HeaderMap::new();
    for line in lines {
        let (name, value) = line.split_once(':').ok_or_else(|| format!("invalid header: {line}"))?;
        headers.append(HeaderName::try_from(name.trim())?, HeaderValue::try_from(value.trim())?);
    }
    // The body may have changed size, so let it be recalculated when sent.
    headers.remove(header::CONTENT_LENGTH);

    Ok((start, headers, Bytes::copy_from_slice(body)))
}

fn parse_version(version: &str) -> Result<Version, Error> {
    match version {
        "HTTP/1.0" => Ok(Version::HTTP_10),
        "HTTP/1.1" => Ok(Version::HTTP_11),
        "HTTP/2.0" => Ok(Version::HTTP_2),
        other => Err(format!("unsupported version: {other}").into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interceptor::testing::{request, response};

    /// An editor result as if the user saved `text` and quit.
    fn saved(text: &str) -> Result<Option<Vec<u8>>, Error> {
        Ok(Some(text.as_bytes().to_vec()))
    }

    /// An editor result as if the user quit with `:cq`.
    fn cancelled() -> Result<Option<Vec<u8>>, Error> {
        Ok(None)
    }

    fn never_opened(_: &[u8]) -> Result<Option<Vec<u8>>, Error> {
        panic!("the editor should not have been opened");
    }

    // --- EditRequest ---

    #[test]
    fn edit_request_ignores_requests_whose_url_does_not_match() {
        let edit = EditRequest::with_editor("example.com", never_opened);
        let mut req = request("GET", "http://other.org/", &[], "");

        edit.request(&mut req).unwrap();

        assert_eq!(req.uri, "http://other.org/");
    }

    #[test]
    fn edit_request_matches_any_part_of_the_url() {
        let edit = EditRequest::with_editor("ample.co", |_| saved("GET http://example.com/edited HTTP/1.1\n\n"));
        let mut req = request("GET", "http://www.example.com/", &[], "");

        edit.request(&mut req).unwrap();

        assert_eq!(req.uri, "http://example.com/edited");
    }

    #[test]
    fn edit_request_shows_the_full_request_in_the_editor() {
        let edit = EditRequest::with_editor("example.com", |shown| {
            assert_eq!(
                String::from_utf8_lossy(shown),
                "POST http://example.com/login HTTP/1.1\n\
                 content-type: text/plain\n\
                 \n\
                 user=bob"
            );
            cancelled()
        });
        let mut req = request("POST", "http://example.com/login", &[("content-type", "text/plain")], "user=bob");

        edit.request(&mut req).unwrap();
    }

    #[test]
    fn edit_request_removes_a_newline_the_editor_added_to_the_end() {
        let edit = EditRequest::with_editor("example.com", |_| {
            saved("POST http://example.com/ HTTP/1.1\n\nname=bob\n")
        });
        let mut req = request("POST", "http://example.com/", &[], "name=bob");

        edit.request(&mut req).unwrap();

        assert_eq!(req.body, "name=bob");
    }

    #[test]
    fn edit_request_keeps_a_newline_that_was_already_at_the_end() {
        let edit = EditRequest::with_editor("example.com", |_| {
            saved("POST http://example.com/ HTTP/1.1\n\nname=bob\n")
        });
        let mut req = request("POST", "http://example.com/", &[], "name=bob\n");

        edit.request(&mut req).unwrap();

        assert_eq!(req.body, "name=bob\n");
    }

    #[test]
    fn edit_request_replaces_the_request_with_the_saved_edit() {
        let edit = EditRequest::with_editor("example.com", |_| {
            saved(
                "PUT https://example.com/changed?a=1 HTTP/1.1\n\
                 x-added: yes\n\
                 \n\
                 new body",
            )
        });
        let mut req = request("GET", "http://example.com/", &[("x-removed", "yes")], "");

        edit.request(&mut req).unwrap();

        assert_eq!(req.method, "PUT");
        assert_eq!(req.uri, "https://example.com/changed?a=1");
        assert_eq!(req.headers["x-added"], "yes");
        assert!(!req.headers.contains_key("x-removed"));
        assert_eq!(req.body, "new body");
    }

    #[test]
    fn edit_request_keeps_the_original_when_the_edit_is_cancelled() {
        let edit = EditRequest::with_editor("example.com", |_| cancelled());
        let mut req = request("POST", "http://example.com/", &[], "original");

        edit.request(&mut req).unwrap();

        assert_eq!(req.method, "POST");
        assert_eq!(req.body, "original");
    }

    #[test]
    fn edit_request_drops_the_request_when_the_first_line_is_invalid() {
        let edit = EditRequest::with_editor("example.com", |_| saved("not a request line\n\n"));
        let mut req = request("GET", "http://example.com/", &[], "");

        let err = edit.request(&mut req).unwrap_err();

        assert_eq!(err.to_string(), "first line must be: METHOD URI VERSION");
    }

    #[test]
    fn edit_request_drops_the_request_when_the_url_is_not_absolute() {
        let edit = EditRequest::with_editor("example.com", |_| saved("GET /relative HTTP/1.1\n\n"));
        let mut req = request("GET", "http://example.com/", &[], "");

        let err = edit.request(&mut req).unwrap_err();

        assert_eq!(err.to_string(), "URI must be absolute");
    }

    #[test]
    fn edit_request_drops_the_request_when_a_header_has_no_colon() {
        let edit = EditRequest::with_editor("example.com", |_| saved("GET http://example.com/ HTTP/1.1\nbroken\n\n"));
        let mut req = request("GET", "http://example.com/", &[], "");

        let err = edit.request(&mut req).unwrap_err();

        assert_eq!(err.to_string(), "invalid header: broken");
    }

    #[test]
    fn edit_request_removes_content_length_so_it_matches_the_new_body() {
        let edit = EditRequest::with_editor("example.com", |_| {
            saved("POST http://example.com/ HTTP/1.1\ncontent-length: 3\n\nmuch longer body")
        });
        let mut req = request("POST", "http://example.com/", &[], "abc");

        edit.request(&mut req).unwrap();

        assert!(!req.headers.contains_key("content-length"));
        assert_eq!(req.body, "much longer body");
    }

    #[test]
    fn edit_request_keeps_colons_in_header_values() {
        let edit = EditRequest::with_editor("example.com", |_| {
            saved("GET http://example.com/ HTTP/1.1\nhost: example.com:8080\n\n")
        });
        let mut req = request("GET", "http://example.com/", &[], "");

        edit.request(&mut req).unwrap();

        assert_eq!(req.headers["host"], "example.com:8080");
    }

    #[test]
    fn edit_request_keeps_blank_lines_in_the_body() {
        let edit = EditRequest::with_editor("example.com", |_| {
            saved("POST http://example.com/ HTTP/1.1\n\nfirst paragraph\n\nsecond paragraph")
        });
        let mut req = request("POST", "http://example.com/", &[], "");

        edit.request(&mut req).unwrap();

        assert_eq!(req.body, "first paragraph\n\nsecond paragraph");
    }

    // --- EditResponse ---

    #[test]
    fn edit_response_ignores_responses_to_requests_whose_url_does_not_match() {
        let edit = EditResponse::with_editor("example.com", never_opened);
        let req = request("GET", "http://other.org/", &[], "");
        let mut resp = response(200, &[], "original");

        edit.response(&req, &mut resp).unwrap();

        assert_eq!(resp.body, "original");
    }

    #[test]
    fn edit_response_shows_the_full_response_in_the_editor() {
        let edit = EditResponse::with_editor("example.com", |shown| {
            assert_eq!(
                String::from_utf8_lossy(shown),
                "HTTP/1.1 200 OK\n\
                 content-type: application/json\n\
                 \n\
                 {\"ok\":true}"
            );
            cancelled()
        });
        let req = request("GET", "http://example.com/", &[], "");
        let mut resp = response(200, &[("content-type", "application/json")], "{\"ok\":true}");

        edit.response(&req, &mut resp).unwrap();
    }

    #[test]
    fn edit_response_removes_a_newline_the_editor_added_to_the_end() {
        let edit = EditResponse::with_editor("example.com", |_| saved("HTTP/1.1 200 OK\n\nhello\n"));
        let req = request("GET", "http://example.com/", &[], "");
        let mut resp = response(200, &[], "hello");

        edit.response(&req, &mut resp).unwrap();

        assert_eq!(resp.body, "hello");
    }

    #[test]
    fn edit_response_replaces_the_response_with_the_saved_edit() {
        let edit = EditResponse::with_editor("example.com", |_| {
            saved(
                "HTTP/1.1 418 I'm a teapot\n\
                 x-added: yes\n\
                 \n\
                 new body",
            )
        });
        let req = request("GET", "http://example.com/", &[], "");
        let mut resp = response(200, &[("x-removed", "yes")], "original");

        edit.response(&req, &mut resp).unwrap();

        assert_eq!(resp.status, 418);
        assert_eq!(resp.headers["x-added"], "yes");
        assert!(!resp.headers.contains_key("x-removed"));
        assert_eq!(resp.body, "new body");
    }

    #[test]
    fn edit_response_keeps_the_original_when_the_edit_is_cancelled() {
        let edit = EditResponse::with_editor("example.com", |_| cancelled());
        let req = request("GET", "http://example.com/", &[], "");
        let mut resp = response(200, &[], "original");

        edit.response(&req, &mut resp).unwrap();

        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, "original");
    }

    #[test]
    fn edit_response_drops_the_response_when_the_first_line_is_invalid() {
        let edit = EditResponse::with_editor("example.com", |_| saved("garbage\n\n"));
        let req = request("GET", "http://example.com/", &[], "");
        let mut resp = response(200, &[], "");

        let err = edit.response(&req, &mut resp).unwrap_err();

        assert_eq!(err.to_string(), "first line must be: VERSION STATUS");
    }

    #[test]
    fn edit_response_drops_the_response_when_the_status_is_not_a_number() {
        let edit = EditResponse::with_editor("example.com", |_| saved("HTTP/1.1 OK\n\n"));
        let req = request("GET", "http://example.com/", &[], "");
        let mut resp = response(200, &[], "");

        assert!(edit.response(&req, &mut resp).is_err());
    }

    #[test]
    fn edit_response_removes_content_length_so_it_matches_the_new_body() {
        let edit = EditResponse::with_editor("example.com", |_| {
            saved("HTTP/1.1 200 OK\ncontent-length: 3\n\nmuch longer body")
        });
        let req = request("GET", "http://example.com/", &[], "");
        let mut resp = response(200, &[], "abc");

        edit.response(&req, &mut resp).unwrap();

        assert!(!resp.headers.contains_key("content-length"));
        assert_eq!(resp.body, "much longer body");
    }
}
