use std::{
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use futures_lite::FutureExt as _;
use futures_util::AsyncReadExt;
use picomux::{LivenessConfig, PicoMux};
use sillad::dialer::{Dialer, DialerExt};

use crate::command::Command;

pub async fn client_main(connect: SocketAddr, sosistab3: Option<String>) -> anyhow::Result<()> {
    let start = Instant::now();
    let wire = if let Some(sosistab3) = sosistab3 {
        sillad_sosistab3::dialer::SosistabDialer {
            inner: sillad::tcp::TcpDialer { dest_addr: connect },
            cookie: sillad_sosistab3::Cookie::new(&sosistab3),
        }
        .dynamic()
    } else {
        sillad::tcp::TcpDialer { dest_addr: connect }.dynamic()
    }
    .dial()
    .await?;
    eprintln!("wire dialed in {:?}", start.elapsed());

    // loop {
    //     let mut buf = b"aaaaaaaaaaaaaaaaaaaaaaa".to_vec();

    //     wire.write_all(&buf).await?;
    // }

    let (read, write) = wire.split();
    let mut mux = PicoMux::new(read, write);
    mux.set_liveness(LivenessConfig {
        ping_interval: Duration::from_secs(1),
        timeout: Duration::from_secs(1000),
    });
    let mux = Arc::new(mux);

    let start_ping = ping_once(mux.clone()).await?;
    eprintln!("unloaded ping: {:?}", start_ping);
    loop {
        let ping_loop = async {
            loop {
                smol::Timer::after(Duration::from_secs(3)).await;
                let ping = ping_once(mux.clone()).await?;
                eprintln!(
                    "loaded ping: {:?}; bloat {:?}",
                    ping,
                    ping.saturating_sub(start_ping)
                );
            }
        };
        ping_loop.race(download_chunk(mux.clone())).await?;
    }
}

async fn ping_once(mux: Arc<PicoMux>) -> anyhow::Result<Duration> {
    let start = Instant::now();
    const COUNT: u32 = 1;
    for _ in 0..COUNT {
        let stream = mux.open(&serde_json::to_vec(&Command::Source(1))?).await?;
        futures_util::io::copy(stream, &mut futures_util::io::sink()).await?;
    }
    Ok(start.elapsed() / COUNT)
}

async fn download_chunk(mux: Arc<PicoMux>) -> anyhow::Result<()> {
    const CHUNK_SIZE: usize = 1024 * 1024 * 1000;
    eprintln!("**** starting chunk download, size {CHUNK_SIZE} ****");
    let start = Instant::now();
    let mut stream = mux
        .open(&serde_json::to_vec(&Command::Source(CHUNK_SIZE))?)
        .await?;
    loop {
        let start = Instant::now();
        let n = futures_util::io::copy(
            (&mut stream).take(10_000_000),
            &mut futures_util::io::sink(),
        )
        .await?;
        if n == 0 {
            break;
        }
        eprintln!(
            "*** current {:.2} Mbps",
            80.0 / start.elapsed().as_secs_f64()
        )
    }
    eprintln!(
        "**** chunk download; total speed {:.2} Mbps ****",
        (CHUNK_SIZE as f64 / start.elapsed().as_secs_f64()) / 1000.0 / 1000.0 * 8.0
    );
    Ok(())
}
