use std::{sync::LazyLock, time::Duration};

use smol::channel::Sender;
use smol_timeout2::TimeoutExt;

static WATCHDOG_KICK: LazyLock<Sender<()>> = LazyLock::new(|| {
    let (send, recv) = smol::channel::unbounded();
    std::thread::Builder::new()
        .name("watchdog".into())
        .spawn(move || {
            smol::future::block_on(async move {
                loop {
                    if recv
                        .recv()
                        .timeout(Duration::from_secs(600))
                        .await
                        .is_none()
                    {
                        panic!("watchdog not kicked for 600 seconds")
                    }
                }
            });
        })
        .unwrap();
    send
});

pub fn kick_watchdog() {
    let _ = WATCHDOG_KICK.try_send(());
}
