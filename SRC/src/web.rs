// Part of the APK.audio project — http://APK.audio — made by Anthony Kuzub
// MIT Licence. Free, for everyone, for ever. Full text in LICENSE at the root.
//! The web door: `/status`, `/config`, `/matrix`, and the REST API they use.
//!
//! A PORT OF ITS OWN, AND WHY THIS PLUGIN IS THE EXCEPTION. Plugins describe
//! themselves read-only over the bus (`incoming/api`) behind the node's one
//! gateway, and do not open ports. This one is configured by a person — thirty-
//! two stream slots, device pickers, SDP pasted from another vendor's page —
//! which needs a writable store (its volume) and a form, the two things that
//! rule was waiting on. Same shape as the discovery engine's dashboard: std
//! TcpListener, a thread per request, pages compiled in.
//!
//! LOOPBACK, AND THE GATEWAY IN FRONT (PLAN-3403.01). The listener binds
//! `APK_AES67_HTTP_BIND` (default 127.0.0.1): the container is host-networked,
//! so 0.0.0.0 was every interface on the bench, outside the gateway's sign-in.
//! The browser reaches it at `/plugins/aes67/ui/` on the one published port;
//! Portal-Gateway dials NODE.BareMetal's relay socket (`aes67.sock`), which
//! dials this port on the host's loopback. Caddy strips the prefix, so every
//! URL the pages use is RELATIVE (`api/status`, `style.css`, `config`) and the
//! same page works at `http://127.0.0.1:8130/config` and under the prefix.
//! The `/` redirect's Location is relative for the same reason.
//!
//! WRITES CAN BE GATED: with `APK_AES67_TOKEN` set, every PUT/POST/DELETE needs
//! `Authorization: Bearer <token>` (or `X-APK-Token`). Unset, the API is open
//! to whoever can reach the port — the same trust the LAN's AES67 gear extends.
//!
//! API — VERSIONED (2026-10-30 audit). Every route below lives under
//! `/api/v1/`, which the pages use; the unversioned `/api/…` spelling is the
//! same handler, kept as an alias for one release for scripts written against
//! it, and retires at crate 0.3.0. A change of a route's meaning is `/api/v2/`
//! beside v1, never an edit of v1. `GET /api/v1/settings` carries the settings
//! file's `schema` (see config.rs).
//!   GET    /api/status                everything live
//!   GET    /api/settings              the saved settings
//!   PUT    /api/settings              replace them (validated, then applied)
//!   GET    /api/devices               ALSA capture / playback devices
//!   GET    /api/interfaces            IPv4 interfaces
//!   GET    /api/browse                SAP sessions heard on the network
//!   GET    /api/sources/{id}/sdp      a source's SDP (application/sdp): the running
//!                                     one, else what its saved settings would send
//!   POST   /api/sources/sdp           {source JSON} -> {sdp, live, sap, ravenna,
//!                                     rtsp_url}: the SDP of the form's row as it
//!                                     stands, saved or not (read-only; no token)
//!   PUT    /api/sources/{id}          replace one source   (same for /api/sinks/{id})
//!   POST   /api/sources               add one              (id assigned if 0)
//!   DELETE /api/sources/{id}          remove one
//!   GET    /api/selftest              the crate's own self test, depth `self`

use crate::config::{MAX_SINKS, MAX_SOURCES, Settings, Sink, Source};
use crate::daemon;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

const STATUS_HTML: &str = include_str!("web/status.html");
const CONFIG_HTML: &str = include_str!("web/config.html");
const MATRIX_HTML: &str = include_str!("web/matrix.html");
const STYLE_CSS: &str = include_str!("web/style.css");
/// THE ONE TABLE (ROAD-3494): every table on the three pages is an APKTable,
/// the copy of WEB.Frontend-Assets' /FrontEnd/shared/apk-table.* vendored here
/// (held byte-identical by `check.sh webshared`) and served beside the pages.
const APK_TABLE_JS: &str = include_str!("web/vendor/apk-table/apk-table.js");
const APK_TABLE_CSS: &str = include_str!("web/vendor/apk-table/apk-table.css");
const MAX_BODY: usize = 1 << 20;

