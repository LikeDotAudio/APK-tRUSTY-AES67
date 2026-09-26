// Part of the APK.audio project — http://APK.audio — made by Anthony Kuzub
// MIT Licence. Free, for everyone, for ever. Full text in LICENSE at the root.
//! RAVENNA announcement: the other way an AES67 sender is found.
//!
//! SAP pushes the SDP at a multicast group; RAVENNA does not push it at all.
//! A RAVENNA sender advertises each session over mDNS as
//! `<session>._rtsp._tcp.local.` with the subtype `_ravenna_session`, and a
//! receiver that wants it asks the advertised host and port with
//! `DESCRIBE rtsp://<host>:<port>/by-name/<session>`. So this module is two
//! halves that must agree on one list: an mDNS registration per session, and
//! an RTSP server that answers DESCRIBE for exactly those sessions.
//!
//! WHAT IS CHOSEN, PER SENDER. The `/config` page ticks `ravenna` on a sender;
//! the daemon hands the ticked, RUNNING senders to [`sync`] once a second, and
//! whatever is no longer in that list is unregistered (an mDNS goodbye) and
//! stops being served. A sender that is not running is never advertised: an
//! announcement is a promise that audio is on the wire.
//!
//! THE PORT IS THE SRV RECORD'S, NOT 554. Receivers — this repo's own
//! `plugin:RAVENNA` included — DESCRIBE the port mDNS gave them, so the server
//! listens on `rtsp_port` (default 8554) and needs no privileged bind.
//!
//! ONLY DESCRIBE. There is no SETUP/PLAY here: an AES67 stream is multicast
//! and already flowing, and the SDP is the whole of what a receiver needs to
//! join it — which is how RAVENNA receivers use AES67 senders in practice.

use mdns_sd::{ServiceDaemon, ServiceInfo};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// The mDNS type every session is registered under, subtype included.
pub const SESSION_TYPE: &str = "_ravenna_session._sub._rtsp._tcp.local.";

/// One advertised session: a running sender and the SDP it runs with.
#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    pub id: u32,
    /// The SDP's `s=` — the mDNS instance name and the `/by-name/` key.
    pub name: String,
    pub sdp: String,
}

impl Session {
    pub fn from_sdp(id: u32, sdp: &str) -> Option<Session> {
        let name = sdp.lines().find_map(|l| l.trim_end().strip_prefix("s="))?.trim().to_string();
        (!name.is_empty()).then(|| Session { id, name, sdp: sdp.to_string() })
    }
}

/// What is registered with mDNS now: session name -> (fullname, ip, port).
type Registered = BTreeMap<String, (String, Ipv4Addr, u16)>;

struct State {
    sessions: Vec<Session>,
    mdns: Option<ServiceDaemon>,
    registered: Registered,
    server: Option<(u16, Arc<AtomicBool>)>,
    last_error: Option<String>,
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(|| {
        Mutex::new(State { sessions: Vec::new(), mdns: None, registered: Registered::new(), server: None, last_error: None })
    })
}

fn lock() -> std::sync::MutexGuard<'static, State> {
    state().lock().unwrap_or_else(|p| p.into_inner())
}

