//! `swarmy web`: the dashboard's data in a browser, on this computer only.
//!
//! What it is and is not:
//! * Read-only. It answers `GET`, changes nothing on disk (the index aside, as every command
//!   does) and has no vault actions: switching logins stays in the terminal.
//! * Loopback only. It binds 127.0.0.1, accepts only `Host: 127.0.0.1:<port>` or
//!   `localhost:<port>` (a page on another site cannot reach it by DNS rebinding), and the page
//!   it serves may load nothing but itself (see `http::CSP`).
//! * Offline. The page is three embedded files; nothing is fetched from anywhere.
//!
//! Threads: one thread accepts; each connection is read on a short-lived thread of its own so
//! an idle speculative connection from a browser cannot block a real request; one worker
//! thread (the caller's) owns the index and answers the API.

mod api;
mod http;
mod timeline;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use chrono::Utc;

use crate::app::context::Ctx;
use crate::claude::project::Resolver;
use crate::claude::registry::{self, LiveSession};
use crate::core::harness::Harness;
use crate::index::{Index, Progress};
use crate::live;
use crate::procs::ProcTable;
use api::ApiError;
use http::{MAX_HEAD, Request, Response};

const INDEX_HTML: &str = include_str!("assets/index.html");
const APP_JS: &str = include_str!("assets/app.js");
const APP_CSS: &str = include_str!("assets/app.css");

/// Connections being read at once; more are dropped (a browser opens a handful).
const MAX_READERS: usize = 32;
const READ_TIMEOUT: Duration = Duration::from_secs(2);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// API calls closer together than this share one refresh.
const MIN_REFRESH_GAP: Duration = Duration::from_millis(1500);
/// Transcripts of every seat are scanned again this often; in between only the live ones.
const FULL_REFRESH_EVERY: Duration = Duration::from_secs(60);
const SEAT_INFO_EVERY: Duration = Duration::from_secs(10);
/// Session totals and the recently-done list are carried over between snapshots this long.
const HEAVY_EVERY: Duration = Duration::from_secs(10);

/// Everything the API reads: the context, the index and the last live snapshot. Owned by the
/// worker thread alone.
pub struct Hub {
    pub(crate) ctx: Ctx,
    pub(crate) idx: Index,
    resolver: Resolver,
    sys: Option<sysinfo::System>,
    live_sessions: Vec<LiveSession>,
    reads: live::Reads,
    pub(crate) snapshot: Option<live::Snapshot>,
    /// `None`: not yet.
    refreshed: Option<Instant>,
    full: Instant,
    seat_info: Instant,
    heavy: Option<Instant>,
}

impl Hub {
    fn new(ctx: Ctx) -> anyhow::Result<Hub> {
        // The first pass reads every transcript: it shows its progress on stderr.
        let (idx, _) = ctx.index()?;
        let resolver = Resolver::new(ctx.config.project.clone());
        let mut hub = Hub {
            ctx,
            idx,
            resolver,
            sys: sysinfo::System::new().ok(),
            live_sessions: Vec::new(),
            reads: live::Reads::default(),
            snapshot: None,
            refreshed: None,
            full: Instant::now(),
            seat_info: Instant::now(),
            heavy: None,
        };
        hub.refresh()?;
        Ok(hub)
    }

