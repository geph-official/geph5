//! Uniform VPN controller and the only VPN platform-selection point.

use anyhow::Context as _;

use crate::platform::{ChildTransport, PacketMode};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "linux")]
use linux as backend;
#[cfg(target_os = "macos")]
use macos as backend;
#[cfg(target_os = "windows")]
use windows as backend;

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
compile_error!("geph5-app supports only Linux, macOS, and Windows");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum NetworkAction {
    Healthy,
    Respawn,
    Rebuild,
}

pub(crate) struct NetworkProbe {
    generation: u64,
    snapshot: backend::NetworkSnapshot,
}

pub(crate) struct CheckedNetwork {
    pub generation: u64,
    pub action: NetworkAction,
}

/// All live VPN state. Teardown is explicit; setup rollback is handled inside
/// each backend with scope guards.
pub(crate) struct Vpn {
    handle: Option<backend::VpnHandle>,
    configured_allow_lan: Option<bool>,
    generation: u64,
    #[cfg(target_os = "windows")]
    pump: Option<backend::Pump>,
}

impl Vpn {
    pub(crate) fn new() -> Self {
        Self {
            handle: None,
            configured_allow_lan: None,
            generation: 0,
            #[cfg(target_os = "windows")]
            pump: None,
        }
    }

    pub(crate) fn is_active(&self) -> bool {
        self.handle.is_some()
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn reconcile(
        &mut self,
        want_vpn: bool,
        force_rebuild: bool,
        allow_lan: bool,
        service_user: Option<(u32, u32)>,
    ) -> anyhow::Result<()> {
        #[cfg(target_os = "linux")]
        let platform_options_changed = false;
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        let platform_options_changed =
            self.handle.is_some() && self.configured_allow_lan != Some(allow_lan);
        if !want_vpn || force_rebuild || platform_options_changed {
            self.stop_transport();
            if let Some(handle) = self.handle.take() {
                backend::cleanup(handle);
                self.configured_allow_lan = None;
                self.generation = self.generation.wrapping_add(1);
            }
        }
        if want_vpn && self.handle.is_none() {
            #[cfg(target_os = "linux")]
            let handle = {
                let (uid, _) = service_user.context("Linux VPN requires a service user")?;
                backend::setup(uid)?
            };
            #[cfg(target_os = "macos")]
            let handle = {
                let (uid, _) = service_user.context("macOS VPN requires a service user")?;
                let physical =
                    backend::physical_iface().context("discovering physical interface")?;
                backend::setup(physical, uid, allow_lan)?
            };
            #[cfg(target_os = "windows")]
            let handle = {
                let _ = service_user;
                let physical =
                    backend::physical_iface().context("discovering physical interface")?;
                backend::setup(physical, allow_lan)?
            };
            self.handle = Some(handle);
            self.configured_allow_lan = Some(allow_lan);
            self.generation = self.generation.wrapping_add(1);
        }
        Ok(())
    }

    pub(crate) fn packet_mode(&self) -> PacketMode {
        let Some(handle) = self.handle.as_ref() else {
            return PacketMode::None;
        };
        #[cfg(target_os = "linux")]
        {
            PacketMode::Fd(handle.tun_fd())
        }
        #[cfg(target_os = "macos")]
        {
            PacketMode::Fd(handle.tun_fd())
        }
        #[cfg(target_os = "windows")]
        {
            let _ = handle;
            PacketMode::Stdio
        }
    }

    pub(crate) fn bind_indices(&self) -> Option<(u32, u32)> {
        let handle = self.handle.as_ref()?;
        #[cfg(target_os = "linux")]
        {
            let _ = handle;
            None
        }
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            Some(handle.bind_indices())
        }
    }

    pub(crate) fn attach_transport(&mut self, transport: ChildTransport) -> anyhow::Result<()> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            match transport {
                ChildTransport::None => Ok(()),
                ChildTransport::Stdio { .. } => {
                    anyhow::bail!("unexpected stdio transport for a fd-based VPN")
                }
            }
        }
        #[cfg(target_os = "windows")]
        {
            match (self.handle.as_ref(), transport) {
                (None, ChildTransport::None) => Ok(()),
                (Some(handle), ChildTransport::Stdio { stdin, stdout }) => {
                    let session = handle.start_session()?;
                    self.pump = Some(backend::Pump::start(session, stdin, stdout));
                    Ok(())
                }
                (Some(_), ChildTransport::None) => {
                    anyhow::bail!("Windows VPN engine did not expose its packet transport")
                }
                (None, ChildTransport::Stdio { .. }) => {
                    anyhow::bail!("proxy-mode engine unexpectedly exposed a VPN transport")
                }
            }
        }
    }

    pub(crate) fn stop_transport(&mut self) {
        #[cfg(target_os = "windows")]
        {
            self.pump = None;
        }
    }

    pub(crate) fn network_probe(&self) -> Option<NetworkProbe> {
        self.handle.as_ref().map(|handle| NetworkProbe {
            generation: self.generation,
            snapshot: backend::network_snapshot(handle),
        })
    }

    pub(crate) fn cleanup(&mut self) {
        self.stop_transport();
        if let Some(handle) = self.handle.take() {
            backend::cleanup(handle);
            self.configured_allow_lan = None;
            self.generation = self.generation.wrapping_add(1);
        }
    }
}

pub(crate) fn cleanup_stale() {
    backend::cleanup_stale();
}

pub(crate) fn check_network(probe: NetworkProbe) -> CheckedNetwork {
    CheckedNetwork {
        generation: probe.generation,
        action: backend::network_check(&probe.snapshot),
    }
}

pub(crate) async fn wait_network_change() {
    #[cfg(target_os = "linux")]
    tokio::time::sleep(std::time::Duration::from_secs(60)).await;

    #[cfg(target_os = "windows")]
    tokio::time::sleep(std::time::Duration::from_secs(10)).await;

    #[cfg(target_os = "macos")]
    {
        use std::sync::{Arc, OnceLock};
        use tokio::sync::Notify;

        static ROUTE_CHANGED: OnceLock<Arc<Notify>> = OnceLock::new();
        let route_changed = ROUTE_CHANGED.get_or_init(|| {
            let notify = Arc::new(Notify::new());
            let thread_notify = notify.clone();
            std::thread::spawn(move || {
                backend::route_change_loop(|| thread_notify.notify_one());
            });
            notify
        });
        tokio::select! {
            _ = route_changed.notified() => {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(60)) => {}
        }
    }
}
