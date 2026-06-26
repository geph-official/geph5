//! The Windows kill switch: a fail-closed leak guard behind a thin trait so the
//! backend can be swapped without touching callers.
//!
//! The production backend is the **Windows Filtering Platform (WFP)**, the only
//! mechanism on Windows that can express the precise filters this design needs
//! *without* mutating the machine's global firewall policy:
//!
//!   - a dynamic WFP engine + a dedicated sublayer (so everything we add is
//!     removed atomically when the handle closes / on `remove`, and — because the
//!     session is dynamic — automatically by the kernel if the daemon dies);
//!   - at `FWPM_LAYER_ALE_AUTH_CONNECT_V4/V6`, in descending filter weight:
//!       * permit loopback;
//!       * permit the geph engine + daemon by app-id (their `IP_UNICAST_IF`-bound
//!         bridge/exit traffic + the broker loopback-forwarder traffic);
//!       * permit on-WinTUN-interface (by LUID) — this is what keeps normal
//!         tunneled traffic flowing, since all of it egresses the tun;
//!       * permit DHCP;
//!       * block `:53` to anything not already permitted above (DNS-leak guard,
//!         weighted *above* the LAN permit so DNS to a LAN resolver is blocked);
//!       * if `allow_lan`, permit RFC1918 / link-local / ULA destinations;
//!       * default block → fail-closed.
//!
//! The daemon holds the engine handle, so the filters persist across engine-child
//! restarts (fail-closed during the gap). A daemon-held *dynamic* session is torn
//! down if the daemon itself dies; a persistent (boot-time) session that survives
//! daemon death is the hardening follow-up.

use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::*;
use windows_sys::Win32::System::Rpc::RPC_C_AUTHN_WINNT;
use windows_sys::core::GUID;

/// Our dedicated sublayer key (distinct from the WinTUN adapter GUID). Everything
/// the kill switch adds lives under this sublayer so it can be reasoned about and
/// purged as a unit.
const SUBLAYER_KEY: GUID = GUID::from_u128(0x6765_7068_0000_0000_0000_0000_0000_0002);

/// `IPPROTO_UDP`, the protocol number used in the DHCP permit condition.
const IPPROTO_UDP: u8 = 17;

/// A fail-closed egress firewall for VPN mode.
pub trait Firewall {
    /// Confirm the backend is usable (e.g. sufficient privilege) before we bring
    /// the tunnel up, so we can fail before creating a leak window.
    fn preflight(&self) -> anyhow::Result<()>;

    /// Install the kill switch. `geph_app_ids` are the full image paths permitted
    /// to egress the physical NIC (the engine child, and the daemon itself).
    /// `wintun_luid` is the WinTUN interface LUID.
    fn install(
        &mut self,
        geph_app_ids: &[std::path::PathBuf],
        wintun_luid: u64,
        allow_lan: bool,
    ) -> anyhow::Result<()>;

    /// Remove the kill switch, restoring normal connectivity. Idempotent.
    fn remove(&mut self);
}

/// The WFP-backed kill switch. See the module docs for the intended filter set.
///
/// The engine handle is stored as a `usize` (not the raw `HANDLE`, which is a
/// `*mut c_void` and thus `!Send`) so this can live in the daemon's
/// `tokio::Mutex`-guarded state across `.await` points. `0` means "not open".
pub struct WfpKillSwitch {
    engine: usize,
}

impl WfpKillSwitch {
    pub fn new() -> Self {
        WfpKillSwitch { engine: 0 }
    }
}

impl Firewall for WfpKillSwitch {
    fn preflight(&self) -> anyhow::Result<()> {
        // WFP requires Administrator; the daemon already enforces elevation at
        // startup (see `main::require_root`), so nothing more is needed here.
        Ok(())
    }

