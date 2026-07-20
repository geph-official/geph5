//! Typed access to the Darwin routing table over a `PF_ROUTE` socket, in the
//! style of Mullvad's `talpid-routing`: operations are structs, results are
//! errnos (`EEXIST` = already there, `ESRCH` = no such route).
//!
//! Scope: exactly the operations the VPN backend needs — add/delete of
//! interface-bound and gateway routes (optionally `RTF_IFSCOPE`d) and a typed
//! `RTM_GET` lookup. Not a general routing library.

use std::{
    io::{self, Read, Write},
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    os::fd::AsRawFd,
    time::{Duration, Instant},
};

// ---- Darwin ABI constants (sys/socket.h, net/route.h, net/if_dl.h) ----
// Defined locally, like the utun constants in mod.rs, so we don't depend on
// libc exposing every Apple-specific item.
const PF_ROUTE: libc::c_int = 17;
const AF_LINK: u8 = 18;

const RTM_VERSION: u8 = 5;
const RTM_ADD: u8 = 0x1;
const RTM_DELETE: u8 = 0x2;
const RTM_GET: u8 = 0x4;

const RTA_DST: i32 = 0x1;
const RTA_GATEWAY: i32 = 0x2;
const RTA_NETMASK: i32 = 0x4;
const RTA_IFP: i32 = 0x10;

pub const RTF_UP: i32 = 0x1;
pub const RTF_GATEWAY: i32 = 0x2;
pub const RTF_HOST: i32 = 0x4;
pub const RTF_STATIC: i32 = 0x800;
pub const RTF_IFSCOPE: i32 = 0x1000000;

/// `sizeof(struct rt_msghdr)` on Darwin: 6 shorts/chars + padding (8 bytes),
/// then 7 ints (28) and rt_metrics (14 × u32 = 56).
const RT_MSGHDR_LEN: usize = 92;
/// Offsets within rt_msghdr (fixed Darwin ABI).
const OFF_MSGLEN: usize = 0;
const OFF_VERSION: usize = 2;
const OFF_TYPE: usize = 3;
const OFF_INDEX: usize = 4;
const OFF_FLAGS: usize = 8;
const OFF_ADDRS: usize = 12;
const OFF_PID: usize = 16;
const OFF_SEQ: usize = 20;
const OFF_ERRNO: usize = 24;

/// Round a sockaddr length up to the routing-socket alignment (4 bytes on
/// Darwin); a zero-length sockaddr still occupies one alignment unit.
fn roundup(len: usize) -> usize {
    if len == 0 { 4 } else { (len + 3) & !3 }
}

/// One route's worth of request parameters.
#[derive(Debug, Clone)]
pub struct RouteSpec {
    pub dest: IpAddr,
    /// None = host route (RTF_HOST, no netmask sockaddr).
    pub prefixlen: Option<u8>,
    pub gateway: Gateway,
    /// Scope the route to this interface index (RTF_IFSCOPE).
    pub ifscope: Option<u16>,
}

/// The route's next hop: a gateway IP, or a directly-attached interface
/// (encoded as an `AF_LINK` sockaddr, like `route add -interface`).
#[derive(Debug, Clone, Copy)]
pub enum Gateway {
    Ip(IpAddr),
    Interface(u16),
}

/// A parsed RTM_GET reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteInfo {
    pub gateway: Option<IpAddr>,
    /// Interface index the route egresses through (from the RTA_IFP sockaddr,
    /// falling back to the header's rtm_index).
    pub ifindex: u16,
    pub flags: i32,
}

fn put_u16(buf: &mut [u8], off: usize, v: u16) {
    buf[off..off + 2].copy_from_slice(&v.to_ne_bytes());
}
fn put_i32(buf: &mut [u8], off: usize, v: i32) {
    buf[off..off + 4].copy_from_slice(&v.to_ne_bytes());
}
fn get_u16(buf: &[u8], off: usize) -> u16 {
    u16::from_ne_bytes(buf[off..off + 2].try_into().unwrap())
}
fn get_i32(buf: &[u8], off: usize) -> i32 {
    i32::from_ne_bytes(buf[off..off + 4].try_into().unwrap())
}