    /// Brings the data up to date: the clock, the seats' logins and limit caches, the running
    /// sessions and their processes, the index, and the live snapshot. Calls closer together
    /// than [`MIN_REFRESH_GAP`] share one pass.
    fn refresh(&mut self) -> anyhow::Result<()> {
        if self
            .refreshed
            .is_some_and(|t| t.elapsed() < MIN_REFRESH_GAP)
        {
            return Ok(());
        }
        self.refreshed = Some(Instant::now());
        self.ctx.now = Utc::now();
        self.idx.set_now(self.ctx.now);
        if self.seat_info.elapsed() >= SEAT_INFO_EVERY {
            self.ctx.reload_seat_info()?;
            self.seat_info = Instant::now();
        }
        let claude_seats = || {
            self.ctx
                .seats
                .iter()
                .filter(|s| s.harness == Harness::Claude)
        };
        let pids: Vec<u32> = claude_seats()
            .flat_map(|s| registry::session_pids(&s.sessions_dir()))
            .collect();
        if let Some(sys) = self.sys.as_mut() {
            self.ctx.procs = ProcTable::for_pids(sys, &pids);
        }
        self.live_sessions = claude_seats()
            .flat_map(|s| registry::live_sessions(&s.id, &s.sessions_dir(), &self.ctx.procs))
            .collect();
        if self.full.elapsed() >= FULL_REFRESH_EVERY {
            self.full = Instant::now();
            let mut quiet = |_p: Progress| {};
            self.idx.refresh(
                &self.ctx.seats,
                &self.ctx.seat_info,
                &mut self.resolver,
                &self.ctx.prices,
                &mut quiet,
            )?;
        } else {
            let keys: Vec<_> = self
                .live_sessions
                .iter()
                .map(|l| (l.seat.clone(), l.session_id.to_string()))
                .collect();
            self.idx.refresh_hot(
                &self.ctx.seats,
                &keys,
                &self.ctx.seat_info,
                &mut self.resolver,
                &self.ctx.prices,
            )?;
        }
        let heavy = self.heavy.is_none_or(|t| t.elapsed() >= HEAVY_EVERY);
        let opts = live::Options {
            stall_minutes: self.ctx.config.stall_minutes() as i64,
            ..Default::default()
        };
        let snap = live::build(
            &self.idx,
            &self.live_sessions,
            &self.ctx.procs,
            &opts,
            self.ctx.now,
            if heavy { None } else { self.snapshot.as_ref() },
            &mut self.reads,
        )?;
        if heavy {
            self.heavy = Some(Instant::now());
        }
        self.snapshot = Some(snap);
        Ok(())
    }
}

/// The page and its two files: no data, no state, answered without the worker.
fn static_response(path: &str) -> Option<Response> {
    // Browsers ask for this on their own; "no icon" is a quiet answer, a 404 is console noise.
    if path == "/favicon.ico" {
        return Some(Response::new(204, "image/x-icon", Vec::new()));
    }
    let (kind, body) = match path {
        "/" | "/index.html" => ("text/html; charset=utf-8", INDEX_HTML),
        "/app.js" => ("text/javascript; charset=utf-8", APP_JS),
        "/app.css" => ("text/css; charset=utf-8", APP_CSS),
        _unknown => return None,
    };
    Some(Response::new(200, kind, body))
}

/// What a reader thread decides about a request before the worker sees it.
enum Routed {
    Done(Response),
    /// An API call that needs the worker's data.
    Api(Request),
}

fn route(req: Request, port: u16) -> Routed {
    if !http::host_allowed(req.host.as_deref(), port) {
        return Routed::Done(Response::error(
            421,
            "this server answers only to 127.0.0.1 and localhost",
        ));
    }
    // The page's own fetches are same-origin. A page on another site reaching for the data
    // (its read is blocked anyway, but it should not even be answered) says `cross-site`.
    if req.path.starts_with("/api/") && req.fetch_site.as_deref() == Some("cross-site") {
        return Routed::Done(Response::error(403, "cross-site requests are refused"));
    }
    if req.method != "GET" {
        let mut r = Response::error(405, "read-only: only GET is accepted");
        r.extra.push("Allow: GET");
        return Routed::Done(r);
    }
    if let Some(r) = static_response(&req.path) {
        return Routed::Done(r);
    }
    match req.path.as_str() {
        "/api/glossary" => Routed::Done(Response::json(200, &api::glossary())),
        "/api/status" | "/api/live" | "/api/limits" | "/api/spend" | "/api/agent" => {
            Routed::Api(req)
        }
        _unknown => Routed::Done(Response::error(404, "not found")),
    }
}

/// Answers an API call on the worker thread.
fn answer(hub: &mut Hub, req: &Request) -> Response {
    let result = match req.path.as_str() {
        "/api/status" => api::status(hub),
        path => match hub.refresh() {
            Err(e) => Err(ApiError::Internal(e)),
            Ok(()) => match path {
                "/api/live" => api::live(hub),
                "/api/limits" => api::limits(hub),
                "/api/spend" => api::spend(hub, req),
                "/api/agent" => api::agent(hub, req),
                other => Err(ApiError::BadRequest(format!("no such call: {other}"))),
            },
        },
    };
    match result {
        Ok(v) => Response::json(200, &v),
        Err(ApiError::BadRequest(m)) => Response::error(400, &m),
        Err(ApiError::NotFound(m)) => Response::error(404, &m),
        Err(ApiError::Internal(e)) => Response::error(500, &format!("{e:#}")),
    }
}