/// Make the advertised set equal `sessions`. Cheap when nothing changed, so
/// the daemon calls it every second. `origin` is the interface address the
/// streams leave from; without one nothing can be advertised.
pub fn sync(origin: Option<Ipv4Addr>, rtsp_port: u16, node: &str, sessions: Vec<Session>) {
    let mut st = lock();
    let wanted: Vec<Session> = if origin.is_some() { sessions } else { Vec::new() };
    if wanted.is_empty() && st.registered.is_empty() {
        st.sessions.clear();
        return;
    }
    st.sessions = wanted.clone();

    // The server first: an advertisement pointing at a closed port is worse
    // than none. A moved port restarts it.
    if !wanted.is_empty() && st.server.as_ref().map(|(p, _)| *p) != Some(rtsp_port) {
        if let Some((_, stop)) = st.server.take() {
            stop.store(true, Ordering::Relaxed);
        }
        match serve(rtsp_port) {
            Ok(stop) => {
                println!("📡 [aes67] RAVENNA RTSP on :{rtsp_port}");
                st.server = Some((rtsp_port, stop));
                st.last_error = None;
            }
            Err(e) => {
                let msg = format!("RTSP :{rtsp_port}: {e}");
                if st.last_error.as_deref() != Some(msg.as_str()) {
                    eprintln!("⚠️  [aes67] RAVENNA {msg}");
                }
                st.last_error = Some(msg);
                return;
            }
        }
    }
    if st.mdns.is_none() {
        match ServiceDaemon::new() {
            Ok(d) => st.mdns = Some(d),
            Err(e) => {
                st.last_error = Some(format!("mDNS: {e}"));
                return;
            }
        }
    }

    let ip = origin.unwrap_or(Ipv4Addr::UNSPECIFIED);
    let host = format!("{}.local.", host_label(node));
    let want: BTreeMap<&str, &Session> = wanted.iter().map(|s| (s.name.as_str(), s)).collect();
    let State { mdns, registered, last_error, .. } = &mut *st;
    let Some(mdns) = mdns.as_ref() else { return };

    let stale: Vec<String> = registered
        .iter()
        .filter(|(name, (_, rip, rport))| !want.contains_key(name.as_str()) || *rip != ip || *rport != rtsp_port)
        .map(|(name, _)| name.clone())
        .collect();
    for name in stale {
        if let Some((fullname, _, _)) = registered.remove(&name) {
            let _ = mdns.unregister(&fullname);
            println!("👋 [aes67] RAVENNA withdrew {name}");
        }
    }
    for (name, _) in want {
        if registered.contains_key(name) {
            continue;
        }
        let instance = instance_name(name);
        match ServiceInfo::new(SESSION_TYPE, &instance, &host, std::net::IpAddr::V4(ip), rtsp_port, None::<std::collections::HashMap<String, String>>) {
            Ok(info) => {
                let fullname = info.get_fullname().to_string();
                match mdns.register(info) {
                    Ok(()) => {
                        println!("📣 [aes67] RAVENNA advertised {name} → rtsp://{ip}:{rtsp_port}/by-name/{}", url_encode(name));
                        registered.insert(name.to_string(), (fullname, ip, rtsp_port));
                    }
                    Err(e) => *last_error = Some(format!("mDNS register {name}: {e}")),
                }
            }
            Err(e) => *last_error = Some(format!("mDNS record {name}: {e}")),
        }
    }
}

/// For `/api/status`: what is advertised and the last thing that failed.
pub fn status() -> serde_json::Value {
    let st = lock();
    serde_json::json!({
        "rtsp_port": st.server.as_ref().map(|(p, _)| *p),
        "sessions": st.registered.keys().collect::<Vec<_>>(),
        "error": st.last_error,
    })
}

/// Whether a session with this `s=` name is advertised right now.
pub fn is_advertised(name: &str) -> bool {
    lock().registered.contains_key(name)
}

/// mDNS labels are ≤ 63 bytes; cut on a char boundary.
fn instance_name(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if out.len() + c.len_utf8() > 63 {
            break;
        }
        out.push(c);
    }
    out
}

/// A host label from the node name: letters, digits and hyphens only.
fn host_label(node: &str) -> String {
    let s: String = node
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string();
    if s.is_empty() { "apk-aes67".to_string() } else { instance_name(&s) }
}

/// Percent-encoding for the `/by-name/` path segment (RFC 3986 unreserved kept).
pub fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Some(v) = s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Bind and run the RTSP server on its own thread; the flag stops it.
fn serve(port: u16) -> std::io::Result<Arc<AtomicBool>> {
    let listener = TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], port)))?;
    listener.set_nonblocking(true)?;
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    std::thread::Builder::new().name("aes67:rtsp".into()).spawn(move || {
        while !flag.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = std::thread::Builder::new().name("aes67:rtsp-conn".into()).spawn(move || connection(stream));
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(100)),
                Err(_) => std::thread::sleep(Duration::from_millis(100)),
            }
        }
    })?;
    Ok(stop)
}

/// One RTSP connection: requests until the peer closes or goes quiet.
fn connection(stream: TcpStream) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    let Ok(mut out) = stream.try_clone() else { return };
    let mut reader = BufReader::new(stream);
    loop {
        let mut head = Vec::new();
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            let line = line.trim_end().to_string();
            if line.is_empty() {
                break;
            }
            head.push(line);
            if head.len() > 64 {
                return;
            }
        }
        let reply = answer(&head, &lock().sessions);
        if out.write_all(reply.as_bytes()).is_err() {
            return;
        }
    }
}

