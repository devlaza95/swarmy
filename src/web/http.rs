//! The little HTTP the web view speaks: `GET` only, one request per connection, nothing read
//! beyond the request line and the `Host` header. Pure functions over text and bytes, so every
//! rule here is table-tested without a socket.

/// A request head longer than this is refused (431) before it is parsed.
pub const MAX_HEAD: usize = 8 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub host: Option<String>,
    /// `Sec-Fetch-Site`, which browsers send and a page cannot forge: `same-origin`, `none`
    /// (typed address), `cross-site`...
    pub fetch_site: Option<String>,
}

impl Request {
    /// The first value of a query parameter.
    pub fn param(&self, name: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
            .filter(|v| !v.is_empty())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Malformed;

/// `%41` → `A`, `+` → space. A bad escape stays as typed; bytes that are not UTF-8 are replaced.
pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' => match (
                b.get(i + 1).copied().and_then(hex),
                b.get(i + 2).copied().and_then(hex),
            ) {
                (Some(h), Some(l)) => {
                    out.push(h * 16 + l);
                    i += 2;
                }
                // Not two hex digits: keep the percent sign as typed.
                (None, _) | (_, None) => out.push(b'%'),
            },
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(c: u8) -> Option<u8> {
    (c as char).to_digit(16).map(|d| d as u8)
}

/// Parses a request head (everything before the blank line).
pub fn parse_request(head: &str) -> Result<Request, Malformed> {
    let mut lines = head.split("\r\n");
    let first = lines.next().ok_or(Malformed)?;
    let mut parts = first.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(Malformed);
    };
    if !version.starts_with("HTTP/1.") || method.is_empty() {
        return Err(Malformed);
    }
    // Origin-form only: no absolute URLs, no `*`, no `//host` tricks.
    if !target.starts_with('/') || target.starts_with("//") {
        return Err(Malformed);
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let mut host = None;
    let mut fetch_site = None;
    for line in lines {
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':').ok_or(Malformed)?;
        if name.eq_ignore_ascii_case("host") {
            // Two Host headers are ambiguous: refuse rather than pick one.
            if host.replace(value.trim().to_string()).is_some() {
                return Err(Malformed);
            }
        } else if name.eq_ignore_ascii_case("sec-fetch-site") {
            fetch_site = Some(value.trim().to_ascii_lowercase());
        }
    }
    let query = query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect();
    Ok(Request {
        method: method.to_string(),
        path: percent_decode(path),
        query,
        host,
        fetch_site,
    })
}

/// Only this computer's own names for the server are accepted. A page on another site that is
/// pointed at 127.0.0.1 by DNS (rebinding) still sends its own name in `Host`, and is refused.
pub fn host_allowed(host: Option<&str>, port: u16) -> bool {
    let Some(h) = host else { return false };
    let h = h.to_ascii_lowercase();
    h == format!("127.0.0.1:{port}") || h == format!("localhost:{port}")
}

pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
    /// Extra header lines, each without the line break.
    pub extra: Vec<&'static str>,
}

const REASONS: &[(u16, &str)] = &[
    (200, "OK"),
    (400, "Bad Request"),
    (403, "Forbidden"),
    (404, "Not Found"),
    (405, "Method Not Allowed"),
    (421, "Misdirected Request"),
    (431, "Request Header Fields Too Large"),
    (500, "Internal Server Error"),
];

/// What a page may load: only itself. No inline script, no other site, no framing.
const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; \
                   img-src 'self' data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

impl Response {
    pub fn new(status: u16, content_type: &'static str, body: impl Into<Vec<u8>>) -> Response {
        Response {
            status,
            content_type,
            body: body.into(),
            extra: Vec::new(),
        }
    }

    pub fn json(status: u16, value: &serde_json::Value) -> Response {
        let body = serde_json::to_vec(value).unwrap_or_else(|_| b"null".to_vec());
        Response::new(status, "application/json; charset=utf-8", body)
    }

    pub fn error(status: u16, message: &str) -> Response {
        Response::json(status, &serde_json::json!({ "error": message }))
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let reason = REASONS
            .iter()
            .find(|(c, _)| *c == self.status)
            .map(|(_, r)| *r)
            .unwrap_or("Status");
        let mut head = format!(
            "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\n\
             Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\
             X-Frame-Options: DENY\r\nReferrer-Policy: no-referrer\r\n\
             Cross-Origin-Resource-Policy: same-origin\r\n\
             Content-Security-Policy: {CSP}\r\nConnection: close\r\n",
            self.status,
            self.content_type,
            self.body.len()
        );
        for line in &self.extra {
            head.push_str(line);
            head.push_str("\r\n");
        }
        head.push_str("\r\n");
        let mut out = head.into_bytes();
        out.extend_from_slice(&self.body);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(target: &str, host: &str) -> String {
        format!("GET {target} HTTP/1.1\r\nHost: {host}\r\nAccept: */*\r\n\r\n")
    }

    #[test]
    fn a_request_gives_its_path_query_and_host() {
        let r = parse_request(&req("/api/spend?by=project&period=7d", "localhost:7878")).unwrap();
        assert_eq!(r.method, "GET");
        assert_eq!(r.path, "/api/spend");
        assert_eq!(r.param("by"), Some("project"));
        assert_eq!(r.param("period"), Some("7d"));
        assert_eq!(r.param("seat"), None);
        assert_eq!(r.host.as_deref(), Some("localhost:7878"));
    }

    #[test]
    fn percent_escapes_and_plus_decode_and_bad_escapes_survive() {
        assert_eq!(percent_decode("a%20b+c"), "a b c");
        assert_eq!(percent_decode("%E2%89%88"), "≈");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_decode("%4"), "%4");
    }