pub fn http_port() -> u16 {
    std::env::var("APK_AES67_HTTP_PORT")
        .ok()
        .and_then(|p| p.trim().parse().ok())
        .unwrap_or_else(|| daemon::global().settings().http_port)
}

/// The address the listener binds. Loopback unless the operator widens it on
/// purpose; a value that does not parse is loopback too, said in the log.
pub fn http_bind() -> std::net::IpAddr {
    let raw = std::env::var("APK_AES67_HTTP_BIND").unwrap_or_default();
    let raw = raw.trim();
    if raw.is_empty() {
        return std::net::IpAddr::from([127, 0, 0, 1]);
    }
    raw.parse().unwrap_or_else(|_| {
        apk_plugin_runner::elog!(warn, "⚠️  [aes67] web: APK_AES67_HTTP_BIND={raw:?} is not an address; binding 127.0.0.1");
        std::net::IpAddr::from([127, 0, 0, 1])
    })
}

/// The web agent. Never returns: a port that will not bind is retried, so a
/// conflict shows up as a log line and a missing page, not a restart loop.
pub fn run_web_agent(_host: &str, _port: u16) {
    let port = http_port();
    let bind = http_bind();
    let listener = loop {
        match TcpListener::bind(SocketAddr::new(bind, port)) {
            Ok(l) => break l,
            Err(e) => {
                apk_plugin_runner::elog!(warn, "⚠️  [aes67] web: cannot bind {bind}:{port} ({e}); retrying in 10 s");
                std::thread::sleep(Duration::from_secs(10));
            }
        }
    };
    println!("🌐 [aes67] status http://{bind}:{port}/status · config http://{bind}:{port}/config · via the gateway /plugins/aes67/ui/");
    accept_with_idle(listener, || {
        apk_plugin_runner::tick();
    });
}

/// How long the accept loop waits between polls of a quiet listener.
const IDLE_EVERY: Duration = Duration::from_millis(50);

