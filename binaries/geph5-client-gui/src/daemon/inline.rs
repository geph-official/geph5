use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Mutex,
};

use geph5_client::{Client, Config};

use super::Daemon;

#[derive(Default)]
pub struct InlineDaemon {
    daemon: Mutex<Option<Client>>,
}

impl Daemon for InlineDaemon {
    fn start(&self, cfg: Config) -> anyhow::Result<()> {
        let client = Client::start(cfg);
        *self.daemon.lock().unwrap() = Some(client);
        Ok(())
    }

    fn stop(&self) -> anyhow::Result<()> {
        self.daemon.lock().unwrap().take();
        Ok(())
    }

    fn control_client(&self) -> geph5_client::ControlClient {
        let daemon = self.daemon.lock().unwrap();
        if let Some(daemon) = daemon.as_ref() {
            daemon.control_client()
        } else {
            geph5_client::ControlClient::from(nanorpc_sillad::DialerTransport(
                sillad::tcp::TcpDialer {
                    dest_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 0),
                },
            ))
        }
    }

    fn check_dead(&self) -> anyhow::Result<()> {
        todo!()
    }
}
