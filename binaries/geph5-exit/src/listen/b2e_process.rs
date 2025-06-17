use std::io::ErrorKind;

use async_trait::async_trait;
use futures_util::TryFutureExt;
use geph5_misc_rpc::bridge::{B2eMetadata, ObfsProtocol};
use sillad::listener::{DynListener, ListenerExt};
use sillad_conntest::ConnTestListener;
use sillad_hex::HexListener;
use sillad_meeklike::MeeklikeListener;
use sillad_sosistab3::{listener::SosistabListener, Cookie};
use tachyonix::Receiver;

use super::{handle_client, tls::dummy_tls_config};

pub async fn b2e_process(
    b2e_metadata: B2eMetadata,
    recv: Receiver<picomux::Stream>,
) -> anyhow::Result<()> {
    tracing::debug!("b2e_process called with {:?}", b2e_metadata);
    let listener = ReceiverListener(recv);
    b2e_inner(create_listener(b2e_metadata.protocol, listener)).await?;
    Ok(())
}

fn create_listener(protocol: ObfsProtocol, bottom: ReceiverListener) -> DynListener {
    match protocol {
        ObfsProtocol::Sosistab3(cookie) => {
            SosistabListener::new(bottom, Cookie::new(&cookie)).dynamic()
        }
        ObfsProtocol::ConnTest(obfs_protocol) => {
            let inner = create_listener(*obfs_protocol, bottom);
            ConnTestListener::new(inner).dynamic()
        }
        ObfsProtocol::None => bottom.dynamic(),
        ObfsProtocol::Hex(obfs_protocol) => {
            let inner = create_listener(*obfs_protocol, bottom);
            HexListener { inner }.dynamic()
        }
        ObfsProtocol::PlainTls(obfs_protocol) => {
            let inner = create_listener(*obfs_protocol, bottom);
            sillad_native_tls::TlsListener::new(inner, dummy_tls_config()).dynamic()
        }
        ObfsProtocol::Sosistab3New(cookie, obfs_protocol) => {
            let inner = create_listener(*obfs_protocol, bottom);
            SosistabListener::new(inner, Cookie::new(&cookie)).dynamic()
        }
        ObfsProtocol::Meeklike(key, obfs_protocol) => {
            let inner = create_listener(*obfs_protocol, bottom);
            MeeklikeListener::new(inner, *blake3::hash(key.as_bytes()).as_bytes()).dynamic()
        }
    }
}

async fn b2e_inner(mut listener: impl sillad::listener::Listener) -> anyhow::Result<()> {
    loop {
        let client = listener.accept().await?;
        smolscale::spawn(
            handle_client(client)
                .map_err(|e| tracing::trace!(err = debug(e), "client stopped through b2e")),
        )
        .detach();
    }
}

struct ReceiverListener(Receiver<picomux::Stream>);

#[async_trait]
impl sillad::listener::Listener for ReceiverListener {
    type P = picomux::Stream;
    async fn accept(&mut self) -> std::io::Result<Self::P> {
        if let Ok(val) = self.0.recv().await {
            Ok(val)
        } else {
            Err(std::io::Error::new(ErrorKind::BrokenPipe, "channel closed"))
        }
    }
}
