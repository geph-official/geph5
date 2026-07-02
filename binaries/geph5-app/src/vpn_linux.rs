//! Linux full-tunnel VPN: a dedicated service user, a tun device, uid-range
//! policy routing, and an always-on nftables kill switch.
//!
//! There is no address whitelisting and no packet marking. The engine runs as
//! the `geph5-daemon` uid (named for the geph5-client daemon it runs), and
//! `ip rule uidrange` sends that uid's traffic (bridges/exits + bootstrap DNS,
//! and the in-engine LAN passthrough) out via the main table — physical NIC,
//! no loop — while a lower-priority catch-all rule routes *everything else*
//! into the tun.
//!
//! Routing by "everything except the engine's uid" rather than positively
//! matching non-engine traffic matters for packets that have no owning socket
//! at all: RSTs the kernel emits for closed sockets, TIME_WAIT ACKs, and other
//! kernel-originated responses. A socket-keyed match (nft `meta skuid`, the
//! old fwmark scheme) simply skips those, so they used to escape out the
//! physical NIC (a leak past the kill switch's conntrack exemption) and never
//! reached the in-engine IP stack — which, deaf to the RSTs, kept
//! retransmitting into dead flows every RTO for up to an hour each. With the
//! uid-range scheme they fall through to the catch-all and land in the tun
//! like everything else.
//!
//! The kill switch drops anything else that would egress a physical
//! interface, so a crash of geph5-client (or even the manager) fails closed:
//! if the tun's routing table empties, the catch-all rule no longer matches
//! and traffic falls through to the main table, where nftables drops it.

pub use imp::*;

#[cfg(target_os = "linux")]
mod imp {
    use std::{
        io::Write,
        os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        process::{Command, Stdio},
    };

    use anyhow::{Context, bail};

    /// Dedicated unprivileged user the child geph5-client runs as.
    pub const SERVICE_USER: &str = "geph5-daemon";

    const TUN_IFACE: &str = "geph-tun";
    const TUN_V4: &str = "100.64.0.1/10";
    const TUN_V6: &str = "fd00:6765::1/64";
    const TUN_MTU: &str = "16384";
    const RT_TABLE: &str = "26469";

    // Policy-routing rule priorities, used for both address families. Lower =
    // evaluated first. The engine's own traffic is matched (and made
    // direct-or-unreachable) before the catch-all that sends everything else
    // into the tun.
    const PRIO_GEPH_DIRECT: &str = "90";
    const PRIO_GEPH_GUARD: &str = "95";
    const PRIO_TUN_ALL: &str = "120";

    // From <linux/if_tun.h>: TUNSETIFF = _IOW('T', 202, int). Kept as a plain
    // integer and cast with `as _` at the call site, because `libc::ioctl`'s
    // request argument is `c_ulong` on glibc but `c_int` on musl.
    const TUNSETIFF: u64 = 0x4004_54ca;
    const IFF_TUN: libc::c_short = 0x0001;
    const IFF_NO_PI: libc::c_short = 0x1000;

    /// Owns the tun fd; the device lives as long as this handle, so routing + the
    /// kill switch survive child restarts.
    pub struct VpnHandle {
        tun: OwnedFd,
    }

    impl VpnHandle {
        /// Raw tun fd, to be dup'd into the child.
        pub fn tun_fd(&self) -> RawFd {
            self.tun.as_raw_fd()
        }
    }

    /// Resolve (creating if absent) the dedicated service user. Returns (uid, gid).
    pub fn ensure_service_user() -> anyhow::Result<(u32, u32)> {
        if let Some(ids) = lookup_user(SERVICE_USER) {
            return Ok(ids);
        }
        run(
            "useradd",
            &[
                "--system",
                "--no-create-home",
                "--shell",
                "/usr/sbin/nologin",
                SERVICE_USER,
            ],
        )
        .context("creating geph5-daemon service user")?;
        lookup_user(SERVICE_USER).context("geph5-daemon missing after useradd")
    }

