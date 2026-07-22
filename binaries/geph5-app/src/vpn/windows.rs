//! Windows full-tunnel VPN backend: a manager-owned WinTUN device, a `/1` split default
//! route into it, a DNS sentinel, a fail-closed kill switch (see [`firewall`]),
//! and a stdio packet pump between the device and the engine child.
//!
//! All host network configuration — the tun addresses, the split-default routes,
//! and physical-interface discovery — is done in-process through the IP Helper
//! API (`iphlpapi`) keyed by the WinTUN interface LUID, *not* by shelling out to
//! `netsh`/`powershell`. Each of those is a process spawn costing hundreds of
//! milliseconds, and a server switch reasserts the whole set under the manager
//! lock; doing it via direct syscalls keeps the reconnect gap short (the same
//! near-instant reconfiguration Linux/macOS get from netlink / route sockets).
//!
//! Loop prevention is entirely per-process: the engine binds its own outbound
//! sockets to the physical interface via `IP_UNICAST_IF` (driven by the
//! `GEPH_VPN_BIND_IF4/6` env vars the manager sets — see `geph5-client`'s
//! `bound_dialer`), so its bridge/exit traffic leaves the real NIC. The broker's
//! own HTTP clients (`reqwest`/`aws-sdk`) connect through an in-process loopback
//! forwarder whose upstream is dialed the same way (see `geph5-client`'s
//! `broker::bind_forward`). Loop prevention is therefore strictly per-process: any
//! other app reaching the same (intentionally shared, domain-fronted) broker IP
//! still routes into the tunnel.
//!
//! Unlike Linux (where the engine reads the tun fd directly), the engine has no
//! native Windows tun ingestion, so the manager owns the WinTUN device and pumps
//! packets to `geph5-client --stdio-vpn` over its stdin/stdout.

mod firewall;

use std::{
    ffi::c_void,
    io::{Read, Write},
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    process::{ChildStdin, ChildStdout},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
};

use anyhow::{Context, bail};
use windows_sys::Win32::{
    Foundation::{ERROR_NOT_FOUND, ERROR_OBJECT_ALREADY_EXISTS, NO_ERROR},
    NetworkManagement::IpHelper::{
        CreateIpForwardEntry2, CreateUnicastIpAddressEntry, DeleteIpForwardEntry2, FreeMibTable,
        GetIpForwardTable2, InitializeIpForwardEntry, InitializeUnicastIpAddressEntry,
        MIB_IPFORWARD_ROW2, MIB_IPFORWARD_TABLE2, MIB_UNICASTIPADDRESS_ROW,
    },
    Networking::WinSock::{
        ADDRESS_FAMILY, AF_INET, AF_INET6, IN6_ADDR, IN6_ADDR_0, IN_ADDR, IN_ADDR_0, SOCKADDR_IN,
        SOCKADDR_IN6, SOCKADDR_INET,
    },
};
use wintun::{Adapter, Session, Wintun};

use firewall::{Firewall, WfpKillSwitch};

/// WinTUN adapter alias (also the interface name shown in the OS).
const TUN_NAME: &str = "Geph";
/// Fixed adapter GUID so we recognise our own device across runs.
const TUN_GUID: u128 = 0x6765_7068_0000_0000_0000_0000_0000_0001;

const TUN_V4_ADDR: Ipv4Addr = Ipv4Addr::new(100, 64, 0, 1);
const TUN_V4_PREFIX: u8 = 10; // 255.192.0.0
const TUN_V6_ADDR: Ipv6Addr = Ipv6Addr::new(0xfd00, 0x6765, 0, 0, 0, 0, 0, 1);
const TUN_V6_PREFIX: u8 = 64;
const TUN_MTU: usize = 16384;

/// Ring-buffer capacity for the WinTUN session (4 MiB; a power of two between
/// wintun's MIN and MAX capacities).
const RING_CAPACITY: u32 = 0x40_0000;

/// The `/1` split that captures all traffic into WinTUN without deleting the
/// physical default route (`redirect-gateway def1`). These are `/1`, never `/0`,
/// so physical-interface discovery (which looks only at exact default routes)
/// never mistakes them for the real default route.
const V4_SPLIT: [(Ipv4Addr, u8); 2] = [
    (Ipv4Addr::new(0, 0, 0, 0), 1),
    (Ipv4Addr::new(128, 0, 0, 0), 1),
];
const V6_SPLIT: [(Ipv6Addr, u8); 2] = [
    (Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 0), 1),
    (Ipv6Addr::new(0x8000, 0, 0, 0, 0, 0, 0, 0), 1),
];

