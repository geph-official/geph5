//! macOS full-tunnel VPN backend: a `utun` device, split `/1` routes that steer all
//! traffic into it, an interface-scoped default route for loop prevention, a
//! sentinel DNS to avoid LAN-resolver leaks, and a fail-closed PF kill switch.
//!
//! Unlike Linux there is no uid-based policy routing, so the engine's own
//! bridge/exit/broker sockets are kept on the physical NIC by `IP_BOUND_IF`
//! pinning (the manager hands it the interface indices via
//! [`VpnHandle::bind_indices`] → `GEPH_VPN_BIND_IF4/6`; see geph5-client's
//! `bound_dialer.rs`). Because macOS scoped routing won't fall back to the global
//! default once the `/1` routes capture everything, [`add_scoped_default`] also
//! installs an interface-scoped default via the physical gateway so those pinned
//! sockets have a route out.
//!
//! The engine runs as the dedicated `_geph` account so the PF kill
//! switch can permit its egress by uid and fail closed on everything else. The tun
//! fd is passed to the child exactly like Linux (`dup2` onto fd 3 in the spawn
//! `pre_exec`, then `--vpn-fd 3`).
//!
//! Physical-route changes are monitored by the uniform VPN layer. Any drift
//! triggers a complete in-place reassertion of PF, routes, addressing, and DNS;
//! the manager then replaces only the engine child so it receives fresh binding
//! indices.
//!
//! macOS has no `PR_SET_PDEATHSIG`, so a *hard* manager crash (SIGKILL) leaves the
//! engine child running and still holding the utun fd, keeping the interface and
//! its routes alive with no supervisor. Manager startup runs [`cleanup_stale`]
//! before constructing fresh state, and `recover-geph.sh` remains the manual
//! escape hatch. Clean disconnect is unaffected — the manager kills the child
//! before dropping the handle.
//!
//! This file is compiled only on macOS (gated by the `mod` declaration), so no
//! inner `cfg` stub is needed.

use std::{
    ffi::CString,
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    process::Command,
};

use anyhow::{Context, bail};

use route_socket::{Gateway, RouteSocket, RouteSpec};

mod discovery;
mod dns;
mod firewall;
mod route_socket;

// tun addressing — v4 matches the Linux/Windows implementations.
const TUN_V4: &str = "100.64.0.1";
// Global-scope (2000::/3), NOT a fd00::/8 ULA: RFC 6724 source selection won't
// pair a ULA source with global v6 destinations, so with a ULA tun address
// macOS suppresses AAAA results (v6-only hosts fail to "resolve") and
// dual-stack hosts always pick v4 — v6 silently never uses the tunnel. The
// address itself is arbitrary because the exit NATs v6 from its own pool;
// 2001:db8::/32 (RFC 3849, documentation-only) is never routed on the real
// internet, so squatting it cannot shadow a reachable destination.
const TUN_V6: &str = "2001:db8:6765::1";
const TUN_V6_PREFIX: &str = "64";
const TUN_MTU: &str = "16384";

// Split-default routes: two `/1`s cover the whole address space and, being more
// specific than the physical `/0` default, win route lookups — so all traffic
// heads into the tun while the physical default is left intact underneath.
const V4_SPLIT: [Ipv4Addr; 2] = [Ipv4Addr::UNSPECIFIED, Ipv4Addr::new(128, 0, 0, 0)];
const V6_SPLIT: [Ipv6Addr; 2] = [
    Ipv6Addr::UNSPECIFIED,
    Ipv6Addr::new(0x8000, 0, 0, 0, 0, 0, 0, 0),
];
const SPLIT_PREFIXLEN: u8 = 1;

// Sentinel DNS: the engine routes these into the tunnel (and rewrites port-53
// traffic), so pointing the system at them keeps DNS from leaking to the LAN
// resolver that the host would otherwise use.
const SENTINEL_DNS_V4: &str = "1.1.1.1";
const SENTINEL_DNS_V6: &str = "2606:4700:4700::1111";