enum Head {
    Complete(String),
    TooLarge,
    /// The client went away, sent nothing, or was too slow.
    Gone,
}

fn read_head(stream: &mut TcpStream) -> Head {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            return Head::Complete(String::from_utf8_lossy(&buf[..end + 2]).into_owned());
        }
        if buf.len() > MAX_HEAD {
            return Head::TooLarge;
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return Head::Gone,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
}

fn send(stream: &mut TcpStream, r: &Response) {
    // A client that left in the meantime is not an error worth reporting.
    let _ = stream.write_all(&r.to_bytes());
    let _ = stream.flush();
}

/// Reads one request off a connection, then either answers it or hands it to the worker.
fn read_and_route(mut stream: TcpStream, port: u16, jobs: &mpsc::Sender<(TcpStream, Request)>) {
    if stream.set_read_timeout(Some(READ_TIMEOUT)).is_err()
        || stream.set_write_timeout(Some(WRITE_TIMEOUT)).is_err()
    {
        return;
    }
    let head = match read_head(&mut stream) {
        Head::Complete(h) => h,
        Head::TooLarge => return send(&mut stream, &Response::error(431, "request too large")),
        Head::Gone => return,
    };
    let Ok(req) = http::parse_request(&head) else {
        return send(&mut stream, &Response::error(400, "malformed request"));
    };
    match route(req, port) {
        Routed::Done(r) => send(&mut stream, &r),
        Routed::Api(req) => {
            // A closed channel means the worker is gone: the process is ending.
            let _ = jobs.send((stream, req));
        }
    }
}