/// Serialize a sockaddr_in/sockaddr_in6 for `addr`, aligned for the routing
/// socket.
fn sockaddr_ip(addr: IpAddr) -> Vec<u8> {
    match addr {
        IpAddr::V4(v4) => {
            let mut sa = vec![0u8; 16];
            sa[0] = 16;
            sa[1] = libc::AF_INET as u8;
            sa[4..8].copy_from_slice(&v4.octets());
            sa
        }
        IpAddr::V6(v6) => {
            let mut sa = vec![0u8; 28];
            sa[0] = 28;
            sa[1] = libc::AF_INET6 as u8;
            sa[8..24].copy_from_slice(&v6.octets());
            sa
        }
    }
}

/// Serialize the netmask sockaddr for a prefix length in `dest`'s family. Full
/// (untruncated) sockaddrs: the kernel accepts them and it keeps parsing dumb.
fn sockaddr_mask(dest: IpAddr, prefixlen: u8) -> Vec<u8> {
    match dest {
        IpAddr::V4(_) => {
            let mask = if prefixlen == 0 {
                0
            } else {
                u32::MAX << (32 - u32::from(prefixlen))
            };
            sockaddr_ip(IpAddr::V4(Ipv4Addr::from(mask)))
        }
        IpAddr::V6(_) => {
            let mask = if prefixlen == 0 {
                0
            } else {
                u128::MAX << (128 - u32::from(prefixlen))
            };
            sockaddr_ip(IpAddr::V6(Ipv6Addr::from(mask)))
        }
    }
}

/// Serialize an `AF_LINK` sockaddr_dl naming an interface by index.
fn sockaddr_link(ifindex: u16) -> Vec<u8> {
    let mut sa = vec![0u8; 20];
    sa[0] = 20;
    sa[1] = AF_LINK;
    sa[2..4].copy_from_slice(&ifindex.to_ne_bytes());
    sa
}

/// Build a complete routing message: header + the sockaddrs listed in
/// `rtm_addrs`, each padded to alignment.
fn build_message(
    msg_type: u8,
    seq: i32,
    spec: &RouteSpec,
    extra_flags: i32,
    want_ifp: bool,
) -> Vec<u8> {
    let mut addrs = RTA_DST;
    let mut body: Vec<u8> = Vec::new();

    let dst = sockaddr_ip(spec.dest);
    body.extend_from_slice(&dst);
    body.resize(roundup(body.len()), 0);

    match (msg_type, &spec.gateway) {
        // Deletes and gets identify the route by dst/mask (+scope); no gateway.
        (RTM_DELETE | RTM_GET, _) => {}
        (_, Gateway::Ip(ip)) => {
            addrs |= RTA_GATEWAY;
            let gw = sockaddr_ip(*ip);
            body.extend_from_slice(&gw);
            body.resize(roundup(body.len()), 0);
        }
        (_, Gateway::Interface(idx)) => {
            addrs |= RTA_GATEWAY;
            let gw = sockaddr_link(*idx);
            body.extend_from_slice(&gw);
            body.resize(roundup(body.len()), 0);
        }
    }

    if let Some(plen) = spec.prefixlen {
        addrs |= RTA_NETMASK;
        let mask = sockaddr_mask(spec.dest, plen);
        body.extend_from_slice(&mask);
        body.resize(roundup(body.len()), 0);
    }
    if want_ifp {
        addrs |= RTA_IFP;
    }

    let mut flags = RTF_UP | RTF_STATIC | extra_flags;
    if spec.prefixlen.is_none() {
        flags |= RTF_HOST;
    }
    if matches!(spec.gateway, Gateway::Ip(_)) && msg_type == RTM_ADD {
        flags |= RTF_GATEWAY;
    }
    if spec.ifscope.is_some() {
        flags |= RTF_IFSCOPE;
    }

    let mut msg = vec![0u8; RT_MSGHDR_LEN];
    put_u16(&mut msg, OFF_MSGLEN, (RT_MSGHDR_LEN + body.len()) as u16);
    msg[OFF_VERSION] = RTM_VERSION;
    msg[OFF_TYPE] = msg_type;
    put_u16(&mut msg, OFF_INDEX, spec.ifscope.unwrap_or(0));
    put_i32(&mut msg, OFF_FLAGS, flags);
    put_i32(&mut msg, OFF_ADDRS, addrs);
    put_i32(&mut msg, OFF_PID, 0);
    put_i32(&mut msg, OFF_SEQ, seq);
    msg.extend_from_slice(&body);
    msg
}