// ---- utun control-socket constants (from <sys/sys_domain.h>, <net/if_utun.h>) ----
// Defined locally (like the Linux backend's TUNSETIFF) so we don't depend on libc exposing
// every apple-specific item.
const PF_SYSTEM: libc::c_int = 32;
const SYSPROTO_CONTROL: libc::c_int = 2;
const AF_SYSTEM: u8 = 32;
const AF_SYS_CONTROL: u16 = 2;
const UTUN_OPT_IFNAME: libc::c_int = 2;
// CTLIOCGINFO = _IOWR('N', 3, struct ctl_info); sizeof(ctl_info) == 100.
const CTLIOCGINFO: libc::c_ulong = 0xC064_4E03;
const UTUN_CONTROL_NAME: &[u8] = b"com.apple.net.utun_control";

#[repr(C)]
struct CtlInfo {
    ctl_id: u32,
    ctl_name: [libc::c_char; 96],
}

#[repr(C)]
struct SockaddrCtl {
    sc_len: u8,
    sc_family: u8,
    ss_sysaddr: u16,
    sc_id: u32,
    sc_unit: u32,
    sc_reserved: [u32; 5],
}

/// Physical egress details the engine and routing need: the interface indices for
/// the engine's `IP_BOUND_IF` socket pinning, plus the `(interface, gateway)` pairs
/// used to install the interface-scoped default routes that let those pinned
/// sockets escape the `/1` capture.
#[derive(Clone, PartialEq, Eq)]
pub(super) struct PhysIface {
    if4: u32,
    if6: u32,
    v4_gw: (String, Ipv4Addr),
    v6_gw: Option<(String, Ipv6Addr)>,
}

/// Owns the utun fd; the device (and its routes) live as long as this handle, so
/// routing survives child restarts.
pub(super) struct VpnHandle {
    tun: OwnedFd,
    ifname: String,
    phys: PhysIface,
    dns_backup: dns::DnsBackup,
    /// PF state captured when the kill switch was applied; `None` until then.
    pf_state: Option<firewall::PfState>,
}

impl VpnHandle {
    /// Raw utun fd, to be dup'd into the child.
    pub(super) fn tun_fd(&self) -> RawFd {
        self.tun.as_raw_fd()
    }

    /// Physical interface indices passed to the engine for `IP_BOUND_IF` pinning.
    pub(super) fn bind_indices(&self) -> (u32, u32) {
        (self.phys.if4, self.phys.if6)
    }

    fn cleanup(&mut self) {
        // Lift the kill switch first so teardown traffic isn't blocked.
        if let Some(state) = self.pf_state.take() {
            firewall::teardown(state);
        }
        dns::restore(&self.dns_backup);
        del_scoped_default(&self.phys);
        delete_split_routes();
        // These explicit deletes are what actually tear routing down: callers kill
        // the engine child first (see reconcile_tunnel), but during a live session
        // that child holds a dup of the utun fd, so closing our OwnedFd alone would
        // NOT remove the interface — the routes must be deleted by hand.

        // Teardown must leave the machine online: if configd hasn't re-asserted
        // the physical default (its router probes were blackholed behind the
        // kill switch), restore it from the captured gateway, first clearing a
        // leftover default that egresses through a tunnel (the dying utun's).
        let (ifn, gw) = &self.phys.v4_gw;
        if let Err(error) = restore_default_route(*gw) {
            tracing::warn!(
                %error,
                gateway = %gw,
                interface = %ifn,
                "could not restore physical default route after VPN teardown"
            );
        }
    }
}

