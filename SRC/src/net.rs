// Part of the APK.audio project — http://APK.audio — made by Anthony Kuzub
// MIT Licence. Free, for everyone, for ever. Full text in LICENSE at the root.
//! Interfaces and the three kinds of UDP socket this daemon opens.
//!
//! EVERY LISTENING SOCKET SETS SO_REUSEPORT. The node is host-networked and
//! apk-ptp already listens on 319/320, apk-sap on 9875; without the flag the
//! second bind fails and one of the two plugins goes blind. With it, both are
//! handed a copy of every MULTICAST datagram, which is all either listens to.

use serde::Serialize;
use socket2::{Domain, Protocol, Socket, Type};
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::time::Duration;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Interface {
    pub name: String,
    pub ipv4: Ipv4Addr,
    pub mac: Option<String>,
    pub up: bool,
    pub multicast: bool,
    pub loopback: bool,
}

/// Every interface with an IPv4 address, from getifaddrs(3).
pub fn interfaces() -> Vec<Interface> {
    let mut out = Vec::new();
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs allocates a list we walk read-only and free once.
    unsafe {
        if libc::getifaddrs(&mut head) != 0 {
            return out;
        }
        let mut cursor = head;
        while !cursor.is_null() {
            let ifa = &*cursor;
            cursor = ifa.ifa_next;
            if ifa.ifa_addr.is_null() || (*ifa.ifa_addr).sa_family as i32 != libc::AF_INET {
                continue;
            }
            let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
            let name = std::ffi::CStr::from_ptr(ifa.ifa_name).to_string_lossy().to_string();
            let flags = ifa.ifa_flags as i32;
            out.push(Interface {
                mac: mac_of(&name),
                name,
                ipv4: Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr)),
                up: flags & libc::IFF_UP != 0 && flags & libc::IFF_RUNNING != 0,
                multicast: flags & libc::IFF_MULTICAST != 0,
                loopback: flags & libc::IFF_LOOPBACK != 0,
            });
        }
        libc::freeifaddrs(head);
    }
    out
}

fn mac_of(name: &str) -> Option<String> {
    std::fs::read_to_string(format!("/sys/class/net/{name}/address"))
        .ok()
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty() && m != "00:00:00:00:00:00")
}

/// Container bridges and tunnels carry no AES67; picking one silently would
/// send every stream into docker0.
fn is_virtual(name: &str) -> bool {
    ["docker", "br-", "veth", "virbr", "tun", "tap", "wg", "zt", "tailscale", "lo"]
        .iter()
        .any(|p| name.starts_with(p))
}

/// `wanted` by name, else the first physical-looking interface that is up.
pub fn resolve(wanted: &str) -> Option<Interface> {
    let all = interfaces();
    let wanted = wanted.trim();
    if !wanted.is_empty() {
        return all.into_iter().find(|i| i.name == wanted || i.ipv4.to_string() == wanted);
    }
    all.into_iter()
        .filter(|i| i.up && i.multicast && !i.loopback && !is_virtual(&i.name))
        .min_by_key(|i| i.name.clone())
}

/// A socket bound to `port` on every address, joined to `group` on `iface`
/// (or, with `source`, to only that sender — SSM, IGMPv3).
pub fn multicast_listener(
    group: Ipv4Addr,
    port: u16,
    iface: Ipv4Addr,
    source: Option<Ipv4Addr>,
) -> std::io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    socket.set_reuse_port(true)?;
    // Room for ~100 ms of a 64-channel stream while a thread is descheduled.
    let _ = socket.set_recv_buffer_size(4 << 20);
    socket.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port).into())?;
    if group.is_multicast() {
        match source {
            Some(src) => socket.join_ssm_v4(&src, &group, &iface)?,
            None => socket.join_multicast_v4(&group, &iface)?,
        }
    }
    let socket: UdpSocket = socket.into();
    socket.set_read_timeout(Some(Duration::from_millis(200)))?;
    Ok(socket)
}

/// A sending socket: multicast out of `iface`, with `ttl` and a DSCP.
/// Loopback stays ON so a receiver on this same host can monitor a source.
pub fn sender(iface: Ipv4Addr, ttl: u8, dscp: u8) -> std::io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.bind(&SocketAddrV4::new(iface, 0).into())?;
    socket.set_multicast_if_v4(&iface)?;
    socket.set_multicast_ttl_v4(ttl.max(1) as u32)?;
    socket.set_multicast_loop_v4(true)?;
    let _ = socket.set_tos_v4((dscp as u32) << 2);
    let _ = socket.set_send_buffer_size(1 << 20);
    Ok(socket.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_bridges_are_never_the_default() {
        assert!(is_virtual("docker0"));
        assert!(is_virtual("br-3f2a"));
        assert!(!is_virtual("enp3s0"));
        assert!(!is_virtual("eth0"));
    }
}