/// Accept for ever, a thread per request, calling `idle` on every turn of the
/// loop — at least every [`IDLE_EVERY`] while nobody asks (PLAN-3428.01, RP-09).
/// The agent passes `apk_plugin_runner::tick`, so the runner's per-agent stall
/// clock sees a listener that is still LISTENING, not only one being asked; a
/// blocking `incoming()` never ticked, so a wedged web agent was invisible.
/// The same shape as PLUGIN.SNMP's `http::serve_with_idle`.
fn accept_with_idle(listener: TcpListener, idle: impl Fn()) {
    if let Err(e) = listener.set_nonblocking(true) {
        apk_plugin_runner::elog!(warn, "⚠️  [aes67] web: cannot poll the listener ({e}); the web agent will not tick");
    }
    loop {
        idle();
        match listener.accept() {
            Ok((stream, _)) => {
                // The accepted socket must block: handle() reads the request whole.
                if stream.set_nonblocking(false).is_err() {
                    continue;
                }
                let _ = std::thread::Builder::new().name("aes67:http".into()).spawn(move || handle(stream));
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(IDLE_EVERY),
            Err(e) => {
                apk_plugin_runner::elog!(warn, "⚠️  [aes67] web: accept failed: {e}");
                std::thread::sleep(IDLE_EVERY);
            }
        }
    }
}

struct Request {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        if buf.len() > 64 * 1024 {
            return None;
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split_whitespace();
    let method = first.next()?.to_string();
    let path = first.next()?.split('?').next()?.to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':').map(|(k, v)| (k.trim().to_string(), v.trim().to_string())))
        .collect();
    let len: usize = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    if len > MAX_BODY {
        return None;
    }
    let mut body = buf[header_end + 4..].to_vec();
    while body.len() < len {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(len);
    Some(Request { method, path, headers, body })
}

fn respond(stream: &mut TcpStream, code: u16, ctype: &str, body: &[u8]) {
    let reason = match code {
        200 => "OK",
        201 => "Created",
        302 => "Found",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\n\
         Access-Control-Allow-Origin: *\r\nConnection: close\r\n{}\r\n",
        body.len(),
        if code == 302 { "Location: status\r\n" } else { "" }
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
}

fn handle(mut stream: TcpStream) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
    let Some(req) = read_request(&mut stream) else { return };
    let (code, ctype, body) = route(&req);
    respond(&mut stream, code, ctype, &body);
}

fn ok(v: Value) -> (u16, &'static str, Vec<u8>) {
    (200, "application/json", v.to_string().into_bytes())
}

fn err(code: u16, msg: impl Into<String>) -> (u16, &'static str, Vec<u8>) {
    (code, "application/json", json!({ "error": msg.into() }).to_string().into_bytes())
}

fn authorised(req: &Request) -> bool {
    let Ok(token) = std::env::var("APK_AES67_TOKEN") else { return true };
    if token.is_empty() {
        return true;
    }
    let bearer = req.header("authorization").and_then(|v| v.strip_prefix("Bearer ")).map(str::trim);
    bearer == Some(token.as_str()) || req.header("x-apk-token") == Some(token.as_str())
}

/// The API version the pages use and `GET /api/v1/…` answers.
pub const API_VERSION: &str = "v1";

/// The path's segments with the API version folded away: `/api/v1/status`
/// and the legacy `/api/status` are both `["api", "status"]`. Any other
/// version is refused (`None`), so a client written for a later API is told
/// so instead of being answered by v1.
fn api_parts(path: &str) -> Option<Vec<&str>> {
    let mut parts: Vec<&str> = path.trim_matches('/').split('/').filter(|s| !s.is_empty()).collect();
    if parts.first() == Some(&"api") {
        if let Some(v) = parts.get(1).filter(|v| v.len() > 1 && v.starts_with('v') && v[1..].bytes().all(|b| b.is_ascii_digit())) {
            if *v != API_VERSION {
                return None;
            }
            parts.remove(1);
        }
    }
    Some(parts)
}

fn route(req: &Request) -> (u16, &'static str, Vec<u8>) {
    let d = daemon::global();
    let Some(parts) = api_parts(&req.path) else {
        return err(404, format!("this build serves API {API_VERSION}, not {}", req.path));
    };
    let method = req.method.as_str();
    if method == "OPTIONS" {
        return (200, "text/plain", Vec::new());
    }
    // A preview reads; it changes nothing, so it needs no token.
    if method == "POST" && parts.as_slice() == ["api", "sources", "sdp"] {
        return preview(&d, &req.body);
    }
    if matches!(method, "PUT" | "POST" | "DELETE") && !authorised(req) {
        return err(401, "this API needs APK_AES67_TOKEN — send it as `Authorization: Bearer …`");
    }
    match (method, parts.as_slice()) {
        ("GET", []) => (302, "text/plain", Vec::new()),
        ("GET", ["status"]) => (200, "text/html; charset=utf-8", STATUS_HTML.as_bytes().to_vec()),
        // One page, three tabs; the path picks the tab.
        ("GET", ["config" | "senders" | "receivers"]) => (200, "text/html; charset=utf-8", CONFIG_HTML.as_bytes().to_vec()),
        ("GET", ["matrix"]) => (200, "text/html; charset=utf-8", MATRIX_HTML.as_bytes().to_vec()),
        ("GET", ["style.css"]) => (200, "text/css; charset=utf-8", STYLE_CSS.as_bytes().to_vec()),
        ("GET", ["vendor", "apk-table", "apk-table.js"]) => (200, "text/javascript; charset=utf-8", APK_TABLE_JS.as_bytes().to_vec()),
        ("GET", ["vendor", "apk-table", "apk-table.css"]) => (200, "text/css; charset=utf-8", APK_TABLE_CSS.as_bytes().to_vec()),
        ("GET", ["api", "status"]) => ok(d.status()),
        ("GET", ["api", "settings"]) => ok(json!({
            "settings": d.settings(),
            "limits": { "sources": MAX_SOURCES, "sinks": MAX_SINKS, "channels": crate::config::MAX_STREAM_CHANNELS },
        })),
        ("PUT", ["api", "settings"]) => match serde_json::from_slice::<Settings>(&req.body) {
            Ok(s) => match d.apply(s) {
                Ok(()) => ok(json!({ "settings": d.settings() })),
                Err(e) => err(400, e),
            },
            Err(e) => err(400, format!("settings JSON: {e}")),
        },
        ("GET", ["api", "devices"]) => ok(d.devices()),
        ("GET", ["api", "interfaces"]) => ok(json!(crate::net::interfaces())),
        ("GET", ["api", "browse"]) => ok(d.browse()),
        ("GET", ["api", "selftest"]) => ok(
            serde_json::from_str(&crate::self_test::self_test_json("self")).unwrap_or(Value::Null),
        ),
        ("GET", ["api", "sources", id, "sdp"]) => match id.parse().ok().and_then(|id| d.source_sdp(id)) {
            Some(sdp) => (200, "application/sdp", sdp.into_bytes()),
            None => err(404, "no running source with that id"),
        },
        (_, ["api", kind @ ("sources" | "sinks"), rest @ ..]) => item(&d, method, kind, rest, &req.body),
        _ => err(404, format!("no route for {method} {}", req.path)),
    }
}

/// The SDP button on /config: the row as the form holds it right now.
fn preview(d: &daemon::Daemon, body: &[u8]) -> (u16, &'static str, Vec<u8>) {
    // Either a bare source, or {"source": …, "settings": …} so the preview
    // also reflects unsaved node settings (sample rate, node name).
    let v: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => return err(400, format!("JSON: {e}")),
    };
    let (src_v, pending) = match v.get("source") {
        Some(src) => (src.clone(), v.get("settings").and_then(|x| serde_json::from_value::<Settings>(x.clone()).ok())),
        None => (v, None),
    };
    let src: Source = match serde_json::from_value(src_v) {
        Ok(x) => x,
        Err(e) => return err(400, format!("source JSON: {e}")),
    };
    let s = pending.clone().unwrap_or_else(|| d.settings());
    match d.preview_sdp_with(&src, pending.as_ref()) {
        Ok((sdp, live)) => {
            let name = crate::ravenna::Session::from_sdp(src.id, &sdp).map(|x| x.name).unwrap_or_default();
            ok(json!({
                "sdp": sdp,
                "live": live,
                "sap": { "wanted": src.sap, "node_announces": s.sap.announce, "group": s.sap.group, "port": s.sap.port },
                "ravenna": { "wanted": src.ravenna, "advertised": crate::ravenna::is_advertised(&name) },
                "rtsp_url": d.rtsp_url(&src),
            }))
        }
        Err(e) => err(400, e),
    }
}

