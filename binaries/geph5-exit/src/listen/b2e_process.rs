use std::io::ErrorKind;

use async_trait::async_trait;
use futures_util::TryFutureExt;
use geph5_misc_rpc::bridge::{DEFAULT_TLS_SNI, ObfsProtocol};
use geph5_rt::Task;
use sillad::listener::{DynListener, ListenerExt};
use sillad_conntest::ConnTestListener;
use sillad_hex::HexListener;
use sillad_meeklike::MeeklikeListener;
use sillad_sosistab3::{Cookie, listener::SosistabListener};
use tachyonix::{Receiver, Sender};

use super::{handle_client, tls::tls_config};

/// The protocol cache owns the listener task and cancels it on eviction.
pub(super) struct B2eListener {
    sender: Sender<picomux::Stream>,
    _task: Task<anyhow::Result<()>>,
}

impl B2eListener {
    pub(super) fn new(protocol: ObfsProtocol) -> Self {
        let (sender, recv) = tachyonix::channel(1);
        let task = geph5_rt::spawn(b2e_process(protocol, recv));
        Self {
            sender,
            _task: task,
        }
    }

    // Forwarders retain only the sender, so pending sends cannot keep an
    // evicted listener task alive.
    pub(super) fn sender(&self) -> Sender<picomux::Stream> {
        self.sender.clone()
    }
}

async fn b2e_process(
    protocol: ObfsProtocol,
    recv: Receiver<picomux::Stream>,
) -> anyhow::Result<()> {
    tracing::debug!("b2e_process called with {:?}", protocol);
    let listener = ReceiverListener(recv);
    b2e_inner(create_listener(protocol, listener)?).await?;
    Ok(())
}

fn create_listener(
    protocol: ObfsProtocol,
    bottom: ReceiverListener,
) -> anyhow::Result<DynListener> {
    Ok(match protocol {
        ObfsProtocol::Sosistab3(cookie) => {
            SosistabListener::new(bottom, Cookie::new(&cookie)).dynamic()
        }
        ObfsProtocol::ConnTest(obfs_protocol) => {
            let inner = create_listener(*obfs_protocol, bottom)?;
            ConnTestListener::new(inner).dynamic()
        }
        ObfsProtocol::None => bottom.dynamic(),
        ObfsProtocol::Hex(obfs_protocol) => {
            let inner = create_listener(*obfs_protocol, bottom)?;
            HexListener { inner }.dynamic()
        }
        ObfsProtocol::PlainTls(obfs_protocol) => {
            let inner = create_listener(*obfs_protocol, bottom)?;
            sillad_native_tls::TlsListener::new(inner, tls_config(DEFAULT_TLS_SNI)?).dynamic()
        }
        ObfsProtocol::PlainTlsWithSni { sni_domain, lower } => {
            let inner = create_listener(*lower, bottom)?;
            sillad_native_tls::TlsListener::new(inner, tls_config(&sni_domain)?).dynamic()
        }
        ObfsProtocol::Sosistab3New(cookie, obfs_protocol) => {
            let inner = create_listener(*obfs_protocol, bottom)?;
            SosistabListener::new(inner, Cookie::new(&cookie)).dynamic()
        }
        ObfsProtocol::Meeklike(key, cfg, obfs_protocol) => {
            let inner = create_listener(*obfs_protocol, bottom)?;
            MeeklikeListener::new(inner, *blake3::hash(key.as_bytes()).as_bytes(), cfg).dynamic()
        }
    })
}

async fn b2e_inner(mut listener: impl sillad::listener::Listener) -> anyhow::Result<()> {
    loop {
        let client = listener.accept().await?;
        geph5_rt::spawn(
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    use sillad_sosistab3::listener::global_listener_stats;

    #[test]
    fn dropping_listener_owner_cancels_stack_despite_live_sender() {
        geph5_rt::block_on(async {
            let protocols = [
                ObfsProtocol::Sosistab3("eviction-test".into()),
                ObfsProtocol::ConnTest(Box::new(ObfsProtocol::Sosistab3New(
                    "eviction-test".into(),
                    Box::new(ObfsProtocol::None),
                ))),
                ObfsProtocol::ConnTest(Box::new(ObfsProtocol::Sosistab3New(
                    "eviction-test".into(),
                    Box::new(ObfsProtocol::PlainTlsWithSni {
                        sni_domain: DEFAULT_TLS_SNI.into(),
                        lower: Box::new(ObfsProtocol::None),
                    }),
                ))),
            ];

            for protocol in protocols {
                let baseline = global_listener_stats();
                let owner = Arc::new(B2eListener::new(protocol.clone()));
                let send = owner.sender();

                tokio::time::timeout(Duration::from_secs(1), async {
                    while global_listener_stats().live_listeners != baseline.live_listeners + 1 {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("protocol listener did not start");
                assert!(!send.is_closed());

                drop(owner);

                // The extra sender deliberately stays alive throughout this
                // check. Cleanup must use cancellation, not channel closure.
                tokio::time::timeout(Duration::from_secs(1), async {
                    loop {
                        let stats = global_listener_stats();
                        if send.is_closed()
                            && stats.live_listeners == baseline.live_listeners
                            && stats.queue_slots == baseline.queue_slots
                            && stats.queue_payload_bytes == baseline.queue_payload_bytes
                        {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap_or_else(|_| {
                    panic!("listener stack retained after owner drop: {protocol:?}")
                });
            }
        });
    }
}
