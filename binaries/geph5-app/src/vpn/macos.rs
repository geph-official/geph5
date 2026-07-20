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
    io::{self, Write},
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    process::{Command, Stdio},
};

use anyhow::{Context, bail};

/// Dedicated unprivileged user the engine runs as, so the PF kill switch can
/// permit its egress by uid (the macOS analogue of Linux's `geph5-daemon`).
/// PF anchor name holding our kill-switch rules.
const PF_ANCHOR: &str = "geph";

// tun addressing — matches the Linux/Windows implementations.
const TUN_V4: &str = "100.64.0.1";
const TUN_V6: &str = "fd00:6765::1";
const TUN_V6_PREFIX: &str = "64";
const TUN_MTU: &str = "16384";

// Split-default routes: two `/1`s cover the whole address space and, being more
// specific than the physical `/0` default, win route lookups — so all traffic
// heads into the tun while the physical default is left intact underneath.
const V4_SPLIT: [&str; 2] = ["0.0.0.0/1", "128.0.0.0/1"];
const V6_SPLIT: [&str; 2] = ["::/1", "8000::/1"];

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
    v4_gw: (String, String),
    v6_gw: Option<(String, String)>,
}

/// Owns the utun fd; the device (and its routes) live as long as this handle, so
/// routing survives child restarts.
pub(super) struct VpnHandle {
    tun: OwnedFd,
    ifname: String,
    phys: PhysIface,
    /// Per network service: its DNS servers before we overrode them (empty = DHCP).
    dns_backup: Vec<(String, Vec<String>)>,
    /// PF enable-reference token from `pfctl -E`, released on teardown.
    pf_token: Option<String>,
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
        if let Some(token) = &self.pf_token {
            pf_teardown(token);
        }
        restore_dns(&self.dns_backup);
        del_scoped_default(&self.phys);
        for net in V4_SPLIT {
            let _ = run(
                "route",
                &["-n", "delete", "-net", net, "-interface", &self.ifname],
            );
        }
        for net in V6_SPLIT {
            let _ = run(
                "route",
                &[
                    "-n",
                    "delete",
                    "-inet6",
                    "-net",
                    net,
                    "-interface",
                    &self.ifname,
                ],
            );
        }
        // These explicit deletes are what actually tear routing down: callers kill
        // the engine child first (see reconcile_tunnel), but during a live session
        // that child holds a dup of the utun fd, so closing our OwnedFd alone would
        // NOT remove the interface — the routes must be deleted by hand.

        // Teardown must leave the machine online. Mid-session the tunnel owns the
        // *global* default and the physical default survives only as our own
        // interface-scoped copy — which del_scoped_default above just removed. If
        // configd doesn't re-assert the physical default (its router probes were
        // blackholed behind the kill switch), the machine is stranded offline
        // until a DHCP renewal or reboot. Like the DNS restore, put back the
        // pre-VPN default from the captured physical gateway.
        //
        // Retried briefly because utun destruction is asynchronous: right after
        // the engine child dies, the tunnel's global default can still sit in the
        // table, making a plain `route add` collide with it ("File exists") and
        // then vanish. Each round clears whatever global default remains — we
        // only get here when no *physical* default exists, so anything present is
        // the dying tunnel's — and installs the real one.
        let (ifn, gw) = &self.phys.v4_gw;
        for _ in 0..5 {
            let present = cmd_output("netstat", &["-rn"])
                .is_ok_and(|t| parse_physical_default_v4(&t).is_ok());
            if present {
                return;
            }
            tracing::warn!(
                gateway = %gw,
                interface = %ifn,
                "physical default route missing after VPN teardown; restoring it"
            );
            let _ = run("route", &["-n", "delete", "-net", "default"]);
            if let Err(error) = run("route", &["-n", "add", "-net", "default", gw]) {
                tracing::warn!(%error, "restoring physical default route failed");
            }
            // Brief blocking pause is acceptable here: teardown correctness (the
            // machine coming back online) outweighs a moment of reactor delay.
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
    }
}