/// Ensure the global IPv4 default egresses through a physical interface,
/// installing `gw` if it is missing or still points at a tunnel.
fn restore_default_route(gw: Ipv4Addr) -> anyhow::Result<()> {
    let mut rs = RouteSocket::new().context("opening routing socket")?;
    let probe = RouteSpec {
        dest: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        prefixlen: Some(0),
        gateway: Gateway::Ip(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
        ifscope: None,
    };
    if let Some(info) = rs.get(&probe)? {
        match iface_name(info.ifindex) {
            Some(name) if !is_tunnel_iface(&name) => return Ok(()),
            _ => {
                rs.delete(&probe)?;
            }
        }
    }
    tracing::warn!(gateway = %gw, "physical default route missing after VPN teardown; restoring it");
    rs.add(&RouteSpec {
        dest: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        prefixlen: Some(0),
        gateway: Gateway::Ip(IpAddr::V4(gw)),
        ifscope: None,
    })
    .context("re-adding physical default route")
}

/// Tear down a live VPN, restoring the exact DNS and routing state it replaced.
pub(super) fn cleanup(mut handle: VpnHandle) {
    handle.cleanup();
}

/// Discover the physical interface(s) from configd's primary-service state,
/// which names the real NIC and its router regardless of what routes our own
/// tunnel has installed.
pub(super) fn physical_iface() -> anyhow::Result<PhysIface> {
    let v4 = discovery::primary_v4()?;
    let if4 = iface_index(&v4.ifname)?;
    // v6 is best-effort: many hosts have no v6 service, in which case the engine
    // runs v4-only. A dual-stack NIC shares one index, so reuse if4 as a fallback.
    let (if6, v6_gw) = match discovery::primary_v6() {
        Ok(v6) => (
            iface_index(&v6.ifname).unwrap_or(if4),
            Some((v6.ifname, v6.router)),
        ),
        Err(_) => (if4, None),
    };
    Ok(PhysIface {
        if4,
        if6,
        v4_gw: (v4.ifname, v4.router),
        v6_gw,
    })
}

/// Bring up the full-tunnel VPN for the engine running as `uid`: utun device,
/// addressing, split routes, the interface-scoped default, sentinel DNS, and the
/// PF kill switch (which permits the engine's egress by `uid`). `allow_lan` opens
/// the private/link-local ranges in the kill switch.
pub(super) fn setup(phys: PhysIface, uid: u32, allow_lan: bool) -> anyhow::Result<VpnHandle> {
    let (tun, ifname) = create_utun().context("creating utun device")?;
    let mut handle = scopeguard::guard(
        VpnHandle {
            tun,
            ifname,
            phys,
            dns_backup: Vec::new(),
            pf_state: None,
        },
        |mut handle| handle.cleanup(),
    );
    handle.bring_up(uid, allow_lan)?;
    Ok(scopeguard::ScopeGuard::into_inner(handle))
}

impl VpnHandle {
    /// Apply addressing, routes, DNS, and the kill switch. Separated from
    /// construction so a mid-way failure unwinds through the setup scope guard.
    fn bring_up(&mut self, uid: u32, allow_lan: bool) -> anyhow::Result<()> {
        let ifname = self.ifname.clone();

        run(
            "ifconfig",
            &[
                ifname.as_str(),
                "inet",
                TUN_V4,
                TUN_V4,
                "mtu",
                TUN_MTU,
                "up",
            ],
        )
        .context("configuring utun IPv4")?;
        // Best-effort v6 (the host may have no v6 at all). macOS ifconfig uses the
        // `prefixlen` keyword, not the `addr/len` slash form.
        let _ = run(
            "ifconfig",
            &[
                ifname.as_str(),
                "inet6",
                TUN_V6,
                "prefixlen",
                TUN_V6_PREFIX,
                "up",
            ],
        );

        // Enable the all-interface kill switch before capture routes are installed.
        // Record the token first so the setup guard can release our reference if
        // loading the rules fails.
        self.pf_state = Some(firewall::apply(&ifname, uid, allow_lan).context("applying PF kill switch")?);

        upsert_split_routes(&ifname)?;

        // Interface-scoped default route via the physical gateway. macOS scoped
        // routing (IP_BOUND_IF, used by the engine's bound dialer to keep its own
        // bridge/exit/broker traffic on the physical NIC) returns ENETUNREACH once
        // the /1 routes capture the destination — the most-specific match is on the
        // utun, and the kernel won't fall back to the global default for a scoped
        // socket. An explicit interface-scoped default gives those pinned sockets a
        // way out. It applies ONLY to sockets explicitly bound to the interface, so
        // normal traffic still follows the /1 into the tunnel (no leak). Fatal if
        // it fails: without it the engine can't reach any exit.
        add_scoped_default(&self.phys).context("installing interface-scoped default route")?;

        self.dns_backup = dns::set_sentinel(SENTINEL_DNS_V4, SENTINEL_DNS_V6);
        Ok(())
    }
}

/// Reassert the complete macOS VPN configuration without replacing the live
/// utun. PF is loaded first and blocks all physical egress, so route repair and
/// physical-interface changes remain fail-closed.
pub(super) fn reconcile(
    handle: &mut VpnHandle,
    uid: u32,
    allow_lan: bool,
) -> anyhow::Result<()> {
    // Re-derive the physical egress so a real network switch (Wi-Fi <-> Ethernet)
    // is picked up, but fall back to the interface captured at setup when the live
    // default route can't be resolved to a real NIC — the network is momentarily
    // down, or the default is transiently owned by the engine's own utun during
    // bring-up. Rebuilding the scoped route from a tunnel would install a bogus
    // link gateway and wedge every reconcile; the last-known-good physical stays
    // valid until the network genuinely changes.
    let physical = match physical_iface() {
        Ok(p) => p,
        Err(e) => {
            tracing::debug!("keeping last-known physical interface; rediscovery failed: {e:#}");
            handle.phys.clone()
        }
    };
    let ifname = handle.ifname.clone();
    firewall::apply(&ifname, uid, allow_lan).context("reasserting PF kill switch")?;
    run(
        "ifconfig",
        &[
            ifname.as_str(),
            "inet",
            TUN_V4,
            TUN_V4,
            "mtu",
            TUN_MTU,
            "up",
        ],
    )
    .context("reasserting utun IPv4")?;
    let _ = run(
        "ifconfig",
        &[
            ifname.as_str(),
            "inet6",
            TUN_V6,
            "prefixlen",
            TUN_V6_PREFIX,
            "up",
        ],
    );
    upsert_split_routes(&ifname)?;

    // Add the new scoped escape route before retiring the old one. These routes
    // affect only explicitly-bound engine sockets; ordinary traffic remains
    // captured by the /1 routes and PF throughout.
    add_scoped_default(&physical).context("reasserting interface-scoped default route")?;
    if handle.phys != physical {
        del_scoped_default(&handle.phys);
    }
    handle.phys = physical;
    dns::reassert_sentinel(SENTINEL_DNS_V4, SENTINEL_DNS_V6);
    Ok(())
}

/// Install the four `/1` split routes onto the tun. IPv4 failures are fatal
/// (the tunnel is useless without capture); IPv6 is best-effort but loud, since
/// silently missing v6 splits means v6 traffic bypasses the tunnel.
fn upsert_split_routes(ifname: &str) -> anyhow::Result<()> {
    let idx = iface_index(ifname)? as u16;
    let mut rs = RouteSocket::new().context("opening routing socket")?;
    for net in V4_SPLIT {
        rs.add(&RouteSpec {
            dest: IpAddr::V4(net),
            prefixlen: Some(SPLIT_PREFIXLEN),
            gateway: Gateway::Interface(idx),
            ifscope: None,
        })
        .with_context(|| format!("installing split route {net}/1 via {ifname}"))?;
    }
    for net in V6_SPLIT {
        if let Err(error) = rs.add(&RouteSpec {
            dest: IpAddr::V6(net),
            prefixlen: Some(SPLIT_PREFIXLEN),
            gateway: Gateway::Interface(idx),
            ifscope: None,
        }) {
            tracing::warn!(
                %net,
                ifname,
                %error,
                "IPv6 split route failed; IPv6 will not be tunneled"
            );
        }
    }
    Ok(())
}

/// Remove the split routes (idempotent; absent routes are not an error).
fn delete_split_routes() {
    let Ok(mut rs) = RouteSocket::new() else {
        return;
    };
    for net in V4_SPLIT {
        let _ = rs.delete(&RouteSpec {
            dest: IpAddr::V4(net),
            prefixlen: Some(SPLIT_PREFIXLEN),
            gateway: Gateway::Interface(0),
            ifscope: None,
        });
    }
    for net in V6_SPLIT {
        let _ = rs.delete(&RouteSpec {
            dest: IpAddr::V6(net),
            prefixlen: Some(SPLIT_PREFIXLEN),
            gateway: Gateway::Interface(0),
            ifscope: None,
        });
    }
}

/// Open a `utun` control socket, let the kernel pick the next free unit, and return
/// the fd plus the assigned interface name (e.g. `"utun7"`).
fn create_utun() -> anyhow::Result<(OwnedFd, String)> {
    let fd = unsafe { libc::socket(PF_SYSTEM, libc::SOCK_DGRAM, SYSPROTO_CONTROL) };
    if fd < 0 {
        return Err(io::Error::last_os_error()).context("socket(PF_SYSTEM)");
    }
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };

    // Resolve the utun kernel-control id by name.
    let mut info = CtlInfo {
        ctl_id: 0,
        ctl_name: [0; 96],
    };
    for (i, b) in UTUN_CONTROL_NAME.iter().enumerate() {
        info.ctl_name[i] = *b as libc::c_char;
    }
    if unsafe { libc::ioctl(owned.as_raw_fd(), CTLIOCGINFO, &mut info as *mut CtlInfo) } < 0 {
        return Err(io::Error::last_os_error()).context("ioctl(CTLIOCGINFO)");
    }

    // sc_unit = 0 → kernel picks the next free utun unit.
    let sc = SockaddrCtl {
        sc_len: std::mem::size_of::<SockaddrCtl>() as u8,
        sc_family: AF_SYSTEM,
        ss_sysaddr: AF_SYS_CONTROL,
        sc_id: info.ctl_id,
        sc_unit: 0,
        sc_reserved: [0; 5],
    };
    let rc = unsafe {
        libc::connect(
            owned.as_raw_fd(),
            &sc as *const SockaddrCtl as *const libc::sockaddr,
            std::mem::size_of::<SockaddrCtl>() as libc::socklen_t,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error()).context("connect(utun control)");
    }

    // Read back the assigned interface name.
    let mut name_buf = [0u8; 64];
    let mut name_len = name_buf.len() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            owned.as_raw_fd(),
            SYSPROTO_CONTROL,
            UTUN_OPT_IFNAME,
            name_buf.as_mut_ptr() as *mut libc::c_void,
            &mut name_len,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error()).context("getsockopt(UTUN_OPT_IFNAME)");
    }
    let end = name_buf
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(name_buf.len());
    let ifname = String::from_utf8_lossy(&name_buf[..end]).into_owned();

    // CLOEXEC so this copy doesn't leak into unrelated children; the child gets a
    // fresh (non-cloexec) dup via the spawn pre_exec dup2. macOS sockets have no
    // atomic SOCK_CLOEXEC, so there is a tiny window between socket() and this
    // fcntl in which a concurrent spawn could inherit the fd — acceptable, as the
    // manager does not spawn children during VPN setup.
    unsafe {
        libc::fcntl(owned.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
    }

    Ok((owned, ifname))
}