fn sentinel_dns() -> [IpAddr; 2] {
    [
        IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)),
        IpAddr::V6(Ipv6Addr::new(0x2606, 0x4700, 0x4700, 0, 0, 0, 0, 0x1111)),
    ]
}

/// The physical default-route interface(s) the engine's own traffic must use
/// (passed to the engine as `GEPH_VPN_BIND_IF4/6` for `IP_UNICAST_IF`).
#[derive(Clone, Debug)]
pub(super) struct PhysIface {
    index4: u32,
    index6: u32,
}

impl PhysIface {
    pub(super) fn bind_indices(&self) -> (u32, u32) {
        (self.index4, self.index6)
    }
}

/// Persistent VPN state, held by the manager across engine-child restarts: the
/// WinTUN adapter (keeps the device + its `/1` routes alive), the kill switch,
/// and enough state to tear routing back down. The packet pump is *not* here —
/// it is re-created per child (see [`Pump`]).
pub(super) struct VpnHandle {
    // `wintun` must outlive `adapter` (the adapter borrows the loaded DLL); keep
    // it alive for the handle's lifetime even though we don't touch it again.
    #[allow(dead_code)]
    wintun: Wintun,
    adapter: Arc<Adapter>,
    phys: PhysIface,
    firewall: WfpKillSwitch,
}

impl VpnHandle {
    /// The physical interface indices the engine child should pin its sockets to
    /// (passed through as `GEPH_VPN_BIND_IF4/6`).
    pub(super) fn bind_indices(&self) -> (u32, u32) {
        (self.phys.index4, self.phys.index6)
    }

    /// The WinTUN interface LUID, the key every IP Helper call below is scoped to.
    fn luid(&self) -> u64 {
        unsafe { self.adapter.get_luid().Value }
    }

    /// Open a fresh WinTUN session on the (persistent) adapter for a newly-spawned
    /// engine child's pump.
    pub(super) fn start_session(&self) -> anyhow::Result<Arc<Session>> {
        Ok(Arc::new(
            self.adapter
                .start_session(RING_CAPACITY)
                .context("start wintun session")?,
        ))
    }

    fn cleanup(&mut self) {
        self.firewall.remove();
        let _ = self.adapter.set_dns_servers(&[]);
        delete_split_routes(self.luid());
    }
}

/// Tear down a live VPN and remove its routes, DNS override, and kill switch.
pub(super) fn cleanup(mut handle: VpnHandle) {
    handle.cleanup();
}

#[derive(Clone)]
pub(super) struct NetworkSnapshot {
    bind_indices: (u32, u32),
}

pub(super) fn network_snapshot(handle: &VpnHandle) -> NetworkSnapshot {
    NetworkSnapshot {
        bind_indices: handle.bind_indices(),
    }
}

pub(super) fn network_check(snapshot: &NetworkSnapshot) -> super::NetworkAction {
    match physical_iface() {
        Ok(current) if current.bind_indices() != snapshot.bind_indices => {
            super::NetworkAction::Reconcile
        }
        _ => super::NetworkAction::Healthy,
    }
}

/// Discover the physical default-route interface(s) via the IP Helper route
/// table. The uniform VPN monitor compares this against the connected route and
/// triggers full in-place reconciliation if it changes.
pub(super) fn physical_iface() -> anyhow::Result<PhysIface> {
    let index4 = default_route_ifindex(AF_INET)
        .context("could not find the IPv4 default-route interface")?;
    // IPv6 may be disabled; fall back to the IPv4 interface index like before.
    let index6 = default_route_ifindex(AF_INET6).unwrap_or(index4);
    Ok(PhysIface { index4, index6 })
}