    fn install(
        &mut self,
        geph_app_ids: &[std::path::PathBuf],
        wintun_luid: u64,
        allow_lan: bool,
    ) -> anyhow::Result<()> {
        if self.engine != 0 {
            return Ok(()); // already installed
        }
        // Drop any leftover sublayer from a previous (non-dynamic) run first.
        purge_stale();

        let engine = open_dynamic_engine()?;
        // From here on, any early return must close the engine so we don't leak a
        // half-installed sublayer.
        let result = build_filters(engine, geph_app_ids, wintun_luid, allow_lan);
        match result {
            Ok(()) => {
                self.engine = engine as usize;
                tracing::info!(
                    wintun_luid,
                    allow_lan,
                    app_ids = ?geph_app_ids,
                    "WFP kill switch installed (fail-closed; DNS-leak guard active)"
                );
                Ok(())
            }
            Err(e) => {
                unsafe { FwpmEngineClose0(engine) };
                Err(e)
            }
        }
    }

    fn remove(&mut self) {
        if self.engine != 0 {
            // Closing the dynamic engine handle atomically removes our sublayer
            // and every filter under it.
            unsafe { FwpmEngineClose0(self.engine as HANDLE) };
            self.engine = 0;
            tracing::info!("WFP kill switch removed");
        }
    }
}

impl Drop for WfpKillSwitch {
    fn drop(&mut self) {
        self.remove();
    }
}

/// Best-effort removal of a leftover sublayer (and its filters) from a previous
/// run that did not clean up — e.g. a future build that used a *persistent*
/// session. A dynamic session is already gone once its owner dies, so this is
/// usually a no-op; failures (most commonly "not found") are ignored.
pub fn purge_stale() {
    let engine = match open_dynamic_engine() {
        Ok(e) => e,
        Err(_) => return,
    };
    // Delete the sublayer by key; this also removes filters bound to it. If it
    // was never there, this returns FWP_E_SUBLAYER_NOT_FOUND — ignore.
    unsafe {
        let _ = FwpmSubLayerDeleteByKey0(engine, &SUBLAYER_KEY);
        FwpmEngineClose0(engine);
    }
}

/// Open a dynamic WFP engine session. Dynamic means everything added under it is
/// torn down when the handle is closed or the process exits.
fn open_dynamic_engine() -> anyhow::Result<HANDLE> {
    let mut name = wide("Geph kill switch");
    let mut engine: HANDLE = std::ptr::null_mut();
    let code = unsafe {
        let mut session: FWPM_SESSION0 = std::mem::zeroed();
        session.displayData = FWPM_DISPLAY_DATA0 {
            name: name.as_mut_ptr(),
            description: std::ptr::null_mut(),
        };
        session.flags = FWPM_SESSION_FLAG_DYNAMIC;
        FwpmEngineOpen0(
            std::ptr::null(),
            RPC_C_AUTHN_WINNT,
            std::ptr::null(),
            &session,
            &mut engine,
        )
    };
    check(code, "FwpmEngineOpen0")?;
    Ok(engine)
}

/// Add our sublayer and the full filter set on both the v4 and v6 ALE connect
/// layers. The caller owns `engine` and closes it on error.
fn build_filters(
    engine: HANDLE,
    geph_app_ids: &[std::path::PathBuf],
    wintun_luid: u64,
    allow_lan: bool,
) -> anyhow::Result<()> {
    // Dedicated sublayer with maximum weight so our filters dominate.
    let mut sl_name = wide("Geph kill switch");
    let code = unsafe {
        let mut sublayer: FWPM_SUBLAYER0 = std::mem::zeroed();
        sublayer.subLayerKey = SUBLAYER_KEY;
        sublayer.displayData = FWPM_DISPLAY_DATA0 {
            name: sl_name.as_mut_ptr(),
            description: std::ptr::null_mut(),
        };
        sublayer.weight = 0xFFFF;
        FwpmSubLayerAdd0(engine, &sublayer, std::ptr::null_mut())
    };
    check(code, "FwpmSubLayerAdd0")?;

    // Resolve each geph image path to its WFP app-id blob once, reused on both
    // layers. The blobs must outlive every FwpmFilterAdd0 call that references
    // them, so we free them only after both families are installed.
    let mut app_blobs: Vec<*mut FWP_BYTE_BLOB> = Vec::new();
    for path in geph_app_ids {
        match app_id_blob(path) {
            Ok(blob) => app_blobs.push(blob),
            Err(e) => tracing::warn!(path = %path.display(), err = %e, "skipping app-id (kill switch)"),
        }
    }

    let r = (|| {
        install_family(engine, false, &app_blobs, wintun_luid, allow_lan)?;
        install_family(engine, true, &app_blobs, wintun_luid, allow_lan)?;
        Ok(())
    })();

    for blob in app_blobs {
        unsafe { FwpmFreeMemory0(&mut (blob as *mut core::ffi::c_void)) };
    }
    r
}

