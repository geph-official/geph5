use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    os::windows::process::CommandExt,
};

use geph5_client::Config;

use crate::prefs::PREF_DIR;

use super::Daemon;

pub struct SubprocDaemon;

const CONTROL_PORT: u16 = 8964;

impl Daemon for SubprocDaemon {
    fn start(&self, mut cfg: Config) -> anyhow::Result<()> {
        cfg.control_listen = Some(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
            CONTROL_PORT,
        ));
        let cfg_path = PREF_DIR.join("config.yaml");
        std::fs::write(
            cfg_path.clone(),
            serde_yaml::to_string(&serde_json::to_value(&cfg)?)?,
        )?;
        if cfg.vpn {
            std::thread::spawn(|| {
                runas::Command::new("geph5-client")
                    .arg("-c")
                    .arg(cfg_path)
                    .gui(true)
                    .show(false)
                    .status()
            });
        } else {
            let mut cmd = std::process::Command::new("geph5-client");

            cmd.arg("-c").arg(cfg_path);

            #[cfg(windows)]
            {
                use winapi::um::winbase::CREATE_NO_WINDOW;
                cmd.creation_flags(CREATE_NO_WINDOW);
            }

            cmd.spawn()?;
        }
        Ok(())
    }

    fn stop(&self) -> anyhow::Result<()> {
        smol::future::block_on(async { Ok(self.control_client().stop().await?) })
    }

    fn control_client(&self) -> geph5_client::ControlClient {
        geph5_client::ControlClient::from(nanorpc_sillad::DialerTransport(sillad::tcp::TcpDialer {
            dest_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), CONTROL_PORT),
        }))
    }

    fn check_dead(&self) -> anyhow::Result<()> {
        todo!()
    }
}