    #[test]
    fn an_empty_value_is_no_value() {
        let r = parse_request(&req("/x?project=&by=day", "localhost:1")).unwrap();
        assert_eq!(r.param("project"), None);
        assert_eq!(r.param("by"), Some("day"));
    }

    #[test]
    fn only_origin_form_http_1_requests_parse() {
        for bad in [
            "GET http://evil.example/ HTTP/1.1\r\nHost: localhost:1\r\n\r\n",
            "GET //evil.example/ HTTP/1.1\r\nHost: localhost:1\r\n\r\n",
            "GET * HTTP/1.1\r\nHost: localhost:1\r\n\r\n",
            "GET / HTTP/2\r\nHost: localhost:1\r\n\r\n",
            "GET /\r\nHost: localhost:1\r\n\r\n",
            "GET / HTTP/1.1 extra\r\nHost: localhost:1\r\n\r\n",
            "GET  / HTTP/1.1\r\nHost: localhost:1\r\n\r\n",
            "GET / HTTP/1.1\r\nno colon here\r\n\r\n",
            "",
        ] {
            assert_eq!(parse_request(bad), Err(Malformed), "{bad:?}");
        }
    }

    #[test]
    fn the_fetch_site_header_is_read_in_any_case() {
        let r = parse_request("GET / HTTP/1.1\r\nHost: h:1\r\nSec-Fetch-Site: Cross-Site\r\n\r\n")
            .unwrap();
        assert_eq!(r.fetch_site.as_deref(), Some("cross-site"));
        let r = parse_request("GET / HTTP/1.1\r\nHost: h:1\r\n\r\n").unwrap();
        assert_eq!(r.fetch_site, None);
    }

    #[test]
    fn two_host_headers_are_refused() {
        let head = "GET / HTTP/1.1\r\nHost: localhost:1\r\nhost: evil.example\r\n\r\n";
        assert_eq!(parse_request(head), Err(Malformed));
    }

    #[test]
    fn header_names_are_case_insensitive() {
        let r = parse_request("GET / HTTP/1.1\r\nHOST: 127.0.0.1:9\r\n\r\n").unwrap();
        assert_eq!(r.host.as_deref(), Some("127.0.0.1:9"));
    }

    /// DNS rebinding: the page's own name arrives in `Host`.
    #[test]
    fn only_this_computers_names_pass_the_host_check() {
        assert!(host_allowed(Some("127.0.0.1:7878"), 7878));
        assert!(host_allowed(Some("localhost:7878"), 7878));
        assert!(host_allowed(Some("LocalHost:7878"), 7878));
        for bad in [
            None,
            Some("evil.example:7878"),
            Some("evil.example"),
            Some("127.0.0.1"),
            Some("127.0.0.1:7879"),
            Some("localhost.evil.example:7878"),
            Some("127.0.0.1:7878.evil.example"),
            Some(""),
        ] {
            assert!(!host_allowed(bad, 7878), "{bad:?}");
        }
    }

    #[test]
    fn a_response_carries_length_and_the_protective_headers() {
        let bytes = Response::new(200, "text/plain", "hi").to_bytes();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"), "{text}");
        assert!(text.contains("Content-Length: 2\r\n"));
        assert!(text.contains("X-Content-Type-Options: nosniff\r\n"));
        assert!(text.contains("Cache-Control: no-store\r\n"));
        assert!(text.contains("frame-ancestors 'none'"));
        assert!(text.contains("Cross-Origin-Resource-Policy: same-origin\r\n"));
        assert!(text.contains("script-src 'self'"));
        assert!(!text.contains("unsafe-inline"));
        assert!(text.ends_with("\r\n\r\nhi"));
    }

    #[test]
    fn an_error_is_json_with_its_status_line() {
        let text = String::from_utf8(Response::error(421, "no").to_bytes()).unwrap();
        assert!(
            text.starts_with("HTTP/1.1 421 Misdirected Request\r\n"),
            "{text}"
        );
        assert!(text.ends_with("{\"error\":\"no\"}"));
    }
}
