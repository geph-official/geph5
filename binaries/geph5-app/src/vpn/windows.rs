//! Windows full-tunnel VPN backend: a manager-owned WinTUN device, a `/1` split default
//! route into it, a DNS sentinel, a fail-closed kill switch (see [`firewall`]),
//! and a stdio packet pump between the device and the engine child.
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
    io::{Read, Write},
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    process::{ChildStdin, ChildStdout, Command},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
};

use anyhow::{Context, bail};
use wintun::{Adapter, Session, Wintun};

use firewall::{Firewall, WfpKillSwitch};

/// WinTUN adapter alias (also the interface name `netsh` targets).
const TUN_NAME: &str = "Geph";
/// Fixed adapter GUID so we recognise our own device across runs.
const TUN_GUID: u128 = 0x6765_7068_0000_0000_0000_0000_0000_0001;

const TUN_V4_ADDR: &str = "100.64.0.1";
const TUN_V4_MASK: &str = "255.192.0.0"; // /10
const TUN_V6_CIDR: &str = "fd00:6765::1/64";
const TUN_MTU: usize = 16384;

/// Ring-buffer capacity for the WinTUN session (4 MiB; a power of two between
/// wintun's MIN and MAX capacities).
const RING_CAPACITY: u32 = 0x40_0000;

/// The `/1` split that captures all traffic into WinTUN without deleting the
/// physical default route (`redirect-gateway def1`).
const V4_SPLIT: [&str; 2] = ["0.0.0.0/1", "128.0.0.0/1"];
const V6_SPLIT: [&str; 2] = ["::/1", "8000::/1"];

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
        for prefix in V4_SPLIT {
            let _ = run(
                "netsh",
                &[
                    "interface",
                    "ipv4",
                    "delete",
                    "route",
                    &format!("prefix={prefix}"),
                    &format!("interface={TUN_NAME}"),
                ],
            );
        }
        for prefix in V6_SPLIT {
            let _ = run(
                "netsh",
                &[
                    "interface",
                    "ipv6",
                    "delete",
                    "route",
                    &format!("prefix={prefix}"),
                    &format!("interface={TUN_NAME}"),
                ],
            );
        }
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

/// Discover the physical default-route interface(s) and gateway(s) via
/// PowerShell's `Get-NetRoute`. The uniform VPN monitor compares this against
/// the connected route and triggers full in-place reconciliation if it changes.
pub(super) fn physical_iface() -> anyhow::Result<PhysIface> {
    let index4 = ps_u32(
        "(Get-NetRoute -DestinationPrefix '0.0.0.0/0' -ErrorAction SilentlyContinue | \
         Sort-Object RouteMetric | Select-Object -First 1).ifIndex",
    )
    .context("could not find the IPv4 default-route interface")?;
    let index6 = ps_u32(
        "(Get-NetRoute -DestinationPrefix '::/0' -ErrorAction SilentlyContinue | \
         Sort-Object RouteMetric | Select-Object -First 1).ifIndex",
    )
    .unwrap_or(index4);
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

    // Addresses + MTU + DNS sentinel.
    run(
        "netsh",
        &[
            "interface",
            "ipv4",
            "set",
            "address",
            &format!("name={TUN_NAME}"),
            "source=static",
            &format!("address={TUN_V4_ADDR}"),
            &format!("mask={TUN_V4_MASK}"),
        ],
    )
    .context("assign tun IPv4 address")?;
    let _ = run(
        "netsh",
        &[
            "interface",
            "ipv6",
            "add",
            "address",
            &format!("interface={TUN_NAME}"),
            &format!("address={TUN_V6_CIDR}"),
            "store=active",
        ],
    );
    let _ = adapter.set_mtu(TUN_MTU);
    let _ = adapter.set_dns_servers(&sentinel_dns());

    // Kill switch before capture routes. (The engine's own broker/bridge/exit sockets reach the
    // physical NIC per-process via IP_UNICAST_IF + the loopback forwarder, so
    // there are no destination bypass routes to punch in here.)
    let mut firewall = WfpKillSwitch::new();
    firewall.preflight().context("kill switch preflight")?;
    let luid = unsafe { adapter.get_luid().Value };
    firewall
        .install(&geph_app_ids(), luid, allow_lan)
        .context("install kill switch")?;

    ensure_split_routes()?;

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
    run(
        "netsh",
        &[
            "interface",
            "ipv4",
            "set",
            "address",
            &format!("name={TUN_NAME}"),
            "source=static",
            &format!("address={TUN_V4_ADDR}"),
            &format!("mask={TUN_V4_MASK}"),
        ],
    )
    .context("reassert tun IPv4 address")?;
    let _ = handle.adapter.set_mtu(TUN_MTU);
    let _ = handle.adapter.set_dns_servers(&sentinel_dns());
    let luid = unsafe { handle.adapter.get_luid().Value };
    handle
        .firewall
        .replace(&geph_app_ids(), luid, allow_lan)
        .context("reconcile kill switch")?;
    ensure_split_routes()?;
    handle.phys = phys;
    Ok(())
}