// Filter weights (`FWP_UINT8`, higher = evaluated first among terminating
// filters). The DNS-leak block sits *above* the LAN permit on purpose.
const W_LOOPBACK: u8 = 15;
const W_APPID: u8 = 14;
const W_WINTUN: u8 = 13;
const W_DHCP: u8 = 12;
const W_DNS_BLOCK: u8 = 11;
const W_LAN: u8 = 10;
const W_DEFAULT_BLOCK: u8 = 0;

/// Install the whole ladder of filters for one address family.
fn install_family(
    engine: HANDLE,
    v6: bool,
    app_blobs: &[*mut FWP_BYTE_BLOB],
    wintun_luid: u64,
    allow_lan: bool,
) -> anyhow::Result<()> {
    let layer = if v6 {
        FWPM_LAYER_ALE_AUTH_CONNECT_V6
    } else {
        FWPM_LAYER_ALE_AUTH_CONNECT_V4
    };
    // Backing storage for pointer-typed condition values; must outlive every add.
    let mut back = Backing::default();

    // permit loopback (by remote address).
    {
        let c = if v6 {
            vec![cond_v6(&mut back, &FWPM_CONDITION_IP_REMOTE_ADDRESS, V6_LOOPBACK, 128)]
        } else {
            vec![cond_v4(&mut back, &FWPM_CONDITION_IP_REMOTE_ADDRESS, 0x7F00_0000, 0xFF00_0000)]
        };
        add_filter(engine, layer, FWP_ACTION_PERMIT, W_LOOPBACK, &c, "loopback")?;
    }

    // permit each geph app-id (any destination — covers bridge/exit/broker).
    for &blob in app_blobs {
        let c = vec![cond_blob(&FWPM_CONDITION_ALE_APP_ID, blob)];
        add_filter(engine, layer, FWP_ACTION_PERMIT, W_APPID, &c, "geph-app")?;
    }

    // permit everything leaving on the WinTUN interface (normal tunneled traffic).
    {
        let c = vec![cond_u64(&mut back, &FWPM_CONDITION_IP_LOCAL_INTERFACE, wintun_luid)];
        add_filter(engine, layer, FWP_ACTION_PERMIT, W_WINTUN, &c, "on-wintun")?;
    }

    // permit DHCP (v4: UDP→:67; v6: DHCPv6 UDP→:547) so leases can renew.
    {
        let dhcp_port: u16 = if v6 { 547 } else { 67 };
        let c = vec![
            cond_u8(&FWPM_CONDITION_IP_PROTOCOL, IPPROTO_UDP),
            cond_u16(&FWPM_CONDITION_IP_REMOTE_PORT, dhcp_port),
        ];
        add_filter(engine, layer, FWP_ACTION_PERMIT, W_DHCP, &c, "dhcp")?;
    }

    // block :53 to anything not permitted above (DNS-leak guard).
    {
        let c = vec![cond_u16(&FWPM_CONDITION_IP_REMOTE_PORT, 53)];
        add_filter(engine, layer, FWP_ACTION_BLOCK, W_DNS_BLOCK, &c, "dns-leak-guard")?;
    }

    // permit LAN destinations if requested (RFC1918 / link-local / ULA).
    if allow_lan {
        if v6 {
            for (addr, prefix) in [(V6_ULA, 7u8), (V6_LINK_LOCAL, 10u8)] {
                let c = vec![cond_v6(&mut back, &FWPM_CONDITION_IP_REMOTE_ADDRESS, addr, prefix)];
                add_filter(engine, layer, FWP_ACTION_PERMIT, W_LAN, &c, "lan")?;
            }
        } else {
            for (addr, mask) in [
                (0x0A00_0000u32, 0xFF00_0000u32), // 10.0.0.0/8
                (0xAC10_0000, 0xFFF0_0000),       // 172.16.0.0/12
                (0xC0A8_0000, 0xFFFF_0000),       // 192.168.0.0/16
                (0xA9FE_0000, 0xFFFF_0000),       // 169.254.0.0/16 link-local
            ] {
                let c = vec![cond_v4(&mut back, &FWPM_CONDITION_IP_REMOTE_ADDRESS, addr, mask)];
                add_filter(engine, layer, FWP_ACTION_PERMIT, W_LAN, &c, "lan")?;
            }
        }
    }

    // default block → fail-closed (no conditions, lowest weight).
    add_filter(engine, layer, FWP_ACTION_BLOCK, W_DEFAULT_BLOCK, &[], "default-block")?;

    drop(back);
    Ok(())
}