/// Interfaces that must never be treated as the physical egress: our own tunnel
/// and other VPNs' tunnels.
pub(self) fn is_tunnel_iface(name: &str) -> bool {
    name.starts_with("utun") || name.starts_with("ipsec")
}

/// Kernel index for an interface name.
fn iface_index(name: &str) -> anyhow::Result<u32> {
    let c = CString::new(name)?;
    let idx = unsafe { libc::if_nametoindex(c.as_ptr()) };
    if idx == 0 {
        bail!("if_nametoindex({name}) failed");
    }
    Ok(idx)
}

/// Interface name for a kernel index; `None` if the interface no longer exists.
fn iface_name(index: u16) -> Option<String> {
    let mut buf = [0u8; libc::IFNAMSIZ];
    let ret =
        unsafe { libc::if_indextoname(u32::from(index), buf.as_mut_ptr() as *mut libc::c_char) };
    if ret.is_null() {
        return None;
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    Some(String::from_utf8_lossy(&buf[..end]).into_owned())
}

/// A link-local v6 gateway is only meaningful with its interface scope; Darwin
/// expects the scope embedded KAME-style in octets 2–3 of the address.
fn kame_scoped(gw: Ipv6Addr, ifindex: u16) -> Ipv6Addr {
    if (gw.segments()[0] & 0xffc0) == 0xfe80 {
        let mut o = gw.octets();
        o[2..4].copy_from_slice(&ifindex.to_be_bytes());
        Ipv6Addr::from(o)
    } else {
        gw
    }
}

/// Install the interface-scoped default route(s) the engine's IP_BOUND_IF sockets
/// use to bypass the `/1` capture. IPv4 is load-bearing (errors propagate); v6 is
/// best-effort. Idempotent (`add` replaces an existing entry).
fn add_scoped_default(phys: &PhysIface) -> anyhow::Result<()> {
    let mut rs = RouteSocket::new().context("opening routing socket")?;
    let (ifn, gw) = &phys.v4_gw;
    let idx = iface_index(ifn)? as u16;
    rs.add(&RouteSpec {
        dest: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        prefixlen: Some(0),
        gateway: Gateway::Ip(IpAddr::V4(*gw)),
        ifscope: Some(idx),
    })
    .with_context(|| format!("scoped default via {gw} on {ifn}"))?;
    if let Some((ifn6, gw6)) = &phys.v6_gw
        && let Ok(idx6) = iface_index(ifn6)
    {
        let _ = rs.add(&RouteSpec {
            dest: IpAddr::V6(Ipv6Addr::UNSPECIFIED),
            prefixlen: Some(0),
            gateway: Gateway::Ip(IpAddr::V6(kame_scoped(*gw6, idx6 as u16))),
            ifscope: Some(idx6 as u16),
        });
    }
    Ok(())
}

/// Remove the interface-scoped default route(s). Best-effort / idempotent.
fn del_scoped_default(phys: &PhysIface) {
    let Ok(mut rs) = RouteSocket::new() else {
        return;
    };
    if let Ok(idx) = iface_index(&phys.v4_gw.0) {
        let _ = rs.delete(&RouteSpec {
            dest: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            prefixlen: Some(0),
            gateway: Gateway::Ip(IpAddr::V4(phys.v4_gw.1)),
            ifscope: Some(idx as u16),
        });
    }
    if let Some((ifn6, gw6)) = &phys.v6_gw
        && let Ok(idx6) = iface_index(ifn6)
    {
        let _ = rs.delete(&RouteSpec {
            dest: IpAddr::V6(Ipv6Addr::UNSPECIFIED),
            prefixlen: Some(0),
            gateway: Gateway::Ip(IpAddr::V6(kame_scoped(*gw6, idx6 as u16))),
            ifscope: Some(idx6 as u16),
        });
    }
}





#[derive(Clone)]
pub(super) struct NetworkSnapshot {
    phys: PhysIface,
}

pub(super) fn network_snapshot(handle: &VpnHandle) -> NetworkSnapshot {
    NetworkSnapshot {
        phys: handle.phys.clone(),
    }
}

/// Check that the captured routing is still valid — sleep/wake invalidates the
/// interface-scoped route, and a network switch changes the physical interface —
/// reporting whether a full reconciliation is needed. This probe never mutates
/// host networking.
pub(super) fn network_check(snapshot: &NetworkSnapshot) -> super::NetworkAction {
    // No usable default route right now (e.g. network momentarily down): don't
    // thrash — wait for it to come back.
    let cur = match physical_iface() {
        Ok(c) => c,
        Err(_) => return super::NetworkAction::Healthy,
    };
    if cur.if4 != snapshot.phys.if4 || cur.v4_gw != snapshot.phys.v4_gw {
        return super::NetworkAction::Reconcile;
    }
    if !scoped_default_ok(&snapshot.phys) {
        return super::NetworkAction::Reconcile;
    }
    super::NetworkAction::Healthy
}

/// Does an interface-scoped lookup still resolve out the physical gateway? (After
/// sleep the scoped route can linger in the table but stop resolving.)
fn scoped_default_ok(phys: &PhysIface) -> bool {
    let (ifn, gw) = &phys.v4_gw;
    let Ok(idx) = iface_index(ifn) else {
        return false;
    };
    let Ok(mut rs) = RouteSocket::new() else {
        return false;
    };
    match rs.get(&RouteSpec {
        dest: IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)),
        prefixlen: None,
        gateway: Gateway::Ip(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
        ifscope: Some(idx as u16),
    }) {
        Ok(Some(info)) => {
            info.gateway == Some(IpAddr::V4(*gw)) && u32::from(info.ifindex) == idx
        }
        _ => false,
    }
}