/// Parse one sockaddr at `buf[off..]`, returning (parsed IP if INET/INET6,
/// link ifindex if AF_LINK, next offset).
fn parse_sockaddr(buf: &[u8], off: usize) -> (Option<IpAddr>, Option<u16>, usize) {
    if off >= buf.len() {
        return (None, None, off);
    }
    let sa_len = buf[off] as usize;
    let next = off + roundup(sa_len);
    if sa_len < 2 {
        return (None, None, next);
    }
    let family = buf[off + 1];
    let ip = match family as i32 {
        libc::AF_INET if sa_len >= 8 => {
            let mut o = [0u8; 4];
            o.copy_from_slice(&buf[off + 4..off + 8]);
            Some(IpAddr::V4(Ipv4Addr::from(o)))
        }
        libc::AF_INET6 if sa_len >= 24 => {
            let mut o = [0u8; 16];
            o.copy_from_slice(&buf[off + 8..off + 24]);
            Some(IpAddr::V6(Ipv6Addr::from(o)))
        }
        _ => None,
    };
    let link = if family == AF_LINK && sa_len >= 4 {
        Some(get_u16(buf, off + 2))
    } else {
        None
    };
    (ip, link, next)
}

/// Parse an RTM_GET reply into the fields we use.
fn parse_get_reply(msg: &[u8]) -> io::Result<RouteInfo> {
    if msg.len() < RT_MSGHDR_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "routing reply shorter than rt_msghdr",
        ));
    }
    let flags = get_i32(msg, OFF_FLAGS);
    let addrs = get_i32(msg, OFF_ADDRS);
    let mut ifindex = get_u16(msg, OFF_INDEX);
    let mut gateway = None;
    let mut off = RT_MSGHDR_LEN;
    // Sockaddrs appear in RTA bit order for each bit set in rtm_addrs.
    for bit in 0..31 {
        let mask = 1i32 << bit;
        if addrs & mask == 0 {
            continue;
        }
        let (ip, link, next) = parse_sockaddr(msg, off);
        match mask {
            RTA_GATEWAY => {
                gateway = ip;
                if let Some(idx) = link {
                    ifindex = idx;
                }
            }
            RTA_IFP => {
                if let Some(idx) = link {
                    ifindex = idx;
                }
            }
            _ => {}
        }
        off = next;
        if off >= msg.len() {
            break;
        }
    }
    Ok(RouteInfo {
        gateway,
        ifindex,
        flags,
    })
}

/// A blocking `PF_ROUTE` socket with sequence-numbered request/reply matching.
pub struct RouteSocket {
    file: std::fs::File,
    seq: i32,
    pid: i32,
}