const V6_LOOPBACK: [u8; 16] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
const V6_ULA: [u8; 16] = [0xfc, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]; // fc00::/7
const V6_LINK_LOCAL: [u8; 16] = [0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]; // fe80::/10

/// Stable heap storage for pointer-typed condition values (`FWP_UINT64`,
/// `FWP_V4_ADDR_AND_MASK`, `FWP_V6_ADDR_AND_MASK`). Conditions hold raw pointers
/// into these boxes, so this must outlive the `FwpmFilterAdd0` calls that use them.
#[derive(Default)]
struct Backing {
    u64s: Vec<Box<u64>>,
    v4s: Vec<Box<FWP_V4_ADDR_AND_MASK>>,
    v6s: Vec<Box<FWP_V6_ADDR_AND_MASK>>,
}

impl Backing {
    fn u64_ptr(&mut self, v: u64) -> *mut u64 {
        let b = Box::new(v);
        let p = b.as_ref() as *const u64 as *mut u64;
        self.u64s.push(b);
        p
    }
    fn v4_ptr(&mut self, addr: u32, mask: u32) -> *mut FWP_V4_ADDR_AND_MASK {
        let b = Box::new(FWP_V4_ADDR_AND_MASK { addr, mask });
        let p = b.as_ref() as *const FWP_V4_ADDR_AND_MASK as *mut FWP_V4_ADDR_AND_MASK;
        self.v4s.push(b);
        p
    }
    fn v6_ptr(&mut self, addr: [u8; 16], prefix: u8) -> *mut FWP_V6_ADDR_AND_MASK {
        let b = Box::new(FWP_V6_ADDR_AND_MASK { addr, prefixLength: prefix });
        let p = b.as_ref() as *const FWP_V6_ADDR_AND_MASK as *mut FWP_V6_ADDR_AND_MASK;
        self.v6s.push(b);
        p
    }
}

// ---- condition constructors (matchType EQUAL) ----

fn cond(field: &GUID, ty: FWP_DATA_TYPE, val: FWP_CONDITION_VALUE0_0) -> FWPM_FILTER_CONDITION0 {
    FWPM_FILTER_CONDITION0 {
        fieldKey: *field,
        matchType: FWP_MATCH_EQUAL,
        conditionValue: FWP_CONDITION_VALUE0 {
            r#type: ty,
            Anonymous: val,
        },
    }
}