// PF_ROUTE: the kernel's routing socket. AF_ROUTE == PF_ROUTE == 17 on Darwin.
const PF_ROUTE: libc::c_int = 17;

/// Block on a `PF_ROUTE` socket, invoking `on_change` on every routing-table change
/// (sleep/wake, DHCP renewal, interface up/down). Runs forever on its own thread,
/// so the watchdog reacts to a wake within ~a second instead of waiting out a poll;
/// returns only if the socket can't be opened or read (the periodic backstop
/// remains). We don't parse the messages — any message means "re-check".
pub(super) fn route_change_loop(mut on_change: impl FnMut()) {
    let fd = unsafe { libc::socket(PF_ROUTE, libc::SOCK_RAW, 0) };
    if fd < 0 {
        tracing::warn!("could not open PF_ROUTE socket; relying on periodic VPN checks only");
        return;
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut buf = [0u8; 4096];
    loop {
        let n = unsafe {
            libc::read(
                fd.as_raw_fd(),
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
            )
        };
        if n <= 0 {
            if n < 0 && std::io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        on_change();
    }
}

/// Purge stale VPN state left by a crashed/`-9`-killed prior instance, so a fresh
/// manager doesn't start on a half-configured machine. Best-effort / idempotent.
pub(super) fn cleanup_stale() {
    firewall::teardown_stale();
    // Pre-0.4 versions replaced the PF main ruleset outright; put the system's
    // back in case one of those crashed here. Harmless otherwise.
    let _ = run("pfctl", &["-f", "/etc/pf.conf"]);
    delete_split_routes();
    dns::cleanup_stale(SENTINEL_DNS_V4, SENTINEL_DNS_V6);
    // Pre-0.4 versions wrote sentinel DNS into persistent preferences via
    // networksetup; sweep those back to DHCP too.
    let services = cmd_output("networksetup", &["-listallnetworkservices"]).unwrap_or_default();
    for svc in services.lines().skip(1).filter(|l| !l.starts_with('*')) {
        let cur = cmd_output("networksetup", &["-getdnsservers", svc]).unwrap_or_default();
        if cur.contains(SENTINEL_DNS_V4) || cur.contains(SENTINEL_DNS_V6) {
            let _ = run("networksetup", &["-setdnsservers", svc, "Empty"]);
        }
    }
}

// ---- command helpers ----

fn run(cmd: &str, args: &[&str]) -> anyhow::Result<()> {
    cmd_output(cmd, args).map(|_| ())
}

fn cmd_output(cmd: &str, args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new(cmd)
        .args(args)
        .output()
        .with_context(|| format!("spawning {cmd}"))?;
    if !out.status.success() {
        bail!(
            "`{cmd} {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::{is_tunnel_iface, kame_scoped};

    #[test]
    fn tunnel_ifaces_are_never_physical() {
        assert!(is_tunnel_iface("utun0"));
        assert!(is_tunnel_iface("utun4"));
        assert!(is_tunnel_iface("ipsec0"));
        assert!(!is_tunnel_iface("en0"));
        assert!(!is_tunnel_iface("en1"));
        assert!(!is_tunnel_iface("bridge0"));
    }

    #[test]
    fn kame_scoping_embeds_index_for_link_local_only() {
        let ll: std::net::Ipv6Addr = "fe80::1".parse().unwrap();
        let scoped = kame_scoped(ll, 17);
        assert_eq!(scoped.octets()[2..4], 17u16.to_be_bytes());
        let global: std::net::Ipv6Addr = "2606:4700::1".parse().unwrap();
        assert_eq!(kame_scoped(global, 17), global);
    }
}
