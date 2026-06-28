//! `geph5 register-daemon` / `unregister-daemon`: install the privileged
//! supervisor (`geph5 daemon`) to run in the background across logins and
//! reboots, independent of any GUI front-end.
//!
//! Linux uses a systemd unit; Windows uses a boot-time scheduled task running as
//! LocalSystem. Other platforms bail clearly.

#[cfg(target_os = "linux")]
mod imp {
    use std::path::Path;
    use std::process::Command;

    use anyhow::Context;

    const UNIT_NAME: &str = "geph-daemon.service";
    const UNIT_PATH: &str = "/etc/systemd/system/geph-daemon.service";

    pub fn register() -> anyhow::Result<()> {
        ensure_systemd()?;

        // Point the unit at wherever this binary currently lives. The packaging
        // /installer owns where that is; re-run register-daemon if it moves.
        let exe = std::env::current_exe()
            .context("could not determine the path to the geph5 binary")?
            .canonicalize()
            .context("could not resolve the geph5 binary path")?;
        let exe = exe
            .to_str()
            .context("the geph5 binary path is not valid UTF-8")?;

        std::fs::write(UNIT_PATH, unit_file(exe))
            .with_context(|| format!("could not write {UNIT_PATH} (are you root?)"))?;

        systemctl(&["daemon-reload"])?;
        systemctl(&["enable", UNIT_NAME])?;
        // `restart` starts a stopped unit and reloads a changed one, so a
        // re-register is idempotent: it always lands the latest unit and leaves
        // the daemon running.
        systemctl(&["restart", UNIT_NAME])?;

        println!("Registered and started {UNIT_NAME}.");
        Ok(())
    }

    pub fn unregister() -> anyhow::Result<()> {
        ensure_systemd()?;

        // Best-effort: the unit may already be disabled/absent. `disable --now`
        // also stops the running daemon (tearing down the tunnel), which is the
        // intended behavior for uninstall.
        let _ = systemctl(&["disable", "--now", UNIT_NAME]);

        if Path::new(UNIT_PATH).exists() {
            std::fs::remove_file(UNIT_PATH)
                .with_context(|| format!("could not remove {UNIT_PATH}"))?;
        }
        systemctl(&["daemon-reload"])?;

        println!("Unregistered {UNIT_NAME}.");
        Ok(())
    }

    fn unit_file(exe: &str) -> String {
        format!(
            "[Unit]\n\
             Description=Geph5 privileged daemon\n\
             After=network-online.target\n\
             Wants=network-online.target\n\
             \n\
             [Service]\n\
             ExecStart={exe} daemon\n\
             Restart=always\n\
             RestartSec=2\n\
             \n\
             [Install]\n\
             WantedBy=multi-user.target\n"
        )
    }

    fn ensure_systemd() -> anyhow::Result<()> {
        if Path::new("/run/systemd/system").is_dir() {
            Ok(())
        } else {
            anyhow::bail!(
                "systemd does not appear to be running; register-daemon currently \
                 supports systemd only"
            )
        }
    }

    fn systemctl(args: &[&str]) -> anyhow::Result<()> {
        let status = Command::new("systemctl")
            .args(args)
            .status()
            .context("failed to run systemctl")?;
        if !status.success() {
            anyhow::bail!("`systemctl {}` failed with {status}", args.join(" "));
        }
        Ok(())
    }
}

#[cfg(windows)]
mod imp {
    use std::path::Path;
    use std::process::Command;

    use anyhow::Context;

    /// Scheduled-task name (Task Scheduler `/TN`) — the Windows analogue of the
    /// Linux `geph-daemon.service` unit.
    const TASK_NAME: &str = "Geph Daemon";

    pub fn register() -> anyhow::Result<()> {
        // Point the task at wherever this binary currently lives. On Windows
        // `current_exe` is already an absolute, non-verbatim path (no `\\?\`),
        // which Task Scheduler wants. The installer owns where that is; re-run
        // register-daemon if it moves.
        let exe = std::env::current_exe()
            .context("could not determine the path to the geph5 binary")?;
        let exe = exe
            .to_str()
            .context("the geph5 binary path is not valid UTF-8")?;

        // Drive `schtasks` from an XML definition: the bare CLI can't express
        // restart-on-failure (the analogue of systemd `Restart=always`) or an
        // unlimited run time (the daemon runs forever; the default 72h limit
        // would kill it).
        let xml_path = std::env::temp_dir().join("geph-daemon-task.xml");
        write_utf16le(&xml_path, &task_xml(exe))
            .with_context(|| format!("could not write {}", xml_path.display()))?;

        // `/F` overwrites any existing task, so re-register is idempotent.
        let create = schtasks(&[
            "/Create",
            "/TN",
            TASK_NAME,
            "/XML",
            xml_path.to_str().context("temp path is not valid UTF-8")?,
            "/F",
        ]);
        let _ = std::fs::remove_file(&xml_path);
        create?;

        // The boot trigger only fires at startup, so start it now too (mirrors
        // the Linux `systemctl restart`, which leaves the daemon running).
        schtasks(&["/Run", "/TN", TASK_NAME])?;

        println!("Registered and started the \"{TASK_NAME}\" scheduled task.");
        Ok(())
    }

    pub fn unregister() -> anyhow::Result<()> {
        // Best-effort: the task may already be stopped/absent. `/End` stops the
        // running daemon (tearing down the tunnel), the intended uninstall behavior.
        let _ = schtasks(&["/End", "/TN", TASK_NAME]);
        let _ = schtasks(&["/Delete", "/TN", TASK_NAME, "/F"]);
        println!("Unregistered the \"{TASK_NAME}\" scheduled task.");
        Ok(())
    }

    /// Task Scheduler XML: run `<exe> daemon` at boot as LocalSystem (well-known
    /// SID S-1-5-18), elevated, with no time limit and restart-on-failure.
    fn task_xml(exe: &str) -> String {
        let exe = xml_escape(exe);
        format!(
            r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Geph5 privileged daemon</Description>
  </RegistrationInfo>
  <Triggers>
    <BootTrigger>
      <Enabled>true</Enabled>
    </BootTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>S-1-5-18</UserId>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>999</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{exe}</Command>
      <Arguments>daemon</Arguments>
    </Exec>
  </Actions>
</Task>
"#
        )
    }

    fn xml_escape(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&apos;")
    }

    /// `schtasks /XML` wants a UTF-16LE file matching the `encoding="UTF-16"`
    /// declaration; write a BOM followed by UTF-16LE code units.
    fn write_utf16le(path: &Path, s: &str) -> std::io::Result<()> {
        let mut bytes = Vec::with_capacity(2 + s.len() * 2);
        bytes.extend_from_slice(&[0xFF, 0xFE]); // UTF-16LE BOM
        for unit in s.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        std::fs::write(path, bytes)
    }

    fn schtasks(args: &[&str]) -> anyhow::Result<()> {
        let out = Command::new("schtasks")
            .args(args)
            .output()
            .context("failed to run schtasks")?;
        if !out.status.success() {
            anyhow::bail!(
                "`schtasks {}` failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
mod imp {
    pub fn register() -> anyhow::Result<()> {
        anyhow::bail!("register-daemon is not supported on this platform")
    }

    pub fn unregister() -> anyhow::Result<()> {
        anyhow::bail!("unregister-daemon is not supported on this platform")
    }
}

pub use imp::{register, unregister};
