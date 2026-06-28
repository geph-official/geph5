//! macOS full-tunnel VPN: a `utun` device, split `/1` routes that steer all
//! traffic into it, an interface-scoped default route for loop prevention, a
//! sentinel DNS to avoid LAN-resolver leaks, and a fail-closed PF kill switch.
//!
//! Unlike Linux there is no uid-based policy routing, so the engine's own
//! bridge/exit/broker sockets are kept on the physical NIC by `IP_BOUND_IF`
//! pinning (the daemon hands it the interface indices via
//! [`VpnHandle::bind_indices`] → `GEPH_VPN_BIND_IF4/6`; see geph5-client's
//! `bound_dialer.rs`). Because macOS scoped routing won't fall back to the global
//! default once the `/1` routes capture everything, [`add_scoped_default`] also
//! installs an interface-scoped default via the physical gateway so those pinned
//! sockets have a route out.
//!
//! The engine runs as the dedicated [`SERVICE_USER`] (`_geph`) so the PF kill
//! switch can permit its egress by uid and fail closed on everything else. The tun
//! fd is passed to the child exactly like Linux (`dup2` onto fd 3 in the spawn
//! `pre_exec`, then `--vpn-fd 3`).
//!
//! The physical interface and gateway are captured once at connect (like the
//! Windows implementation); a network change (Wi-Fi ↔ Ethernet) mid-session is not
//! re-evaluated and needs a reconnect.
//!
//! macOS has no `PR_SET_PDEATHSIG`, so a *hard* daemon crash (SIGKILL) leaves the
//! engine child running and still holding the utun fd, keeping the interface and
//! its routes alive with no supervisor. Two things contain that: [`setup`] is
//! idempotent (it clears stale `/1` and scoped-default routes and never trusts a
//! leftover sentinel as the user's DNS), and `recover-geph.sh` is the manual
//! escape hatch. Clean disconnect is unaffected — the daemon kills the child
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
pub const SERVICE_USER: &str = "_geph";

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
// Defined locally (like vpn_linux's TUNSETIFF) so we don't depend on libc exposing
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
#[derive(Clone)]
pub struct PhysIface {
    if4: u32,
    if6: u32,
    v4_gw: (String, String),
    v6_gw: Option<(String, String)>,
}

/// Owns the utun fd; the device (and its routes) live as long as this handle, so
/// routing survives child restarts. `Drop` restores DNS and removes the routes.
pub struct VpnHandle {
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
    pub fn tun_fd(&self) -> RawFd {
        self.tun.as_raw_fd()
    }

    /// Physical interface indices passed to the engine for `IP_BOUND_IF` pinning.
    pub fn bind_indices(&self) -> (u32, u32) {
        (self.phys.if4, self.phys.if6)
    }
}

impl Drop for VpnHandle {
    fn drop(&mut self) {
        // Lift the kill switch first so teardown traffic isn't blocked.
        if let Some(token) = &self.pf_token {
            pf_teardown(token);
        }
        restore_dns(&self.dns_backup);
        del_scoped_default(&self.phys);
        for net in V4_SPLIT {
            let _ = run("route", &["-n", "delete", "-net", net, "-interface", &self.ifname]);
        }
        for net in V6_SPLIT {
            let _ = run(
                "route",
                &["-n", "delete", "-inet6", "-net", net, "-interface", &self.ifname],
            );
        }
        // These explicit deletes are what actually tear routing down: callers kill
        // the engine child first (see reconcile_tunnel), but during a live session
        // that child holds a dup of the utun fd, so dropping our OwnedFd alone would
        // NOT remove the interface — the routes must be deleted by hand.
    }
}