/// Serves until the process is stopped. Prints the address on stderr when it is listening.
pub fn serve(ctx: Ctx, port: u16) -> anyhow::Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port)).map_err(|e| {
        anyhow::anyhow!("cannot listen on 127.0.0.1:{port}: {e}. Another program may use the port: try --port <other>")
    })?;
    let port = listener.local_addr()?.port();
    let mut hub = Hub::new(ctx)?;
    eprintln!("swarmy web: http://127.0.0.1:{port}   (this computer only · Ctrl-C stops it)");
    let (tx, rx) = mpsc::channel::<(TcpStream, Request)>();
    let readers = Arc::new(AtomicUsize::new(0));
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            if readers.fetch_add(1, Ordering::AcqRel) >= MAX_READERS {
                readers.fetch_sub(1, Ordering::AcqRel);
                continue;
            }
            let (tx, readers) = (tx.clone(), Arc::clone(&readers));
            std::thread::spawn(move || {
                read_and_route(conn, port, &tx);
                readers.fetch_sub(1, Ordering::AcqRel);
            });
        }
    });
    for (mut stream, req) in rx {
        let response = answer(&mut hub, &req);
        send(&mut stream, &response);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get(path: &str, host: Option<&str>) -> Request {
        Request {
            method: "GET".into(),
            path: path.into(),
            query: Vec::new(),
            host: host.map(str::to_string),
            fetch_site: None,
        }
    }

    fn done(r: Routed) -> Response {
        match r {
            Routed::Done(r) => r,
            Routed::Api(req) => panic!("not answered by the reader: {}", req.path),
        }
    }

    /// Every call that needs the worker's data. A new call is added here once, and every guard
    /// below covers it.
    const DATA_CALLS: [&str; 5] = [
        "/api/status",
        "/api/live",
        "/api/limits",
        "/api/spend",
        "/api/agent",
    ];

    #[test]
    fn a_foreign_host_is_refused_before_anything_else() {
        for path in DATA_CALLS {
            let r = done(route(get(path, Some("evil.example:7878")), 7878));
            assert_eq!(r.status, 421, "{path}");
        }
        let r = done(route(get("/", None), 7878));
        assert_eq!(r.status, 421);
    }

    #[test]
    fn a_cross_site_page_cannot_reach_the_api_but_can_load_the_page() {
        for path in DATA_CALLS {
            let mut api = get(path, Some("127.0.0.1:7878"));
            api.fetch_site = Some("cross-site".into());
            assert_eq!(done(route(api, 7878)).status, 403, "{path}");
            // Same-origin (the page's own fetch) and a typed address (`none`) pass.
            for site in ["same-origin", "none"] {
                let mut ok = get(path, Some("127.0.0.1:7878"));
                ok.fetch_site = Some(site.into());
                assert!(matches!(route(ok, 7878), Routed::Api(_)), "{path} {site}");
            }
        }
        // A link from elsewhere to the page itself is a navigation, not a data read.
        let mut page = get("/", Some("127.0.0.1:7878"));
        page.fetch_site = Some("cross-site".into());
        assert_eq!(done(route(page, 7878)).status, 200);
    }

    #[test]
    fn only_get_is_accepted() {
        for path in DATA_CALLS {
            for method in ["POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"] {
                let mut req = get(path, Some("localhost:7878"));
                req.method = method.into();
                let r = done(route(req, 7878));
                assert_eq!(r.status, 405, "{method} {path}");
                assert!(r.extra.contains(&"Allow: GET"));
            }
        }
    }

    #[test]
    fn the_page_and_the_glossary_need_no_worker() {
        for (path, kind) in [
            ("/", "text/html"),
            ("/app.js", "text/javascript"),
            ("/app.css", "text/css"),
            ("/api/glossary", "application/json"),
        ] {
            let r = done(route(get(path, Some("127.0.0.1:7878")), 7878));
            assert_eq!(r.status, 200, "{path}");
            assert!(r.content_type.starts_with(kind), "{path}");
            assert!(!r.body.is_empty(), "{path}");
        }
    }

    #[test]
    fn data_calls_go_to_the_worker_and_the_rest_is_not_found() {
        for path in DATA_CALLS {
            let routed = route(get(path, Some("localhost:7878")), 7878);
            assert!(matches!(routed, Routed::Api(_)), "{path}");
        }
        for path in [
            "/api",
            "/api/",
            "/api/vault",
            "/api/agent/",
            "/api/agents",
            "/etc/passwd",
            "/../Cargo.toml",
        ] {
            let r = done(route(get(path, Some("localhost:7878")), 7878));
            assert_eq!(r.status, 404, "{path}");
        }
    }

    /// The page never builds markup from data with `innerHTML`: names and paths come from
    /// transcripts and are not trusted.
    #[test]
    fn the_page_script_never_writes_html_from_data() {
        for forbidden in [
            "innerHTML",
            "outerHTML",
            "insertAdjacentHTML",
            "document.write",
            "eval(",
        ] {
            assert!(!APP_JS.contains(forbidden), "app.js uses {forbidden}");
        }
        assert!(
            !INDEX_HTML.contains("<script>"),
            "no inline script (the CSP forbids it)"
        );
        assert!(!INDEX_HTML.contains("onclick"), "no inline handlers");
    }

    #[test]
    fn the_page_never_loads_anything_from_another_site() {
        for text in [INDEX_HTML, APP_JS, APP_CSS] {
            assert!(!text.contains("http://") && !text.contains("https://"));
            assert!(!text.contains("@import"));
        }
    }

    /// The page looks terms up by name for its tooltips; a renamed term must not silently
    /// leave a tooltip empty.
    #[test]
    fn every_term_the_page_looks_up_exists_in_the_glossary() {
        let mut wanted: Vec<&str> = Vec::new();
        // tip('name')
        for part in APP_JS.split("tip('").skip(1) {
            if let Some((name, _)) = part.split_once("')") {
                wanted.push(name);
            }
        }
        // The third entry of each STATES row: ['●', 'working', 'working'].
        let states = APP_JS
            .split("const STATES = {")
            .nth(1)
            .and_then(|r| r.split("};").next())
            .unwrap_or_default();
        for line in states.lines().filter(|l| l.contains("['")) {
            let quoted: Vec<&str> = line.split('\'').skip(1).step_by(2).collect();
            if let Some(term) = quoted.get(2) {
                wanted.push(term);
            }
        }
        assert!(wanted.len() > 10, "the scan found too little: {wanted:?}");
        for name in wanted {
            assert!(
                crate::ui::glossary::by_name(name).is_some(),
                "app.js looks up `{name}`, which the glossary does not have"
            );
        }
    }

    /// The page may only read. One `fetch`, with no method, body or credentials of its own, and
    /// no other way to send anything anywhere.
    #[test]
    fn the_page_can_only_read() {
        assert_eq!(
            APP_JS.matches("fetch(").count(),
            1,
            "one place asks the server"
        );
        for forbidden in [
            "method:",
            "body:",
            "XMLHttpRequest",
            "WebSocket",
            "EventSource",
            "sendBeacon",
            "importScripts",
            "navigator.serviceWorker",
        ] {
            assert!(!APP_JS.contains(forbidden), "app.js uses {forbidden}");
        }
    }

    /// Every call the page makes is one the server answers, and a path the page builds from
    /// data does not exist: the page names its calls (`api('agent', ...)`), the server routes
    /// them.
    #[test]
    fn the_page_calls_only_routes_the_server_has() {
        let mut named: Vec<&str> = Vec::new();
        for part in APP_JS.split("api('").skip(1) {
            if let Some((name, _)) = part.split_once('\'') {
                named.push(name);
            }
        }
        assert!(named.contains(&"agent"), "the timeline call: {named:?}");
        for name in named {
            let path = format!("/api/{name}");
            let routed = route(get(&path, Some("127.0.0.1:7878")), 7878);
            let known = match routed {
                Routed::Api(_) => true,
                Routed::Done(r) => r.status == 200,
            };
            assert!(
                known,
                "app.js calls {path}, which the server does not answer"
            );
        }
        // Every call site (not the definition) starts with a string: a name, never a path built
        // from data.
        for (at, _) in APP_JS.match_indices("api(") {
            if APP_JS[..at].ends_with("function ") {
                continue;
            }
            assert!(
                APP_JS[at + 4..].starts_with('\''),
                "a call to api() is named by a string literal: {}",
                APP_JS[at..].lines().next().unwrap_or_default()
            );
        }
    }

    /// Text from transcripts goes into the page as text nodes, and style only through the
    /// CSSOM, which the CSP allows; a style attribute written as a string would be blocked, and
    /// markup parsed from a string would be the hole `innerHTML` is.
    #[test]
    fn the_page_has_no_other_way_to_write_markup_or_style() {
        for forbidden in [
            "DOMParser",
            "createContextualFragment",
            "srcdoc",
            "cssText",
            "setAttribute('style'",
            "setAttribute(\"style\"",
            "new Function",
            "style: '",
            "style: \"",
            "style: `",
            ".href =",
            "location.assign",
            "location =",
        ] {
            assert!(!APP_JS.contains(forbidden), "app.js uses {forbidden}");
        }
        assert!(
            !INDEX_HTML.contains("style="),
            "no inline style (the CSP forbids it)"
        );
        assert!(
            !INDEX_HTML.contains("<style"),
            "no inline style (the CSP forbids it)"
        );
    }

    /// The CSP is the same on the page, its two files, the data and the errors, and it is the
    /// one the page was written for: nothing inline, nothing from another site. Loosening it for
    /// a feature is a change to this test, which a reviewer sees.
    #[test]
    fn the_csp_is_unchanged_and_on_every_kind_of_answer() {
        const CSP: &str = "Content-Security-Policy: default-src 'none'; script-src 'self'; \
            style-src 'self'; connect-src 'self'; img-src 'self' data:; base-uri 'none'; \
            form-action 'none'; frame-ancestors 'none'\r\n";
        let host = Some("127.0.0.1:7878");
        let mut answers: Vec<Response> = ["/", "/app.js", "/app.css", "/api/glossary", "/nope"]
            .iter()
            .map(|p| done(route(get(p, host), 7878)))
            .collect();
        answers.push(Response::error(500, "x"));
        answers.push(Response::json(200, &serde_json::json!({ "tools": [] })));
        for r in answers {
            let text = String::from_utf8_lossy(&r.to_bytes()).into_owned();
            assert!(text.contains(CSP), "{}: {text}", r.status);
        }
    }
}