/// Bring up the WinTUN device, routing, DNS, and the kill switch. Idempotent:
/// stale state from a prior run is cleaned first.
pub(super) fn setup(phys: PhysIface, allow_lan: bool) -> anyhow::Result<VpnHandle> {
    let wintun = unsafe { wintun::load() }.map_err(|e| anyhow::anyhow!("load wintun.dll: {e}"))?;
    cleanup_stale_with_wintun(&wintun);
    let rollback = scopeguard::guard(&wintun, |wintun| cleanup_stale_with_wintun(wintun));

    let adapter = match Adapter::open(&wintun, TUN_NAME) {
        Ok(existing) => existing,
        Err(_) => Adapter::create(&wintun, TUN_NAME, TUN_NAME, Some(TUN_GUID))
            .map_err(|e| anyhow::anyhow!("create wintun adapter: {e}"))?,
    };
    let luid = unsafe { adapter.get_luid().Value };

    // Addresses + MTU + DNS sentinel.
    set_unicast_address(luid, IpAddr::V4(TUN_V4_ADDR), TUN_V4_PREFIX)
        .context("assign tun IPv4 address")?;
    let _ = set_unicast_address(luid, IpAddr::V6(TUN_V6_ADDR), TUN_V6_PREFIX);
    let _ = adapter.set_mtu(TUN_MTU);
    let _ = adapter.set_dns_servers(&sentinel_dns());

    // Kill switch before capture routes. (The engine's own broker/bridge/exit sockets reach the
    // physical NIC per-process via IP_UNICAST_IF + the loopback forwarder, so
    // there are no destination bypass routes to punch in here.)
    let mut firewall = WfpKillSwitch::new();
    firewall.preflight().context("kill switch preflight")?;
    firewall
        .install(&geph_app_ids(), luid, allow_lan)
        .context("install kill switch")?;

    ensure_split_routes(luid)?;

    scopeguard::ScopeGuard::into_inner(rollback);
    Ok(VpnHandle {
        wintun,
        adapter,
        phys,
        firewall,
    })
}

/// Reassert WinTUN, DNS, routes, and the complete WFP policy without replacing
/// the live adapter. WFP is committed first, so route repair remains fail-closed.
pub(super) fn reconcile(
    handle: &mut VpnHandle,
    phys: PhysIface,
    allow_lan: bool,
) -> anyhow::Result<()> {
    let luid = handle.luid();
    set_unicast_address(luid, IpAddr::V4(TUN_V4_ADDR), TUN_V4_PREFIX)
        .context("reassert tun IPv4 address")?;
    let _ = set_unicast_address(luid, IpAddr::V6(TUN_V6_ADDR), TUN_V6_PREFIX);
    let _ = handle.adapter.set_mtu(TUN_MTU);
    let _ = handle.adapter.set_dns_servers(&sentinel_dns());
    handle
        .firewall
        .replace(&geph_app_ids(), luid, allow_lan)
        .context("reconcile kill switch")?;
    ensure_split_routes(luid)?;
    handle.phys = phys;
    Ok(())
}

/// Ensure the `/1` capture routes point our WinTUN interface. Idempotent: an
/// identical route already present is treated as success, so no delete/re-add
/// churn is needed. IPv4 is fatal (the tunnel cannot capture without it); IPv6
/// is best-effort since the host may have IPv6 disabled.
fn ensure_split_routes(luid: u64) -> anyhow::Result<()> {
    for (addr, prefix) in V4_SPLIT {
        add_route(luid, IpAddr::V4(addr), prefix)
            .with_context(|| format!("add tun route {addr}/{prefix}"))?;
    }
    for (addr, prefix) in V6_SPLIT {
        let _ = add_route(luid, IpAddr::V6(addr), prefix);
    }
    Ok(())
}

/// Remove the `/1` capture routes from our WinTUN interface. Best-effort.
fn delete_split_routes(luid: u64) {
    for (addr, prefix) in V4_SPLIT {
        let _ = delete_route(luid, IpAddr::V4(addr), prefix);
    }
    for (addr, prefix) in V6_SPLIT {
        let _ = delete_route(luid, IpAddr::V6(addr), prefix);
    }
}

/// Startup purge of VPN state a prior crashed manager left behind. The kill switch
/// is now non-dynamic (it stays installed across a crash so the machine remains
/// fail-closed), so a manager that restarts *disconnected* must delete it here or
/// the host stays blackholed. Best-effort; safe to call when nothing is stranded.
pub(super) fn cleanup_stale() {
    match unsafe { wintun::load() } {
        Ok(wintun) => cleanup_stale_with_wintun(&wintun),
        // Even if wintun can't load, still remove the (crash-persisted) kill
        // switch so the host isn't left blackholed.
        Err(_) => {
            let _ = firewall::purge_stale();
        }
    }
}

/// Best-effort removal of leftover state from a previous run.
fn cleanup_stale_with_wintun(wintun: &Wintun) {
    if let Ok(adapter) = Adapter::open(wintun, TUN_NAME) {
        let _ = adapter.set_dns_servers(&[]);
        delete_split_routes(unsafe { adapter.get_luid().Value });
    }
    // Delete any leftover kill-switch sublayer (and its filters) by GUID, in case
    // a previous manager used a non-dynamic session or otherwise didn't clean up.
    let _ = firewall::purge_stale();
}