/// Discover the physical interface(s) behind the current default route(s). Must be
/// called *before* [`setup`] installs the `/1` routes, while the default route
/// still reflects the real NIC.
pub fn physical_iface() -> anyhow::Result<PhysIface> {
    let (ifname4, gw4) = default_route(&["-n", "get", "default"])
        .context("no IPv4 default route — not connected to a network?")?;
    let if4 = iface_index(&ifname4)?;
    // v6 is best-effort: many hosts have no v6 default, in which case the engine
    // runs v4-only. A dual-stack NIC shares one index, so reuse if4 as a fallback.
    let (if6, v6_gw) = match default_route(&["-n", "get", "-inet6", "default"]) {
        Ok((ifname6, gw6)) => (iface_index(&ifname6).unwrap_or(if4), Some((ifname6, gw6))),
        Err(_) => (if4, None),
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
pub fn setup(phys: PhysIface, uid: u32, allow_lan: bool) -> anyhow::Result<VpnHandle> {
    let (tun, ifname) = create_utun().context("creating utun device")?;
    let mut handle = VpnHandle {
        tun,
        ifname,
        phys,
        dns_backup: Vec::new(),
        pf_token: None,
    };
    // On any failure inside bring_up, `handle` drops here and its Drop undoes
    // whatever was applied so far (every teardown step is idempotent/best-effort).
    handle.bring_up(uid, allow_lan)?;
    Ok(handle)
}

impl VpnHandle {
    /// Apply addressing, routes, DNS, and the kill switch. Separated from
    /// construction so a mid-way failure unwinds cleanly through `Drop`.
    fn bring_up(&mut self, uid: u32, allow_lan: bool) -> anyhow::Result<()> {
        let ifname = self.ifname.clone();

        run(
            "ifconfig",
            &[ifname.as_str(), "inet", TUN_V4, TUN_V4, "mtu", TUN_MTU, "up"],
        )
        .context("configuring utun IPv4")?;
        // Best-effort v6 (the host may have no v6 at all). macOS ifconfig uses the
        // `prefixlen` keyword, not the `addr/len` slash form.
        let _ = run(
            "ifconfig",
            &[ifname.as_str(), "inet6", TUN_V6, "prefixlen", TUN_V6_PREFIX, "up"],
        );

        // Delete-then-add so a stale /1 left by a crashed prior instance (pointing
        // at a now-defunct utun) doesn't make the add fail with "already in table".
        for net in V4_SPLIT {
            let _ = run("route", &["-n", "delete", "-net", net]);
            run("route", &["-n", "add", "-net", net, "-interface", &ifname])
                .with_context(|| format!("adding route {net}"))?;
        }
        for net in V6_SPLIT {
            let _ = run("route", &["-n", "delete", "-inet6", "-net", net]);
            let _ = run("route", &["-n", "add", "-inet6", "-net", net, "-interface", &ifname]);
        }

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

        // PF kill switch, last. Record the enable token *before* loading the rules
        // so a load failure still releases our PF reference via Drop.
        self.pf_token = Some(pf_enable().context("enabling PF")?);
        pf_load(&pf_ruleset(&ifname, &self.phys.v4_gw.0, uid, allow_lan))
            .context("loading PF kill switch")?;
        Ok(())
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
    let end = name_buf.iter().position(|&b| b == 0).unwrap_or(name_buf.len());
    let ifname = String::from_utf8_lossy(&name_buf[..end]).into_owned();

    // CLOEXEC so this copy doesn't leak into unrelated children; the child gets a
    // fresh (non-cloexec) dup via the spawn pre_exec dup2. macOS sockets have no
    // atomic SOCK_CLOEXEC, so there is a tiny window between socket() and this
    // fcntl in which a concurrent spawn could inherit the fd — acceptable, as the
    // daemon does not spawn children during VPN setup.
    unsafe {
        libc::fcntl(owned.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
    }

    Ok((owned, ifname))
}

/// Parse the `interface:` and `gateway:` lines from `route get <args>`.
fn default_route(route_args: &[&str]) -> anyhow::Result<(String, String)> {
    let out = cmd_output("route", route_args)?;
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
    Ok((
        ifname.context("`route get default` had no interface line")?,
        gateway.context("`route get default` had no gateway line")?,
    ))
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
    let _ = run("route", &["-n", "delete", "-net", "default", gw, "-ifscope", ifn]);
    run("route", &["-n", "add", "-net", "default", gw, "-ifscope", ifn])?;
    if let Some((ifn, gw)) = &phys.v6_gw {
        let _ = run("route", &["-n", "delete", "-inet6", "-net", "default", gw, "-ifscope", ifn]);
        let _ = run("route", &["-n", "add", "-inet6", "-net", "default", gw, "-ifscope", ifn]);
    }
    Ok(())
}

/// Remove the interface-scoped default route(s). Best-effort / idempotent.
fn del_scoped_default(phys: &PhysIface) {
    let (ifn, gw) = &phys.v4_gw;
    let _ = run("route", &["-n", "delete", "-net", "default", gw, "-ifscope", ifn]);
    if let Some((ifn, gw)) = &phys.v6_gw {
        let _ = run("route", &["-n", "delete", "-inet6", "-net", "default", gw, "-ifscope", ifn]);
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
                // daemon crash left them in place, capturing them here would make
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

/// What the daemon's watchdog should do after [`network_check`].
pub enum VpnAction {
    /// Routing is healthy; nothing to do.
    Healthy,
    /// The scoped route was repaired in place; respawn the engine so it recovers
    /// promptly instead of waiting out its retry backoff.
    Respawn,
    /// The physical interface/gateway changed; tear down and rebuild fully (the
    /// engine needs fresh `IP_BOUND_IF` indices).
    Rebuild,
}

/// Check that the captured routing is still valid — sleep/wake invalidates the
/// interface-scoped route, and a network switch changes the physical interface —
/// repairing the scoped route in place when that's enough.
pub fn network_check(handle: &VpnHandle) -> VpnAction {
    // No usable default route right now (e.g. network momentarily down): don't
    // thrash — wait for it to come back.
    let cur = match physical_iface() {
        Ok(c) => c,
        Err(_) => return VpnAction::Healthy,
    };
    if cur.if4 != handle.phys.if4 || cur.v4_gw != handle.phys.v4_gw {
        return VpnAction::Rebuild;
    }
    if !scoped_default_ok(&handle.phys) {
        let _ = add_scoped_default(&handle.phys);
        return VpnAction::Respawn;
    }
    VpnAction::Healthy
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
pub fn route_change_loop(mut on_change: impl FnMut()) {
    let fd = unsafe { libc::socket(PF_ROUTE, libc::SOCK_RAW, 0) };
    if fd < 0 {
        tracing::warn!("could not open PF_ROUTE socket; relying on periodic VPN checks only");
        return;
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut buf = [0u8; 4096];
    loop {
        let n = unsafe {
            libc::read(fd.as_raw_fd(), buf.as_mut_ptr() as *mut libc::c_void, buf.len())
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

/// Purge stale VPN state left by a crashed/`-9`-killed prior instance (whose `Drop`
/// never ran), so a fresh daemon doesn't start on a half-configured machine.
/// Best-effort / idempotent.
pub fn cleanup_stale() {
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

// ---- dedicated service user (_geph) ----

/// Resolve (creating if absent) the dedicated `_geph` service user, returning
/// `(uid, gid)`. The engine runs as this user so the PF kill switch can permit its
/// egress by uid. Mirrors `vpn_linux::ensure_service_user`.
pub fn ensure_service_user() -> anyhow::Result<(u32, u32)> {
    if let Some(ids) = lookup_user(SERVICE_USER) {
        return Ok(ids);
    }
    let uid = free_uid()?;
    let gid: u32 = 1; // the system "daemon" group; PF matches by uid, so group is immaterial
    let path = format!("/Users/{SERVICE_USER}");
    run("dscl", &[".", "-create", &path])?;
    run("dscl", &[".", "-create", &path, "UserShell", "/usr/bin/false"])?;
    run("dscl", &[".", "-create", &path, "RealName", "Geph VPN service"])?;
    run("dscl", &[".", "-create", &path, "UniqueID", &uid.to_string()])?;
    run("dscl", &[".", "-create", &path, "PrimaryGroupID", &gid.to_string()])?;
    run("dscl", &[".", "-create", &path, "NFSHomeDirectory", "/var/empty"])?;
    run("dscl", &[".", "-create", &path, "Password", "*"])?; // no password login
    let _ = run("dscl", &[".", "-create", &path, "IsHidden", "1"]);
    let _ = run("dscacheutil", &["-flushcache"]);
    lookup_user(SERVICE_USER).context("_geph missing after dscl create")
}

fn lookup_user(name: &str) -> Option<(u32, u32)> {
    let cname = CString::new(name).ok()?;
    // SAFETY: getpwnam with a valid C string; we copy out scalar fields only.
    unsafe {
        let pw = libc::getpwnam(cname.as_ptr());
        if pw.is_null() {
            None
        } else {
            Some(((*pw).pw_uid, (*pw).pw_gid))
        }
    }
}

/// First unused uid in a range suitable for a hidden service account.
fn free_uid() -> anyhow::Result<u32> {
    for uid in 300..700u32 {
        // SAFETY: getpwuid is always safe to call; null => the uid is free.
        if unsafe { libc::getpwuid(uid).is_null() } {
            return Ok(uid);
        }
    }
    bail!("no free uid for the service user");
}

// ---- PF kill switch ----

/// Compose the PF main ruleset: the system's `com.apple` anchor points (so we
/// don't disable system PF features) plus our inline fail-closed `geph` anchor.
/// The anchor blocks all egress on the physical interface except the engine's own
/// (matched by `uid`), the tunnel, loopback, and DHCP — plus private/link-local
/// ranges when `allow_lan` — and blocks stray DNS to plug leaks.
fn pf_ruleset(utun: &str, phys: &str, uid: u32, allow_lan: bool) -> String {
    let lan = if allow_lan {
        format!(
            "  pass out quick on {phys} from any to {{ 10.0.0.0/8 172.16.0.0/12 \
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
  pass out quick on {phys} proto {{ tcp udp icmp }} from any to any user {uid} keep state
  pass out quick on {phys} proto udp from any port 68 to any port 67
  block drop quick on {phys} proto {{ tcp udp }} from any to any port 53
{lan}  block drop out on {phys} all
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
        bail!("pfctl -f failed: {}", String::from_utf8_lossy(&out.stderr).trim());
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
