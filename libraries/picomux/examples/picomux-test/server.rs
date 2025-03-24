use std::{net::SocketAddr, sync::atomic::AtomicU64};

use futures_util::{AsyncReadExt, AsyncWriteExt, TryFutureExt};
use picomux::{PicoMux, Stream};
use rand::RngCore;

use sillad::{listener::Listener, Pipe};

use crate::command::Command;

pub async fn server_main(listen: SocketAddr, sosistab3: Option<String>) -> anyhow::Result<()> {
    if let Some(sosistab3) = sosistab3 {
        let mut listener = sillad_sosistab3::listener::SosistabListener::new(
            sillad::tcp::TcpListener::bind(listen).await?,
            sillad_sosistab3::Cookie::new(&sosistab3),
        );
        loop {
            let wire = listener.accept().await?;
            smolscale::spawn(once_wire(wire).inspect_err(|err| eprintln!("wire died: {:?}", err)))
                .detach();
        }
    } else {
        let mut listener = sillad::tcp::TcpListener::bind(listen).await?;
        loop {
            let wire = listener.accept().await?;
            smolscale::spawn(once_wire(wire).inspect_err(|err| eprintln!("wire died: {:?}", err)))
                .detach();
        }
    }
}

async fn once_wire(wire: impl Pipe) -> anyhow::Result<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let wire_count = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    eprintln!("accepted wire {wire_count} from {:?}", wire.remote_addr());
    // loop {
    //     let mut buf = [0u8; 1024];
    //     wire.read_exact(&mut buf).await?;
    //     eprintln!("gotten 1024 garbages");
    // }

    let (read_wire, write_wire) = wire.split();
    let mux = PicoMux::new(read_wire, write_wire);
    for stream_count in 0u64.. {
        let stream = mux.accept().await?;
        eprintln!("accepted stream {stream_count} from wire {wire_count}");
        smolscale::spawn(
            once_stream(wire_count, stream_count, stream).inspect_err(move |err| {
                eprintln!("stream {wire_count}/{stream_count} died: {:?}", err)
            }),
        )
        .detach();
    }
    unreachable!()
}

async fn once_stream(wire_count: u64, stream_count: u64, mut stream: Stream) -> anyhow::Result<()> {
    let command: Command = serde_json::from_slice(stream.metadata())?;
    eprintln!("{wire_count}/{stream_count} command {:?}", command);
    match command {
        Command::Source(mut len) => {
            while len > 0 {
                let n = len.min(65536);
                let mut buff = vec![0u8; n];
                rand::thread_rng().fill_bytes(&mut buff);
                stream.write_all(&buff).await?;
                len = len.saturating_sub(n);
            }
        }
        Command::Sink(sink) => {
            futures_util::io::copy(stream.take(sink as _), &mut futures_util::io::sink()).await?;
        }
    }
    anyhow::Ok(())
}
