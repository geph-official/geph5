use std::sync::Arc;

use anyctx::AnyCtx;
use futures_util::AsyncReadExt as _;
use geph5_misc_rpc::{
    exit::{ClientCryptHello, ClientExitCryptPipe, ClientHello, ExitHello, ExitHelloInner},
    read_prepend_length, write_prepend_length,
};
use picomux::PicoMux;
use sillad::{dialer::Dialer as _, Pipe};
use smol::future::FutureExt as _;

use stdcode::StdcodeSerializeExt;

use super::{Config, CtxField};

pub async fn open_conn(ctx: &AnyCtx<Config>, dest_addr: &str) -> anyhow::Result<picomux::Stream> {
    let (send, recv) = oneshot::channel();
    let elem = (dest_addr.to_string(), send);
    let _ = ctx.get(CONN_REQ_CHAN).0.send(elem).await;
    Ok(recv.await?)
}

type ChanElem = (String, oneshot::Sender<picomux::Stream>);

static CONN_REQ_CHAN: CtxField<(
    smol::channel::Sender<ChanElem>,
    smol::channel::Receiver<ChanElem>,
)> = |_| smol::channel::unbounded();

pub async fn client_inner(ctx: AnyCtx<Config>) -> anyhow::Result<()> {
    let raw_dialer = ctx.init().exit_constraint.dialer().await?;
    let raw_pipe = raw_dialer.dial().await?;
    tracing::debug!("raw dialer done");
    let authed_pipe = client_auth(raw_pipe).await?;
    tracing::debug!("authentication done, starting mux system");
    let (read, write) = authed_pipe.split();
    let mux = Arc::new(PicoMux::new(read, write));

    let (send_stop, mut recv_stop) = tachyonix::channel(1);
    // run a socks5 loop
    async {
        let err: std::io::Error = recv_stop.recv().await?;
        Err(err.into())
    }
    .race(async {
        loop {
            let mux = mux.clone();
            let send_stop = send_stop.clone();
            let ctx = ctx.clone();
            let (remote_addr, send_back) = ctx.get(CONN_REQ_CHAN).1.recv().await?;
            smolscale::spawn(async move {
                tracing::debug!(remote_addr = display(&remote_addr), "connecting to remote");
                let stream = mux.open(remote_addr.as_bytes()).await;
                match stream {
                    Ok(stream) => {
                        let _ = send_back.send(stream);
                    }
                    Err(err) => {
                        let _ = send_stop.try_send(err);
                    }
                }
                anyhow::Ok(())
            })
            .detach();
        }
    })
    .await
}

async fn client_auth(mut pipe: impl Pipe) -> anyhow::Result<impl Pipe> {
    match pipe.shared_secret() {
        Some(_) => todo!(),
        None => {
            tracing::debug!("requiring full authentication");
            let my_esk = x25519_dalek::EphemeralSecret::random_from_rng(rand::thread_rng());
            let client_hello = ClientHello {
                credentials: Default::default(), // no authentication support yet
                crypt_hello: ClientCryptHello::X25519((&my_esk).into()),
            };
            write_prepend_length(&client_hello.stdcode(), &mut pipe).await?;
            let exit_hello: ExitHello =
                stdcode::deserialize(&read_prepend_length(&mut pipe).await?)?;
            match exit_hello.inner {
                ExitHelloInner::Reject(reason) => {
                    anyhow::bail!("exit rejected our authentication attempt: {reason}")
                }
                ExitHelloInner::SharedSecretResponse(_) => anyhow::bail!(
                    "exit sent a shared-secret response to our full authentication request"
                ),
                ExitHelloInner::X25519(their_epk) => {
                    let shared_secret = my_esk.diffie_hellman(&their_epk);
                    let read_key = blake3::derive_key("e2c", shared_secret.as_bytes());
                    let write_key = blake3::derive_key("c2e", shared_secret.as_bytes());
                    Ok(ClientExitCryptPipe::new(pipe, read_key, write_key))
                }
            }
        }
    }
}