fn cond_u8(field: &GUID, v: u8) -> FWPM_FILTER_CONDITION0 {
    cond(field, FWP_UINT8, FWP_CONDITION_VALUE0_0 { uint8: v })
}
fn cond_u16(field: &GUID, v: u16) -> FWPM_FILTER_CONDITION0 {
    cond(field, FWP_UINT16, FWP_CONDITION_VALUE0_0 { uint16: v })
}
fn cond_u64(back: &mut Backing, field: &GUID, v: u64) -> FWPM_FILTER_CONDITION0 {
    cond(field, FWP_UINT64, FWP_CONDITION_VALUE0_0 { uint64: back.u64_ptr(v) })
}
fn cond_v4(back: &mut Backing, field: &GUID, addr: u32, mask: u32) -> FWPM_FILTER_CONDITION0 {
    cond(field, FWP_V4_ADDR_MASK, FWP_CONDITION_VALUE0_0 { v4AddrMask: back.v4_ptr(addr, mask) })
}
fn cond_v6(back: &mut Backing, field: &GUID, addr: [u8; 16], prefix: u8) -> FWPM_FILTER_CONDITION0 {
    cond(field, FWP_V6_ADDR_MASK, FWP_CONDITION_VALUE0_0 { v6AddrMask: back.v6_ptr(addr, prefix) })
}
fn cond_blob(field: &GUID, blob: *mut FWP_BYTE_BLOB) -> FWPM_FILTER_CONDITION0 {
    cond(field, FWP_BYTE_BLOB_TYPE, FWP_CONDITION_VALUE0_0 { byteBlob: blob })
}

/// Add a single terminating filter under our sublayer at `layer`.
fn add_filter(
    engine: HANDLE,
    layer: GUID,
    action: FWP_ACTION_TYPE,
    weight: u8,
    conditions: &[FWPM_FILTER_CONDITION0],
    label: &str,
) -> anyhow::Result<()> {
    let mut name = wide(&format!("Geph: {label}"));
    let mut id: u64 = 0;
    let code = unsafe {
        let mut filter: FWPM_FILTER0 = std::mem::zeroed();
        filter.displayData = FWPM_DISPLAY_DATA0 {
            name: name.as_mut_ptr(),
            description: std::ptr::null_mut(),
        };
        filter.layerKey = layer;
        filter.subLayerKey = SUBLAYER_KEY;
        filter.flags = FWPM_FILTER_FLAG_NONE;
        filter.weight = FWP_VALUE0 {
            r#type: FWP_UINT8,
            Anonymous: FWP_VALUE0_0 { uint8: weight },
        };
        filter.action.r#type = action;
        filter.numFilterConditions = conditions.len() as u32;
        filter.filterCondition = if conditions.is_empty() {
            std::ptr::null_mut()
        } else {
            conditions.as_ptr() as *mut FWPM_FILTER_CONDITION0
        };
        FwpmFilterAdd0(engine, &filter, std::ptr::null_mut(), &mut id)
    };
    check(code, "FwpmFilterAdd0").map_err(|e| anyhow::anyhow!("{e} (filter: {label})"))
}

/// Resolve a file path to the WFP app-id blob WFP expects in an `ALE_APP_ID`
/// condition. The returned blob must be freed with `FwpmFreeMemory0`.
fn app_id_blob(path: &Path) -> anyhow::Result<*mut FWP_BYTE_BLOB> {
    let wpath = wide(&path.to_string_lossy());
    let mut blob: *mut FWP_BYTE_BLOB = std::ptr::null_mut();
    let code = unsafe { FwpmGetAppIdFromFileName0(wpath.as_ptr(), &mut blob) };
    check(code, "FwpmGetAppIdFromFileName0")?;
    Ok(blob)
}

/// Map a nonzero WFP/Win32 return code to an error.
fn check(code: u32, what: &str) -> anyhow::Result<()> {
    if code == 0 {
        Ok(())
    } else {
        anyhow::bail!("{what} failed: 0x{code:08X}")
    }
}

/// UTF-16, NUL-terminated, for the wide-string WFP/Win32 APIs.
fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}