/// Tear down a live VPN, restoring the exact DNS and routing state it replaced.
pub(super) fn cleanup(mut handle: VpnHandle) {
    handle.cleanup();
}

/// Discover the physical interface(s) behind the current default route(s). Must be
/// called *before* [`setup`] installs the `/1` routes, while the default route
/// still reflects the real NIC.
pub(super) fn physical_iface() -> anyhow::Result<PhysIface> {
    // Read the routing TABLE (`netstat -rn`), not a route LOOKUP (`route get
    // default`). Once our `/1` split routes are installed, a lookup of 0.0.0.0
    // resolves into `0.0.0.0/1` and deterministically returns our own utun with a
    // non-IP link gateway — so lookup-based discovery can never work mid-session.
    // The table still lists the physical `default` entry alongside the splits.
    // Tunnel interfaces and non-IP gateways are skipped by the parser: the
    // physical NIC is never a tunnel, and adopting one would build an
    // interface-scoped default that the kernel rejects ("bad address"), wedging
    // every reconcile.
    let table = cmd_output("netstat", &["-rn"])?;
    let (ifname4, gw4) = parse_physical_default_v4(&table)
        .context("no IPv4 default route — not connected to a network?")?;
    let if4 = iface_index(&ifname4)?;
    // v6 is best-effort: many hosts have no v6 default, in which case the engine
    // runs v4-only. A dual-stack NIC shares one index, so reuse if4 as a fallback.
    let (if6, v6_gw) = match default_route(&["-n", "get", "-inet6", "default"]) {
        Ok((ifname6, gw6)) if !is_tunnel_iface(&ifname6) => {
            (iface_index(&ifname6).unwrap_or(if4), Some((ifname6, gw6)))
        }
        _ => (if4, None),
    };
    Ok(PhysIface {
        if4,
        if6,
        v4_gw: (ifname4, gw4),
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
            pf_token: None,
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
        self.pf_token = Some(pf_enable().context("enabling PF")?);
        pf_load(&pf_ruleset(&ifname, uid, allow_lan)).context("loading PF kill switch")?;

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

        self.dns_backup = set_sentinel_dns();
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
    pf_load(&pf_ruleset(&ifname, uid, allow_lan)).context("reasserting PF kill switch")?;
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
    reassert_sentinel_dns();
    Ok(())
}

fn upsert_split_routes(ifname: &str) -> anyhow::Result<()> {
    for net in V4_SPLIT {
        if run(
            "route",
            &["-n", "change", "-net", net, "-interface", ifname],
        )
        .is_err()
        {
            run("route", &["-n", "add", "-net", net, "-interface", ifname])
                .with_context(|| format!("adding route {net}"))?;
        }
    }
    for net in V6_SPLIT {
        if run(
            "route",
            &["-n", "change", "-inet6", "-net", net, "-interface", ifname],
        )
        .is_err()
        {
            let _ = run(
                "route",
                &["-n", "add", "-inet6", "-net", net, "-interface", ifname],
            );
        }
    }
    Ok(())
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

/// Run `route get <args>` and parse the physical default it reports.
fn default_route(route_args: &[&str]) -> anyhow::Result<(String, String)> {
    parse_default_route(&cmd_output("route", route_args)?)
}

/// Parse the `interface:`/`gateway:` lines from `route get` output, validating
/// that they describe a usable physical default (real interface + IP gateway).
///
/// Split from the command call so it can be unit-tested against captured output.
/// Critically, `route -n get default` with no default route present writes
/// "not in table" to *stderr* and exits **0** (verified on Darwin 25) — so this
/// sees empty stdout and must return a clean error, which is what lets callers
/// treat "no default route right now" as a transient (retry / keep last-known)
/// rather than crashing the connect.
fn parse_default_route(out: &str) -> anyhow::Result<(String, String)> {
    let mut ifname = None;
    let mut gateway = None;
    for line in out.lines() {
        let l = line.trim();
        if let Some(v) = l.strip_prefix("interface:") {
            ifname = Some(v.trim().to_string());
        } else if let Some(v) = l.strip_prefix("gateway:") {
            gateway = Some(v.trim().to_string());
        }
    }
    let ifname = ifname.context("`route get default` had no interface line")?;
    let gateway = gateway.context("`route get default` had no gateway line")?;
    // A gatewayless point-to-point route renders its gateway as a link reference
    // (e.g. `index: 25 utun4`), not an IP. That is meaningless as the gateway
    // argument to `route add`, so reject it here rather than shell out a command
    // the kernel refuses with "bad address" — the value that started this whole
    // fire drill.
    if !gateway_is_ip(&gateway) {
        bail!("default route via {ifname} has a non-IP gateway {gateway:?}");
    }
    Ok((ifname, gateway))
}

/// Find the physical IPv4 default in `netstat -rn` output: the first `default`
/// row in the `Internet:` section whose gateway is a real IP and whose interface
/// is not a tunnel. Row format (Darwin): `Destination Gateway Flags Netif
/// [Expire]`. Returns `(ifname, gateway)`.
fn parse_physical_default_v4(table: &str) -> anyhow::Result<(String, String)> {
    let mut in_v4 = false;
    for line in table.lines() {
        let l = line.trim();
        if l == "Internet:" {
            in_v4 = true;
            continue;
        }
        if l == "Internet6:" {
            break;
        }
        if !in_v4 {
            continue;
        }
        let mut parts = l.split_whitespace();
        if parts.next() != Some("default") {
            continue;
        }
        let (Some(gw), Some(_flags), Some(netif)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if !is_tunnel_iface(netif) && gateway_is_ip(gw) {
            return Ok((netif.to_string(), gw.to_string()));
        }
    }
    bail!("routing table has no physical IPv4 default entry")
}

/// Interfaces that must never be treated as the physical egress: our own tunnel
/// and other VPNs' tunnels. A `route add default … -ifscope <tunnel>` with the
/// tunnel's link gateway is invalid and only ever appears when discovery races a
/// tunnel that has (transiently) grabbed the default route.
fn is_tunnel_iface(name: &str) -> bool {
    name.starts_with("utun") || name.starts_with("ipsec")
}

/// A usable route gateway is a real IP address. IPv6 gateways may carry a
/// `%zone` scope suffix (e.g. `fe80::1%en0`), which is stripped before parsing.
fn gateway_is_ip(gw: &str) -> bool {
    let addr = gw.split('%').next().unwrap_or(gw);
    addr.parse::<std::net::IpAddr>().is_ok()
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

/// Install the interface-scoped default route(s) the engine's IP_BOUND_IF sockets
/// use to bypass the `/1` capture. IPv4 is load-bearing (errors propagate); v6 is
/// best-effort. Idempotent (clears any stale copy first).
fn add_scoped_default(phys: &PhysIface) -> anyhow::Result<()> {
    let (ifn, gw) = &phys.v4_gw;
    let _ = run(
        "route",
        &["-n", "delete", "-net", "default", gw, "-ifscope", ifn],
    );
    run(
        "route",
        &["-n", "add", "-net", "default", gw, "-ifscope", ifn],
    )?;
    if let Some((ifn, gw)) = &phys.v6_gw {
        let _ = run(
            "route",
            &[
                "-n", "delete", "-inet6", "-net", "default", gw, "-ifscope", ifn,
            ],
        );
        let _ = run(
            "route",
            &[
                "-n", "add", "-inet6", "-net", "default", gw, "-ifscope", ifn,
            ],
        );
    }
    Ok(())
}

/// Remove the interface-scoped default route(s). Best-effort / idempotent.
fn del_scoped_default(phys: &PhysIface) {
    let (ifn, gw) = &phys.v4_gw;
    let _ = run(
        "route",
        &["-n", "delete", "-net", "default", gw, "-ifscope", ifn],
    );
    if let Some((ifn, gw)) = &phys.v6_gw {
        let _ = run(
            "route",
            &[
                "-n", "delete", "-inet6", "-net", "default", gw, "-ifscope", ifn,
            ],
        );
    }
}

// ---- DNS sentinel (per network service via `networksetup`) ----

/// Enabled network services (e.g. `"Wi-Fi"`, `"Ethernet"`). The first output line
/// is an informational header; `*`-prefixed services are disabled.
fn list_network_services() -> Vec<String> {
    cmd_output("networksetup", &["-listallnetworkservices"])
        .unwrap_or_default()
        .lines()
        .skip(1)
        .filter(|l| !l.starts_with('*'))
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// Point every service's DNS at the sentinel resolvers, returning the prior config
/// per service (empty vec = was DHCP/unset) so [`restore_dns`] can revert it.
fn set_sentinel_dns() -> Vec<(String, Vec<String>)> {
    let mut backup = Vec::new();
    for svc in list_network_services() {
        let prior = cmd_output("networksetup", &["-getdnsservers", &svc]).unwrap_or_default();
        // "There aren't any DNS Servers set on <svc>." (English-only, but the only
        // signal networksetup gives) => DHCP / unset.
        let servers: Vec<String> = if prior.contains("aren't any") {
            Vec::new()
        } else {
            prior
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                // Never record our own sentinels as the user's setting: if a prior
                // manager crash left them in place, capturing them here would make
                // `restore_dns` write them back forever (DNS stuck on the sentinel).
                .filter(|l| l != SENTINEL_DNS_V4 && l != SENTINEL_DNS_V6)
                .collect()
        };
        let _ = run(
            "networksetup",
            &["-setdnsservers", &svc, SENTINEL_DNS_V4, SENTINEL_DNS_V6],
        );
        backup.push((svc, servers));
    }
    backup
}

fn reassert_sentinel_dns() {
    for svc in list_network_services() {
        let _ = run(
            "networksetup",
            &["-setdnsservers", &svc, SENTINEL_DNS_V4, SENTINEL_DNS_V6],
        );
    }
}

/// Restore each service's DNS to its saved value (`Empty` == back to DHCP).
fn restore_dns(backup: &[(String, Vec<String>)]) {
    for (svc, servers) in backup {
        if servers.is_empty() {
            let _ = run("networksetup", &["-setdnsservers", svc, "Empty"]);
        } else {
            let mut args = vec!["-setdnsservers", svc.as_str()];
            args.extend(servers.iter().map(|s| s.as_str()));
            let _ = run("networksetup", &args);
        }
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
    let out = match cmd_output("route", &["-n", "get", "-ifscope", ifn, "1.1.1.1"]) {
        Ok(o) => o,
        Err(_) => return false,
    };
    let mut gw_ok = false;
    let mut if_ok = false;
    for line in out.lines() {
        let l = line.trim();
        if let Some(v) = l.strip_prefix("gateway:") {
            gw_ok = v.trim() == gw;
        } else if let Some(v) = l.strip_prefix("interface:") {
            if_ok = v.trim() == ifn;
        }
    }
    gw_ok && if_ok
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
    // Stop any leftover kill-switch blocking: flush our anchor and restore the
    // default ruleset. We don't force `pfctl -d` (that's the emergency recover
    // script's job) so we don't disturb other PF users; an orphaned `-E` ref just
    // leaves PF enabled with no geph rules.
    let _ = run("pfctl", &["-a", PF_ANCHOR, "-F", "all"]);
    let _ = run("pfctl", &["-f", "/etc/pf.conf"]);
    for net in V4_SPLIT {
        let _ = run("route", &["-n", "delete", "-net", net]);
    }
    for net in V6_SPLIT {
        let _ = run("route", &["-n", "delete", "-inet6", "-net", net]);
    }
    // Reset any service still pointed at our sentinel resolvers back to DHCP.
    for svc in list_network_services() {
        let cur = cmd_output("networksetup", &["-getdnsservers", &svc]).unwrap_or_default();
        if cur.contains(SENTINEL_DNS_V4) || cur.contains(SENTINEL_DNS_V6) {
            let _ = run("networksetup", &["-setdnsservers", &svc, "Empty"]);
        }
    }
}

// ---- PF kill switch ----

/// Compose the PF main ruleset: the system's `com.apple` anchor points (so we
/// don't disable system PF features) plus our inline fail-closed `geph` anchor.
/// The anchor blocks all egress on the physical interface except the engine's own
/// (matched by `uid`), the tunnel, loopback, and DHCP — plus private/link-local
/// ranges when `allow_lan` — and blocks stray DNS to plug leaks.
fn pf_ruleset(utun: &str, uid: u32, allow_lan: bool) -> String {
    let lan = if allow_lan {
        format!(
            "  pass out quick from any to {{ 10.0.0.0/8 172.16.0.0/12 \
             192.168.0.0/16 169.254.0.0/16 224.0.0.0/4 fc00::/7 fe80::/10 ff00::/8 }} keep state\n"
        )
    } else {
        String::new()
    };
    format!(
        r#"scrub-anchor "com.apple/*"
nat-anchor "com.apple/*"
rdr-anchor "com.apple/*"
dummynet-anchor "com.apple/*"
anchor "com.apple/*"
load anchor "com.apple" from "/etc/pf.anchors/com.apple"
anchor "{PF_ANCHOR}" {{
  pass quick on lo0 all
  pass quick on {utun} all
  pass out quick proto {{ tcp udp icmp }} from any to any user {uid} keep state
  pass out quick proto udp from any port 68 to any port 67
  pass out quick proto udp from any port 546 to any port 547
  block drop quick proto {{ tcp udp }} from any to any port 53
{lan}  block drop out all
}}
"#
    )
}

/// Enable PF (reference-counted) and return the token from `pfctl -E`.
fn pf_enable() -> anyhow::Result<String> {
    let out = Command::new("pfctl")
        .arg("-E")
        .output()
        .context("spawning pfctl -E")?;
    // `pfctl -E` prints "Token : <n>" (on stderr).
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    text.lines()
        .find_map(|l| l.trim().strip_prefix("Token :"))
        .map(|s| s.trim().to_string())
        .context("pfctl -E returned no token")
}

/// Load `ruleset` as the PF main ruleset (piped on stdin).
fn pf_load(ruleset: &str) -> anyhow::Result<()> {
    let mut child = Command::new("pfctl")
        .args(["-f", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning pfctl -f")?;
    child
        .stdin
        .take()
        .context("pfctl stdin")?
        .write_all(ruleset.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!(
            "pfctl -f failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Flush our anchor, restore the system's default ruleset, and release our PF
/// enable reference.
fn pf_teardown(token: &str) {
    let _ = run("pfctl", &["-a", PF_ANCHOR, "-F", "all"]);
    let _ = run("pfctl", &["-f", "/etc/pf.conf"]);
    let _ = run("pfctl", &["-X", token]);
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
    use super::{gateway_is_ip, is_tunnel_iface, parse_default_route, parse_physical_default_v4};

    // Captured verbatim from `netstat -rn` on a live Wi-Fi (en1) host, truncated.
    const NETSTAT_IDLE: &str = "Routing tables

Internet:
Destination        Gateway            Flags               Netif Expire
default            10.100.0.1         UGScg                 en1
10.100/16          link#17            UCS                   en1      !
10.100.0.1/32      link#17            UCS                   en1      !
10.100.0.1         20:5:b6:1:87:b1    UHLWIir               en1   1188

Internet6:
Destination                             Gateway                                 Flags               Netif Expire
default                                 fe80::%utun0                            UGcIg               utun0
";

    // The mid-session shape: our /1 splits and the physical default coexist.
    // (`route get default` would resolve into 0.0.0.0/1 → utun here, which is
    // exactly why discovery must read the table instead.)
    const NETSTAT_VPN_UP: &str = "Routing tables

Internet:
Destination        Gateway            Flags               Netif Expire
0/1                utun4              USc                 utun4
default            10.100.0.1         UGScg                 en1
10.100/16          link#17            UCS                   en1      !
128.0/1            utun4              USc                 utun4

Internet6:
Destination                             Gateway                                 Flags               Netif Expire
default                                 fe80::%utun0                            UGcIg               utun0
";

    // A tunnel that has fully grabbed the v4 default, with the physical entry
    // withdrawn (the configd-behind-kill-switch scenario): no usable physical.
    const NETSTAT_TUNNEL_ONLY: &str = "Routing tables

Internet:
Destination        Gateway            Flags               Netif Expire
default            link#25            UCS                 utun4

Internet6:
Destination                             Gateway                                 Flags               Netif Expire
default                                 fe80::%utun4                            UGcIg               utun4
";

    #[test]
    fn table_parse_finds_physical_default_when_idle() {
        let (ifn, gw) = parse_physical_default_v4(NETSTAT_IDLE).unwrap();
        assert_eq!((ifn.as_str(), gw.as_str()), ("en1", "10.100.0.1"));
    }

    #[test]
    fn table_parse_finds_physical_default_mid_session() {
        let (ifn, gw) = parse_physical_default_v4(NETSTAT_VPN_UP).unwrap();
        assert_eq!((ifn.as_str(), gw.as_str()), ("en1", "10.100.0.1"));
    }

    #[test]
    fn table_parse_rejects_tunnel_only_default() {
        // Must NOT return utun4 (and must not mistake the v6 section's default
        // for a v4 one): this is the "no usable physical default" case that
        // callers answer with the last-known-good interface.
        assert!(parse_physical_default_v4(NETSTAT_TUNNEL_ONLY).is_err());
    }

    #[test]
    fn table_parse_handles_empty_output() {
        assert!(parse_physical_default_v4("").is_err());
    }

    // Captured verbatim from `route -n get default` on a live Wi-Fi (en1) host.
    const REAL_V4: &str = "   route to: default
destination: default
       mask: default
    gateway: 10.100.0.1
  interface: en1
      flags: <UP,GATEWAY,DONE,STATIC,PRCLONING,GLOBAL>
 recvpipe  sendpipe  ssthresh  rtt,msec    rttvar  hopcount      mtu     expire
       0         0         0         0         0         0      1500         0";

    #[test]
    fn parses_real_physical_default() {
        let (ifn, gw) = parse_default_route(REAL_V4).unwrap();
        assert_eq!(ifn, "en1");
        assert_eq!(gw, "10.100.0.1");
    }

    #[test]
    fn empty_output_is_clean_error() {
        // `route -n get default` prints "not in table" to stderr and exits 0
        // when there is no default route, so parse sees empty stdout. This MUST
        // be an error, not a panic — it's the "no interface line" case that a
        // boot-time / mid-churn absent default route hits.
        assert!(parse_default_route("").is_err());
        assert!(parse_default_route("   route to: default\n").is_err());
    }

    #[test]
    fn rejects_link_gateway_that_broke_route_add() {
        // The exact shape that produced `route ... add default index: 25 utun4`.
        let out = "   route to: default\n    gateway: index: 25 utun4\n  interface: utun4\n";
        assert!(parse_default_route(out).is_err());
    }

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
    fn gateway_ip_validation() {
        assert!(gateway_is_ip("10.100.0.1"));
        assert!(gateway_is_ip("192.168.1.1"));
        assert!(gateway_is_ip("fe80::1%en0")); // v6 link-local with zone
        assert!(!gateway_is_ip("index: 25 utun4"));
        assert!(!gateway_is_ip("link#19"));
        assert!(!gateway_is_ip(""));
    }
}