impl RouteSocket {
    pub fn new() -> io::Result<Self> {
        let fd = unsafe { libc::socket(PF_ROUTE, libc::SOCK_RAW, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let file = unsafe { <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(fd) };
        Ok(Self {
            file,
            seq: 0,
            pid: std::process::id() as i32,
        })
    }

    fn send(&mut self, msg: &[u8]) -> io::Result<()> {
        // Routing-socket writes are atomic; kernel verdicts arrive as errnos here.
        self.file.write_all(msg)
    }

    /// Add the route, replacing any existing entry for the same key. `EEXIST`
    /// is handled by delete-then-retry, so callers get upsert semantics with
    /// real errors for everything else.
    pub fn add(&mut self, spec: &RouteSpec) -> io::Result<()> {
        self.seq += 1;
        let msg = build_message(RTM_ADD, self.seq, spec, 0, false);
        match self.send(&msg) {
            Err(e) if e.raw_os_error() == Some(libc::EEXIST) => {
                self.delete(spec)?;
                self.seq += 1;
                let msg = build_message(RTM_ADD, self.seq, spec, 0, false);
                self.send(&msg)
            }
            other => other,
        }
    }

    /// Delete the route. Absent routes (`ESRCH`) are not an error: delete is
    /// idempotent for our teardown paths. Returns whether a route was removed.
    pub fn delete(&mut self, spec: &RouteSpec) -> io::Result<bool> {
        self.seq += 1;
        let msg = build_message(RTM_DELETE, self.seq, spec, 0, false);
        match self.send(&msg) {
            Ok(()) => Ok(true),
            Err(e) if e.raw_os_error() == Some(libc::ESRCH) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Look up the route the kernel would use for `spec.dest` (scoped to
    /// `spec.ifscope` if given). `Ok(None)` = no route (`ESRCH`).
    pub fn get(&mut self, spec: &RouteSpec) -> io::Result<Option<RouteInfo>> {
        self.seq += 1;
        let seq = self.seq;
        let msg = build_message(RTM_GET, seq, spec, 0, true);
        match self.send(&msg) {
            Ok(()) => {}
            Err(e) if e.raw_os_error() == Some(libc::ESRCH) => return Ok(None),
            Err(e) => return Err(e),
        }
        // The kernel echoes the answer to every PF_ROUTE listener; ours is
        // identified by (pid, seq). Skip unrelated broadcasts (other processes'
        // changes) until our reply arrives.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut buf = vec![0u8; 2048];
        loop {
            if Instant::now() > deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out waiting for RTM_GET reply",
                ));
            }
            let n = self.file.read(&mut buf)?;
            if n < RT_MSGHDR_LEN {
                continue;
            }
            let reply = &buf[..n];
            if reply[OFF_TYPE] != RTM_GET
                || get_i32(reply, OFF_PID) != self.pid
                || get_i32(reply, OFF_SEQ) != seq
            {
                continue;
            }
            let errno = get_i32(reply, OFF_ERRNO);
            if errno == libc::ESRCH {
                return Ok(None);
            }
            if errno != 0 {
                return Err(io::Error::from_raw_os_error(errno));
            }
            return Ok(Some(parse_get_reply(reply)?));
        }
    }
}

/// Convenience: `RawFd` access for callers that need to poll.
impl AsRawFd for RouteSocket {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.file.as_raw_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v4(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn header_layout_matches_darwin_abi() {
        let spec = RouteSpec {
            dest: v4("0.0.0.0"),
            prefixlen: Some(1),
            gateway: Gateway::Interface(25),
            ifscope: None,
        };
        let msg = build_message(RTM_ADD, 7, &spec, 0, false);
        // header + dst sockaddr_in (16) + sockaddr_dl (20) + mask sockaddr_in (16)
        assert_eq!(msg.len(), RT_MSGHDR_LEN + 16 + 20 + 16);
        assert_eq!(get_u16(&msg, OFF_MSGLEN) as usize, msg.len());
        assert_eq!(msg[OFF_VERSION], RTM_VERSION);
        assert_eq!(msg[OFF_TYPE], RTM_ADD);
        assert_eq!(get_i32(&msg, OFF_SEQ), 7);
        assert_eq!(get_i32(&msg, OFF_ADDRS), RTA_DST | RTA_GATEWAY | RTA_NETMASK);
        let flags = get_i32(&msg, OFF_FLAGS);
        assert_ne!(flags & RTF_UP, 0);
        assert_ne!(flags & RTF_STATIC, 0);
        assert_eq!(flags & RTF_GATEWAY, 0, "interface routes are not RTF_GATEWAY");
        assert_eq!(flags & RTF_IFSCOPE, 0);
        // dst sockaddr
        assert_eq!(msg[RT_MSGHDR_LEN], 16);
        assert_eq!(msg[RT_MSGHDR_LEN + 1], libc::AF_INET as u8);
        // gateway sockaddr_dl carries the interface index
        let gw_off = RT_MSGHDR_LEN + 16;
        assert_eq!(msg[gw_off], 20);
        assert_eq!(msg[gw_off + 1], AF_LINK);
        assert_eq!(get_u16(&msg, gw_off + 2), 25);
        // netmask 128.0.0.0 for /1
        let mask_off = gw_off + 20;
        assert_eq!(&msg[mask_off + 4..mask_off + 8], &[128, 0, 0, 0]);
    }

    #[test]
    fn scoped_default_sets_ifscope_and_index() {
        let spec = RouteSpec {
            dest: v4("0.0.0.0"),
            prefixlen: Some(0),
            gateway: Gateway::Ip(v4("10.100.0.1")),
            ifscope: Some(17),
        };
        let msg = build_message(RTM_ADD, 1, &spec, 0, false);
        assert_eq!(get_u16(&msg, OFF_INDEX), 17);
        let flags = get_i32(&msg, OFF_FLAGS);
        assert_ne!(flags & RTF_IFSCOPE, 0);
        assert_ne!(flags & RTF_GATEWAY, 0);
        // /0 netmask must be present (all-zero mask sockaddr), not treated as host.
        assert_eq!(flags & RTF_HOST, 0);
        assert_ne!(get_i32(&msg, OFF_ADDRS) & RTA_NETMASK, 0);
    }

    #[test]
    fn deletes_omit_gateway() {
        let spec = RouteSpec {
            dest: v4("128.0.0.0"),
            prefixlen: Some(1),
            gateway: Gateway::Interface(25),
            ifscope: None,
        };
        let msg = build_message(RTM_DELETE, 2, &spec, 0, false);
        assert_eq!(get_i32(&msg, OFF_ADDRS), RTA_DST | RTA_NETMASK);
        assert_eq!(msg.len(), RT_MSGHDR_LEN + 16 + 16);
    }

    #[test]
    fn v6_sockaddrs_and_masks() {
        let spec = RouteSpec {
            dest: "8000::".parse().unwrap(),
            prefixlen: Some(1),
            gateway: Gateway::Interface(3),
            ifscope: None,
        };
        let msg = build_message(RTM_ADD, 3, &spec, 0, false);
        // dst sockaddr_in6 (28 → padded 28), dl (20), mask in6 (28)
        assert_eq!(msg.len(), RT_MSGHDR_LEN + 28 + 20 + 28);
        assert_eq!(msg[RT_MSGHDR_LEN + 1], libc::AF_INET6 as u8);
        assert_eq!(msg[RT_MSGHDR_LEN + 8], 0x80, "8000:: first octet");
        let mask_off = RT_MSGHDR_LEN + 28 + 20;
        assert_eq!(msg[mask_off + 8], 0x80, "/1 mask first octet");
        assert_eq!(msg[mask_off + 9], 0x00);
    }

    #[test]
    fn get_reply_roundtrip() {
        // Synthesize a reply shaped like the kernel's: our own GET message for a
        // host dst, with a gateway sockaddr and an RTA_IFP sockaddr_dl appended.
        let mut msg = vec![0u8; RT_MSGHDR_LEN];
        msg[OFF_VERSION] = RTM_VERSION;
        msg[OFF_TYPE] = RTM_GET;
        put_i32(&mut msg, OFF_ADDRS, RTA_DST | RTA_GATEWAY | RTA_IFP);
        put_u16(&mut msg, OFF_INDEX, 0);
        put_i32(&mut msg, OFF_FLAGS, RTF_UP | RTF_GATEWAY);
        msg.extend_from_slice(&sockaddr_ip(v4("1.1.1.1")));
        msg.extend_from_slice(&sockaddr_ip(v4("10.100.0.1")));
        msg.extend_from_slice(&sockaddr_link(17));
        let total = msg.len() as u16;
        put_u16(&mut msg, OFF_MSGLEN, total);
        let info = parse_get_reply(&msg).unwrap();
        assert_eq!(info.gateway, Some(v4("10.100.0.1")));
        assert_eq!(info.ifindex, 17);
        assert_ne!(info.flags & RTF_GATEWAY, 0);
    }

    // Live smoke test: runs on the dev Mac. Read-only (RTM_GET needs no
    // privileges); tolerate sandboxed environments that deny PF_ROUTE.
    #[test]
    fn live_get_default_route() {
        let Ok(mut sock) = RouteSocket::new() else {
            eprintln!("PF_ROUTE unavailable in this environment; skipping");
            return;
        };
        let spec = RouteSpec {
            dest: v4("0.0.0.0"),
            prefixlen: Some(0),
            gateway: Gateway::Ip(v4("0.0.0.0")),
            ifscope: None,
        };
        match sock.get(&spec) {
            Ok(Some(info)) => {
                assert_ne!(info.ifindex, 0, "default route has an interface");
            }
            Ok(None) => eprintln!("no default route right now; nothing to assert"),
            Err(e) => eprintln!("RTM_GET denied ({e}); skipping"),
        }
    }
}