    fn lookup_user(name: &str) -> Option<(u32, u32)> {
        let cname = std::ffi::CString::new(name).ok()?;
        // SAFETY: getpwnam with a valid C string; we copy out the scalar fields.
        unsafe {
            let pw = libc::getpwnam(cname.as_ptr());
            if pw.is_null() {
                None
            } else {
                Some(((*pw).pw_uid, (*pw).pw_gid))
            }
        }
    }

    /// Bring up tun + routing + kill switch for `geph_uid`. Idempotent.
    pub fn setup(geph_uid: u32) -> anyhow::Result<VpnHandle> {
        firewall_preflight()?;
        teardown(); // clean any stale state first

        let tun = create_tun().context("creating tun device")?;

        run("ip", &["link", "set", TUN_IFACE, "up"])?;
        run("ip", &["link", "set", TUN_IFACE, "mtu", TUN_MTU])?;

        // v4 is mandatory: without these rules there is no tunnel at all (the
        // kill switch below would just drop everything).
        setup_rules("-4", geph_uid).context("installing v4 uid policy routing")?;
        // v6 is best-effort: hosts frequently have broken or absent v6, and the
        // kill switch still fails closed for any v6 the rules don't cover.
        let _ = setup_rules("-6", geph_uid);

        firewall_install(geph_uid).context("installing nft kill switch")?;
        Ok(VpnHandle { tun })
    }

    /// Install the uid-range policy routing for one address family:
    ///
    ///   prio 90: engine uid → main table (physical NIC, so the engine's own
    ///            bridge/exit traffic never loops back through the tun)
    ///   prio 95: engine uid → unreachable (guard: if the main table can't
    ///            route it — e.g. no native v6 — fail rather than fall through
    ///            to the tun and loop into ourselves)
    ///   prio 120: everything else → the tun table. This is a *fallthrough*
    ///            catch-all, deliberately not keyed on socket uid or fwmark:
    ///            it also catches kernel-originated socketless packets (RSTs
    ///            for closed sockets, TIME_WAIT ACKs), which must reach the
    ///            in-engine IP stack for it to learn that flows have died. If
    ///            the tun table is empty (engine dead), the lookup falls
    ///            through to main and the nft kill switch drops the traffic.
    ///
    /// The catch-all is installed only after the engine is safely excluded
    /// above it.
    fn setup_rules(family: &str, geph_uid: u32) -> anyhow::Result<()> {
        let uids = format!("{geph_uid}-{geph_uid}");
        let addr = if family == "-6" { TUN_V6 } else { TUN_V4 };
        run("ip", &[family, "addr", "replace", addr, "dev", TUN_IFACE])?;
        run("ip", &[family, "route", "replace", "default", "dev", TUN_IFACE, "table", RT_TABLE])?;

        run("ip", &[family, "rule", "add", "uidrange", &uids, "table", "main", "priority", PRIO_GEPH_DIRECT])?;
        run("ip", &[family, "rule", "add", "uidrange", &uids, "type", "unreachable", "priority", PRIO_GEPH_GUARD])?;
        run("ip", &[family, "rule", "add", "table", RT_TABLE, "priority", PRIO_TUN_ALL])?;
        Ok(())
    }

    /// Remove all VPN routing/firewall state. Error-tolerant / idempotent.
    pub fn teardown() {
        firewall_remove();
        for family in ["-4", "-6"] {
            for prio in [PRIO_GEPH_DIRECT, PRIO_GEPH_GUARD, PRIO_TUN_ALL] {
                let _ = run("ip", &[family, "rule", "del", "priority", prio]);
            }
            let _ = run("ip", &[family, "route", "flush", "table", RT_TABLE]);
        }
        let _ = run("ip", &["link", "del", TUN_IFACE]);
    }