/// The whole of the protocol, pure so it is tested without a socket.
fn answer(head: &[String], sessions: &[Session]) -> String {
    let cseq = head
        .iter()
        .find_map(|l| l.split_once(':').filter(|(k, _)| k.trim().eq_ignore_ascii_case("cseq")).map(|(_, v)| v.trim().to_string()))
        .unwrap_or_else(|| "0".into());
    let mut first = head.first().map(|l| l.split_whitespace()).into_iter().flatten();
    let method = first.next().unwrap_or("");
    let url = first.next().unwrap_or("");
    let plain = |code: &str| format!("RTSP/1.0 {code}\r\nCSeq: {cseq}\r\n\r\n");
    match method {
        "OPTIONS" => format!("RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nPublic: OPTIONS, DESCRIBE\r\n\r\n"),
        "DESCRIBE" => {
            let path = url.split_once("://").map(|(_, rest)| rest.split_once('/').map_or("", |(_, p)| p)).unwrap_or(url);
            let path = path.trim_start_matches('/');
            let found = if let Some(name) = path.strip_prefix("by-name/") {
                let name = url_decode(name.trim_end_matches('/'));
                sessions.iter().find(|s| s.name == name)
            } else if let Some(id) = path.strip_prefix("by-id/") {
                id.trim_end_matches('/').parse::<u32>().ok().and_then(|id| sessions.iter().find(|s| s.id == id))
            } else {
                None
            };
            match found {
                Some(s) => format!(
                    "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nContent-Type: application/sdp\r\nContent-Base: {url}/\r\n\
                     Content-Length: {}\r\n\r\n{}",
                    s.sdp.len(),
                    s.sdp
                ),
                None => plain("404 Not Found"),
            }
        }
        "" => plain("400 Bad Request"),
        _ => plain("405 Method Not Allowed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SDP: &str = "v=0\r\no=- 1 2 IN IP4 10.0.0.5\r\ns=bench TX 01\r\nc=IN IP4 239.69.0.1/32\r\n";

    fn head(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_session_is_named_by_its_sdp() {
        let s = Session::from_sdp(1, SDP).unwrap();
        assert_eq!(s.name, "bench TX 01");
        assert!(Session::from_sdp(1, "v=0\r\n").is_none());
    }

    #[test]
    fn describe_by_name_returns_the_sdp_and_a_stranger_is_404() {
        let sessions = vec![Session::from_sdp(1, SDP).unwrap()];
        let r = answer(&head(&["DESCRIBE rtsp://10.0.0.5:8554/by-name/bench%20TX%2001 RTSP/1.0", "CSeq: 3"]), &sessions);
        assert!(r.starts_with("RTSP/1.0 200 OK\r\nCSeq: 3\r\n"), "{r}");
        assert!(r.contains("Content-Type: application/sdp") && r.ends_with(SDP));
        let r = answer(&head(&["DESCRIBE rtsp://10.0.0.5:8554/by-id/1 RTSP/1.0", "CSeq: 4"]), &sessions);
        assert!(r.ends_with(SDP));
        let r = answer(&head(&["DESCRIBE rtsp://10.0.0.5:8554/by-name/nope RTSP/1.0", "CSeq: 5"]), &sessions);
        assert!(r.starts_with("RTSP/1.0 404"));
        let r = answer(&head(&["OPTIONS * RTSP/1.0", "CSeq: 1"]), &sessions);
        assert!(r.contains("Public: OPTIONS, DESCRIBE"));
        assert!(answer(&head(&["PLAY x RTSP/1.0"]), &sessions).starts_with("RTSP/1.0 405"));
    }

    #[test]
    fn names_round_trip_the_url_and_fit_mdns() {
        for n in ["bench TX 01", "Ü 1/2 + x", "a%b"] {
            assert_eq!(url_decode(&url_encode(n)), n);
        }
        assert!(instance_name(&"é".repeat(40)).len() <= 63);
        assert_eq!(host_label("My Node!"), "My-Node");
        assert_eq!(host_label(""), "apk-aes67");
    }
}