/// Full image paths the kill switch permits to egress the physical NIC: the
/// manager itself and the engine child.
fn geph_app_ids() -> Vec<std::path::PathBuf> {
    let mut ids = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        ids.push(exe);
    }
    // Must be the *same* full path the engine is actually spawned from, or the
    // kill switch's app-id permit won't match and the engine blocks itself.
    ids.push(crate::platform::engine_bin_path());
    ids
}

// ---- IP Helper (iphlpapi) host-network configuration ----
//
// Everything here is keyed by the WinTUN interface LUID and runs in-process, in
// place of the `netsh`/`powershell` subprocesses the reconnect path used to spawn.

/// Fill a `SOCKADDR_INET` in place with `ip` (address family + address; port and
/// scope left zeroed). Writing a union field is safe in Rust; only reads are not.
fn write_sockaddr_inet(dst: &mut SOCKADDR_INET, ip: IpAddr) {
    match ip {
        IpAddr::V4(v4) => {
            let mut sa: SOCKADDR_IN = unsafe { std::mem::zeroed() };
            sa.sin_family = AF_INET;
            // `from_ne_bytes` lays the octets out in memory order, i.e. network
            // byte order, which is exactly what `S_addr` expects.
            sa.sin_addr = IN_ADDR {
                S_un: IN_ADDR_0 {
                    S_addr: u32::from_ne_bytes(v4.octets()),
                },
            };
            dst.Ipv4 = sa;
        }
        IpAddr::V6(v6) => {
            let mut sa: SOCKADDR_IN6 = unsafe { std::mem::zeroed() };
            sa.sin6_family = AF_INET6;
            sa.sin6_addr = IN6_ADDR {
                u: IN6_ADDR_0 { Byte: v6.octets() },
            };
            dst.Ipv6 = sa;
        }
    }
}

/// The unspecified address of `ip`'s family, used as the on-link route next hop.
fn unspecified_like(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
    }
}

/// Assign a unicast address to the interface. Replaces
/// `netsh interface ip set/add address`. An already-present identical address is
/// treated as success so reconciles are idempotent.
fn set_unicast_address(luid: u64, ip: IpAddr, prefix: u8) -> anyhow::Result<()> {
    let mut row: MIB_UNICASTIPADDRESS_ROW = unsafe { std::mem::zeroed() };
    unsafe { InitializeUnicastIpAddressEntry(&mut row) };
    row.InterfaceLuid.Value = luid;
    write_sockaddr_inet(&mut row.Address, ip);
    row.OnLinkPrefixLength = prefix;
    let code = unsafe { CreateUnicastIpAddressEntry(&row) };
    if code == NO_ERROR || code == ERROR_OBJECT_ALREADY_EXISTS {
        Ok(())
    } else {
        bail!("CreateUnicastIpAddressEntry({ip}/{prefix}) failed: 0x{code:08X}");
    }
}

/// Add an on-link route pointing our WinTUN interface. Replaces
/// `netsh interface ip add route`. `ERROR_OBJECT_ALREADY_EXISTS` (the exact route
/// is already present) is success. The `/1` splits are more specific than the
/// physical `/0`, so longest-prefix match captures traffic regardless of metric.
fn add_route(luid: u64, dest: IpAddr, prefix: u8) -> anyhow::Result<()> {
    let mut row: MIB_IPFORWARD_ROW2 = unsafe { std::mem::zeroed() };
    unsafe { InitializeIpForwardEntry(&mut row) };
    row.InterfaceLuid.Value = luid;
    write_sockaddr_inet(&mut row.DestinationPrefix.Prefix, dest);
    row.DestinationPrefix.PrefixLength = prefix;
    write_sockaddr_inet(&mut row.NextHop, unspecified_like(dest));
    let code = unsafe { CreateIpForwardEntry2(&row) };
    if code == NO_ERROR || code == ERROR_OBJECT_ALREADY_EXISTS {
        Ok(())
    } else {
        bail!("CreateIpForwardEntry2({dest}/{prefix}) failed: 0x{code:08X}");
    }
}

