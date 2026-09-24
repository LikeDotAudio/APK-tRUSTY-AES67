// Part of the APK.audio project — http://APK.audio — made by Anthony Kuzub
// MIT Licence. Free, for everyone, for ever. Full text in LICENSE at the root.
//! The web door: `/status`, `/config`, and the REST API both pages use.
//!
//! A PORT OF ITS OWN, AND WHY THIS PLUGIN IS THE EXCEPTION. Plugins describe
//! themselves read-only over the bus (`incoming/api`) behind the node's one
//! gateway, and do not open ports. This one is configured by a person — thirty-
//! two stream slots, device pickers, SDP pasted from another vendor's page —
//! which needs a writable store (its volume) and a form, the two things that
//! rule was waiting on. Same shape as the discovery engine's dashboard: std
//! TcpListener, a thread per request, pages compiled in.
//!
//! WRITES CAN BE GATED: with `APK_AES67_TOKEN` set, every PUT/POST/DELETE needs
//! `Authorization: Bearer <token>` (or `X-APK-Token`). Unset, the API is open
//! to whoever can reach the port — the same trust the LAN's AES67 gear extends.
//!
//! API
//!   GET    /api/status                everything live
//!   GET    /api/settings              the saved settings
//!   PUT    /api/settings              replace them (validated, then applied)
//!   GET    /api/devices               ALSA capture / playback devices
//!   GET    /api/interfaces            IPv4 interfaces
//!   GET    /api/browse                SAP sessions heard on the network
//!   GET    /api/sources/{id}/sdp      a running source's SDP (application/sdp)
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
const STYLE_CSS: &str = include_str!("web/style.css");
const MAX_BODY: usize = 1 << 20;

pub fn http_port() -> u16 {
    std::env::var("APK_AES67_HTTP_PORT")
        .ok()
        .and_then(|p| p.trim().parse().ok())
        .unwrap_or_else(|| daemon::global().settings().http_port)
}

/// The web agent. Never returns: a port that will not bind is retried, so a
/// conflict shows up as a log line and a missing page, not a restart loop.
pub fn run_web_agent(_host: &str, _port: u16) {
    let port = http_port();
    let listener = loop {
        match TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], port))) {
            Ok(l) => break l,
            Err(e) => {
                eprintln!("⚠️  [aes67] web: cannot bind :{port} ({e}); retrying in 10 s");
                std::thread::sleep(Duration::from_secs(10));
            }
        }
    };
    println!("🌐 [aes67] status http://0.0.0.0:{port}/status · config http://0.0.0.0:{port}/config");
    for stream in listener.incoming().flatten() {
        let _ = std::thread::Builder::new().name("aes67:http".into()).spawn(move || handle(stream));
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
        if code == 302 { "Location: /status\r\n" } else { "" }
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

fn route(req: &Request) -> (u16, &'static str, Vec<u8>) {
    let d = daemon::global();
    let parts: Vec<&str> = req.path.trim_matches('/').split('/').filter(|s| !s.is_empty()).collect();
    let method = req.method.as_str();
    if method == "OPTIONS" {
        return (200, "text/plain", Vec::new());
    }
    if matches!(method, "PUT" | "POST" | "DELETE") && !authorised(req) {
        return err(401, "this API needs APK_AES67_TOKEN — send it as `Authorization: Bearer …`");
    }
    match (method, parts.as_slice()) {
        ("GET", []) => (302, "text/plain", Vec::new()),
        ("GET", ["status"]) => (200, "text/html; charset=utf-8", STATUS_HTML.as_bytes().to_vec()),
        ("GET", ["config"]) => (200, "text/html; charset=utf-8", CONFIG_HTML.as_bytes().to_vec()),
        ("GET", ["style.css"]) => (200, "text/css; charset=utf-8", STYLE_CSS.as_bytes().to_vec()),
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