fn ensure_split_routes() -> anyhow::Result<()> {
    for prefix in V4_SPLIT {
        // Delete+add converges on one exact route. WFP is already fail-closed.
        let _ = run(
            "netsh",
            &[
                "interface",
                "ipv4",
                "delete",
                "route",
                &format!("prefix={prefix}"),
                &format!("interface={TUN_NAME}"),
            ],
        );
        run(
            "netsh",
            &[
                "interface",
                "ipv4",
                "add",
                "route",
                &format!("prefix={prefix}"),
                &format!("interface={TUN_NAME}"),
                "store=active",
            ],
        )
        .with_context(|| format!("add tun route {prefix}"))?;
    }
    for prefix in V6_SPLIT {
        let _ = run(
            "netsh",
            &[
                "interface",
                "ipv6",
                "delete",
                "route",
                &format!("prefix={prefix}"),
                &format!("interface={TUN_NAME}"),
            ],
        );
        let _ = run(
            "netsh",
            &[
                "interface",
                "ipv6",
                "add",
                "route",
                &format!("prefix={prefix}"),
                &format!("interface={TUN_NAME}"),
                "store=active",
            ],
        );
    }
    Ok(())
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
        Err(_) => firewall::purge_stale(),
    }
}

/// Best-effort removal of leftover state from a previous run.
fn cleanup_stale_with_wintun(wintun: &Wintun) {
    if let Ok(adapter) = Adapter::open(wintun, TUN_NAME) {
        let _ = adapter.set_dns_servers(&[]);
        for prefix in V4_SPLIT {
            let _ = run(
                "netsh",
                &[
                    "interface",
                    "ipv4",
                    "delete",
                    "route",
                    &format!("prefix={prefix}"),
                    &format!("interface={TUN_NAME}"),
                ],
            );
        }
        for prefix in V6_SPLIT {
            let _ = run(
                "netsh",
                &[
                    "interface",
                    "ipv6",
                    "delete",
                    "route",
                    &format!("prefix={prefix}"),
                    &format!("interface={TUN_NAME}"),
                ],
            );
        }
    }
    // Delete any leftover kill-switch sublayer (and its filters) by GUID, in case
    // a previous manager used a non-dynamic session or otherwise didn't clean up.
    firewall::purge_stale();
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

/// Run `powershell -Command <expr>` and return trimmed stdout, if it succeeded
/// and was non-empty.
fn powershell(expr: &str) -> Option<String> {
    let out = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", expr])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

fn ps_u32(expr: &str) -> Option<u32> {
    powershell(expr)?.parse().ok()
}

fn run(cmd: &str, args: &[&str]) -> anyhow::Result<()> {
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
    Ok(())
}