/// One source or sink, edited through a copy of the whole settings so every
/// write goes through the same validate-save-reconcile as a full PUT.
fn item(d: &daemon::Daemon, method: &str, kind: &str, rest: &[&str], body: &[u8]) -> (u16, &'static str, Vec<u8>) {
    let mut s = d.settings();
    let id: Option<u32> = rest.first().and_then(|x| x.parse().ok());
    let sources = kind == "sources";
    match (method, id) {
        ("GET", None) => {
            return ok(if sources { json!(s.sources) } else { json!(s.sinks) });
        }
        ("GET", Some(id)) => {
            let v = if sources {
                s.sources.iter().find(|x| x.id == id).map(|x| json!(x))
            } else {
                s.sinks.iter().find(|x| x.id == id).map(|x| json!(x))
            };
            return v.map(ok).unwrap_or_else(|| err(404, "no such id"));
        }
        ("DELETE", Some(id)) => {
            let before = s.sources.len() + s.sinks.len();
            if sources {
                s.sources.retain(|x| x.id != id);
            } else {
                s.sinks.retain(|x| x.id != id);
            }
            if s.sources.len() + s.sinks.len() == before {
                return err(404, "no such id");
            }
        }
        ("POST", None) | ("PUT", Some(_)) => {
            let next = s.next_id();
            if sources {
                let mut x: Source = match serde_json::from_slice(body) {
                    Ok(x) => x,
                    Err(e) => return err(400, format!("source JSON: {e}")),
                };
                x.id = id.unwrap_or(if x.id == 0 { next } else { x.id });
                match s.sources.iter_mut().find(|y| y.id == x.id) {
                    Some(slot) => *slot = x,
                    None if method == "PUT" => return err(404, "no such id"),
                    None => s.sources.push(x),
                }
            } else {
                let mut x: Sink = match serde_json::from_slice(body) {
                    Ok(x) => x,
                    Err(e) => return err(400, format!("sink JSON: {e}")),
                };
                x.id = id.unwrap_or(if x.id == 0 { next } else { x.id });
                match s.sinks.iter_mut().find(|y| y.id == x.id) {
                    Some(slot) => *slot = x,
                    None if method == "PUT" => return err(404, "no such id"),
                    None => s.sinks.push(x),
                }
            }
        }
        _ => return err(405, format!("{method} is not allowed here")),
    }
    match d.apply(s) {
        Ok(()) => ok(json!({ "settings": d.settings() })),
        Err(e) => err(400, e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quiet_listener_still_ticks_and_survives_a_caller() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let ticks = Arc::new(AtomicUsize::new(0));
        let seen = ticks.clone();
        std::thread::spawn(move || accept_with_idle(listener, move || {
            seen.fetch_add(1, Ordering::Relaxed);
        }));
        std::thread::sleep(Duration::from_millis(300));
        assert!(ticks.load(Ordering::Relaxed) >= 2, "no request arrived, yet the loop must keep ticking");
        // A caller that connects and hangs up is accepted and dropped, and the
        // loop keeps ticking after it.
        drop(TcpStream::connect(addr).unwrap());
        let before = ticks.load(Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(300));
        assert!(ticks.load(Ordering::Relaxed) > before, "the loop kept turning after a connection");
    }

    #[test]
    fn v1_and_the_legacy_alias_are_one_route_and_other_versions_are_refused() {
        assert_eq!(api_parts("/api/v1/sources/3/sdp"), Some(vec!["api", "sources", "3", "sdp"]));
        assert_eq!(api_parts("/api/sources/3/sdp"), api_parts("/api/v1/sources/3/sdp"));
        assert_eq!(api_parts("/api/v2/status"), None);
        assert_eq!(api_parts("/config"), Some(vec!["config"]));
        // A segment that merely starts with v is not a version.
        assert_eq!(api_parts("/api/values"), Some(vec!["api", "values"]));
        for page in [STATUS_HTML, CONFIG_HTML, MATRIX_HTML] {
            assert_eq!(page.matches("api/").count(), page.matches("api/v1/").count(), "pages call api/v1/");
        }
    }

    /// THE ONE TABLE (ROAD-3494): every table on the pages is an APKTable host
    /// under its own id, no page writes table markup itself, and the vendored
    /// apk-table the hosts need is what the server answers at vendor/apk-table/.
    #[test]
    fn every_table_is_an_apktable_and_the_table_script_is_served() {
        let ids = |page: &str| -> Vec<String> {
            page.split("data-apk-table-host=\"").skip(1).map(|s| s[..s.find('"').unwrap()].to_string()).collect()
        };
        assert_eq!(ids(STATUS_HTML), ["aes67-status-senders", "aes67-status-receivers", "aes67-status-sap"]);
        assert_eq!(ids(CONFIG_HTML), ["aes67-config-senders", "aes67-config-receivers"]);
        assert_eq!(ids(MATRIX_HTML), ["aes67-matrix"]);
        for page in [STATUS_HTML, CONFIG_HTML, MATRIX_HTML] {
            assert!(!page.contains("<table"), "a page writes no <table> itself");
            let tag = page.find("vendor/apk-table/apk-table.js").expect("loads the table script");
            assert!(tag < page.find("<script>\n").unwrap(), "before the page's own script");
        }
        assert!(APK_TABLE_JS.contains("root.APKTable = api"));
        assert!(APK_TABLE_JS.contains("function scan("));
        let req = |path: &str| Request { method: "GET".into(), path: path.into(), headers: Vec::new(), body: Vec::new() };
        let (code, kind, body) = route(&req("/vendor/apk-table/apk-table.js"));
        assert_eq!((code, kind), (200, "text/javascript; charset=utf-8"));
        assert_eq!(body, APK_TABLE_JS.as_bytes());
    }
}