    fn create_tun() -> anyhow::Result<OwnedFd> {
        // O_CLOEXEC so this copy doesn't leak into unrelated children; the child
        // gets a fresh (non-cloexec) dup via pre_exec dup2.
        let fd = unsafe { libc::open(c"/dev/net/tun".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error()).context("open /dev/net/tun");
        }
        let owned = unsafe { OwnedFd::from_raw_fd(fd) };

        #[repr(C)]
        struct IfReq {
            name: [libc::c_char; 16],
            flags: libc::c_short,
            _pad: [u8; 22],
        }
        let mut req = IfReq {
            name: [0; 16],
            flags: IFF_TUN | IFF_NO_PI,
            _pad: [0; 22],
        };
        for (i, b) in TUN_IFACE.bytes().enumerate() {
            req.name[i] = b as libc::c_char;
        }
        let rc = unsafe { libc::ioctl(owned.as_raw_fd(), TUNSETIFF as _, &mut req as *mut IfReq) };
        if rc < 0 {
            return Err(std::io::Error::last_os_error()).context("ioctl(TUNSETIFF)");
        }
        Ok(owned)
    }

    // ---- nftables kill switch (inet family → covers v4 + v6) ----

    fn firewall_preflight() -> anyhow::Result<()> {
        let ok = Command::new("nft")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            bail!("`nft` (nftables) not found — install nftables for VPN mode");
        }
        Ok(())
    }

    fn firewall_install(geph_uid: u32) -> anyhow::Result<()> {
        // A single killswitch chain at the POSTROUTING filter hook: the leak
        // guard behind the uid-range policy routing. Anything leaving on a
        // physical interface that isn't the engine's own traffic (skuid of the
        // geph uid) is a leak and gets dropped — the policy rules route all
        // other traffic (socketless kernel packets included) into the tun, so
        // nothing legitimate can end up here. This also fails closed: if the
        // engine dies and the tun table empties, app traffic falls through to
        // the main table, arrives here, and is dropped.
        //
        // No `ct state` exemption: it used to admit kernel-generated
        // socketless packets (RSTs, TIME_WAIT ACKs) of conntrack-established
        // flows out the physical NIC, which both leaked flow metadata and
        // starved the in-engine IP stack of the RSTs it needs to reap dead
        // flows.
        let ruleset = format!(
            "table inet geph
delete table inet geph
table inet geph {{
\tchain killswitch {{
\t\ttype filter hook postrouting priority filter; policy accept;
\t\toifname \"lo\" accept
\t\toifname \"{iface}\" accept
\t\tmeta skuid {uid} accept
\t\tcounter drop
\t}}
}}
",
            uid = geph_uid,
            iface = TUN_IFACE,
        );
        nft_apply(&ruleset)
    }

    fn firewall_remove() {
        // create-then-delete so this never errors when the table is absent.
        let _ = nft_apply("table inet geph\ndelete table inet geph\n");
    }

    fn nft_apply(ruleset: &str) -> anyhow::Result<()> {
        let mut child = Command::new("nft")
            .arg("-f")
            .arg("-")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("spawning nft")?;
        child
            .stdin
            .take()
            .context("nft stdin")?
            .write_all(ruleset.as_bytes())?;
        let out = child.wait_with_output()?;
        if !out.status.success() {
            bail!("nft failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(())
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
}

#[cfg(not(target_os = "linux"))]
mod imp {
    pub const SERVICE_USER: &str = "geph5-daemon";

    /// Stub handle on non-Linux (full-tunnel VPN is Linux-only for now).
    pub struct VpnHandle;

    impl VpnHandle {
        pub fn tun_fd(&self) -> i32 {
            -1
        }
    }

    pub fn ensure_service_user() -> anyhow::Result<(u32, u32)> {
        anyhow::bail!("full-tunnel VPN mode is only implemented on Linux")
    }

    pub fn setup(_geph_uid: u32) -> anyhow::Result<VpnHandle> {
        anyhow::bail!("full-tunnel VPN mode is only implemented on Linux")
    }

    pub fn teardown() {}
}