/// Delete one of our on-link routes. Replaces `netsh interface ip delete route`.
/// `ERROR_NOT_FOUND` (already gone) is success. The key is
/// (InterfaceLuid, DestinationPrefix, NextHop).
fn delete_route(luid: u64, dest: IpAddr, prefix: u8) -> anyhow::Result<()> {
    let mut row: MIB_IPFORWARD_ROW2 = unsafe { std::mem::zeroed() };
    row.InterfaceLuid.Value = luid;
    write_sockaddr_inet(&mut row.DestinationPrefix.Prefix, dest);
    row.DestinationPrefix.PrefixLength = prefix;
    write_sockaddr_inet(&mut row.NextHop, unspecified_like(dest));
    let code = unsafe { DeleteIpForwardEntry2(&row) };
    if code == NO_ERROR || code == ERROR_NOT_FOUND {
        Ok(())
    } else {
        bail!("DeleteIpForwardEntry2({dest}/{prefix}) failed: 0x{code:08X}");
    }
}

/// Interface index of the lowest-(route-)metric default route for `family`,
/// ignoring loopback. Replaces the `Get-NetRoute` PowerShell query. Our own
/// capture routes are `/1`, so the exact `/0` filter never selects them.
fn default_route_ifindex(family: ADDRESS_FAMILY) -> Option<u32> {
    let mut table: *mut MIB_IPFORWARD_TABLE2 = std::ptr::null_mut();
    let code = unsafe { GetIpForwardTable2(family, &mut table) };
    if code != NO_ERROR || table.is_null() {
        return None;
    }
    let mut best: Option<(u32, u32)> = None; // (route metric, interface index)
    unsafe {
        let count = (*table).NumEntries as usize;
        let rows = std::ptr::addr_of!((*table).Table) as *const MIB_IPFORWARD_ROW2;
        for i in 0..count {
            let row = &*rows.add(i);
            if row.DestinationPrefix.PrefixLength != 0 {
                continue; // not a default route
            }
            if row.DestinationPrefix.Prefix.si_family != family {
                continue;
            }
            if row.Loopback != 0 {
                continue;
            }
            if best.map(|(m, _)| row.Metric < m).unwrap_or(true) {
                best = Some((row.Metric, row.InterfaceIndex));
            }
        }
        FreeMibTable(table as *const c_void);
    }
    best.map(|(_, idx)| idx)
}

/// The stdio packet pump bridging the WinTUN session and one engine child's
/// stdin/stdout (16-bit big-endian length framing, matching the engine's
/// `--stdio-vpn`). Dropping it stops both directions and joins the threads.
pub(super) struct Pump {
    session: Arc<Session>,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl Pump {
    pub(super) fn start(
        session: Arc<Session>,
        child_stdin: ChildStdin,
        child_stdout: ChildStdout,
    ) -> Pump {
        let stop = Arc::new(AtomicBool::new(false));
        let up = {
            let session = session.clone();
            let stop = stop.clone();
            std::thread::spawn(move || pump_up(session, child_stdin, stop))
        };
        let dn = {
            let session = session.clone();
            let stop = stop.clone();
            std::thread::spawn(move || pump_down(session, child_stdout, stop))
        };
        Pump {
            session,
            stop,
            threads: vec![up, dn],
        }
    }
}

impl Drop for Pump {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Unblock the up-thread parked in `receive_blocking`.
        let _ = self.session.shutdown();
        for handle in self.threads.drain(..) {
            let _ = handle.join();
        }
    }
}

/// WinTUN -> engine: read IP packets off the device, length-prefix them, write to
/// the child's stdin.
fn pump_up(session: Arc<Session>, mut child_stdin: ChildStdin, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::SeqCst) {
        let packet = match session.receive_blocking() {
            Ok(p) => p,
            Err(_) => break,
        };
        let bytes = packet.bytes();
        let len = std::cmp::min(bytes.len(), u16::MAX as usize);
        if child_stdin.write_all(&(len as u16).to_be_bytes()).is_err()
            || child_stdin.write_all(&bytes[..len]).is_err()
            || child_stdin.flush().is_err()
        {
            break;
        }
    }
}

/// Engine -> WinTUN: read length-prefixed IP packets off the child's stdout and
/// inject them into the device.
fn pump_down(session: Arc<Session>, mut child_stdout: ChildStdout, stop: Arc<AtomicBool>) {
    let mut len_buf = [0u8; 2];
    while !stop.load(Ordering::SeqCst) {
        if child_stdout.read_exact(&mut len_buf).is_err() {
            break;
        }
        let len = u16::from_be_bytes(len_buf) as usize;
        if len == 0 {
            continue;
        }
        let mut buf = vec![0u8; len];
        if child_stdout.read_exact(&mut buf).is_err() {
            break;
        }
        match session.allocate_send_packet(len as u16) {
            Ok(mut packet) => {
                packet.bytes_mut().copy_from_slice(&buf);
                session.send_packet(packet);
            }
            Err(_) => break,
        }
    }
}
