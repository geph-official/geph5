//! Linux full-tunnel VPN: a dedicated service user, a tun device, uid-marked
//! policy routing, and an always-on nftables kill switch.
//!
//! There is no address whitelisting. The engine runs as the `geph5-daemon` uid;
//! nftables marks every packet *not* from that uid and an `ip rule` routes marked
//! packets into the tun. The engine's own traffic (bridges/exits + bootstrap DNS,
//! and the in-engine LAN passthrough) is unmarked → physical NIC → no loop. The
//! kill switch drops anything else that would egress a physical interface, so a
//! crash of geph5-client (or even the daemon) fails closed.

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
    const FWMARK: &str = "0x6765";
    const FWMARK_MASK: &str = "0x6765/0xffff";
    const RULE_PRIO: &str = "100";

    // IPv6 policy-routing rule priorities (see `setup_ipv6`). Lower = evaluated
    // first. The engine's own v6 is matched (and made direct/unreachable) before
    // the catch-all that sends everything else into the tun.
    const V6_PRIO_GEPH_DIRECT: &str = "90";
    const V6_PRIO_GEPH_GUARD: &str = "95";
    const V6_PRIO_TUN_ALL: &str = "120";

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
        run("ip", &["addr", "replace", TUN_V4, "dev", TUN_IFACE])?;
        run("ip", &["route", "replace", "default", "dev", TUN_IFACE, "table", RT_TABLE])?;
        run("ip", &["rule", "add", "fwmark", FWMARK_MASK, "table", RT_TABLE, "priority", RULE_PRIO])?;

        // IPv6 needs a different mechanism than v4 — best-effort (see setup_ipv6).
        setup_ipv6(geph_uid);

        firewall_install(geph_uid).context("installing nft kill switch")?;
        Ok(VpnHandle { tun })
    }

    /// Route IPv6 into the tunnel.
    ///
    /// v4 works by letting the host's main table emit the packet (every host has
    /// a v4 default route) and then having the nft marker re-route non-engine
    /// packets into the tun. v6 can't rely on that: hosts frequently have no v6
    /// default route, so an app's `connect()` to a v6 address fails at route
    /// lookup *before* any packet exists to mark — so v6 is silently dead even
    /// though the exit can egress it. So for v6 we route by uid directly: send
    /// every *non-engine* v6 packet into the tun, while keeping the engine's own
    /// v6 on the physical NIC — or unreachable, if the host has no native v6 — so
    /// it can never loop back through the tunnel into itself.
    ///
    /// All best-effort: on failure we leave v6 alone (the kill switch still drops
    /// any non-tunneled v6, so there's no leak), and the catch-all that funnels
    /// everything into the tun is installed only once the engine is safely
    /// excluded above it.
    fn setup_ipv6(geph_uid: u32) {
        let _ = run("ip", &["-6", "addr", "replace", TUN_V6, "dev", TUN_IFACE]);
        let _ = run("ip", &["-6", "route", "replace", "default", "dev", TUN_IFACE, "table", RT_TABLE]);

        let uids = format!("{geph_uid}-{geph_uid}");
        // Engine's own v6: physical NIC if the host has v6, else unreachable.
        let direct = run("ip", &["-6", "rule", "add", "uidrange", &uids, "table", "main", "priority", V6_PRIO_GEPH_DIRECT]).is_ok();
        let guard = run("ip", &["-6", "rule", "add", "uidrange", &uids, "type", "unreachable", "priority", V6_PRIO_GEPH_GUARD]).is_ok();
        // Everything else: into the tun.
        if direct && guard {
            let _ = run("ip", &["-6", "rule", "add", "table", RT_TABLE, "priority", V6_PRIO_TUN_ALL]);
        }
    }

    /// Remove all VPN routing/firewall state. Error-tolerant / idempotent.
    pub fn teardown() {
        firewall_remove();
        let _ = run("ip", &["rule", "del", "fwmark", FWMARK_MASK, "table", RT_TABLE, "priority", RULE_PRIO]);
        // v6 policy rules by priority (plus the legacy v6 fwmark rule, for upgrades).
        let _ = run("ip", &["-6", "rule", "del", "fwmark", FWMARK_MASK, "table", RT_TABLE, "priority", RULE_PRIO]);
        for prio in [V6_PRIO_GEPH_DIRECT, V6_PRIO_GEPH_GUARD, V6_PRIO_TUN_ALL] {
            let _ = run("ip", &["-6", "rule", "del", "priority", prio]);
        }
        let _ = run("ip", &["route", "flush", "table", RT_TABLE]);
        let _ = run("ip", &["-6", "route", "flush", "table", RT_TABLE]);
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
        // Two chains, using `meta skuid` (owning socket's uid for locally-generated
        // packets):
        //   - marker (output route hook): mark everything NOT from geph; the mark
        //     triggers a re-route into the tun table. This must be at `output`,
        //     where skuid is available and the route lookup can be redone.
        //   - killswitch (POSTROUTING filter hook): the leak guard. It must be at
        //     postrouting, NOT output: the mark-based re-route is applied only
        //     *after* the output hooks, so at the output filter hook `oifname` is
        //     still the physical NIC and an `oifname "geph-tun"` accept never
        //     matches (everything would be dropped). At postrouting the interface
        //     is final, and skuid is still available for local traffic.
        let ruleset = format!(
            "table inet geph
delete table inet geph
table inet geph {{
\tchain marker {{
\t\ttype route hook output priority mangle; policy accept;
\t\tmeta skuid != {uid} meta mark set {mark}
\t}}
\tchain killswitch {{
\t\ttype filter hook postrouting priority filter; policy accept;
\t\toifname \"lo\" accept
\t\toifname \"{iface}\" accept
\t\tmeta skuid {uid} accept
\t\tct state established,related accept
\t\tcounter drop
\t}}
}}
",
            uid = geph_uid,
            mark = FWMARK,
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
