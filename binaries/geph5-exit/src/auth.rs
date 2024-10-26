use std::{sync::LazyLock, thread::available_parallelism};

use geph5_broker_protocol::AccountLevel;
use mizaru2::{ClientToken, UnblindedSignature};
use threadpool::ThreadPool;

static POOL: LazyLock<ThreadPool> = LazyLock::new(|| {
    ThreadPool::with_name(
        "user-verifier".to_string(),
        (available_parallelism().unwrap().get() / 8).max(2),
    )
});

pub async fn verify_user(
    level: AccountLevel,
    token: ClientToken,
    sig: UnblindedSignature,
) -> anyhow::Result<()> {
    if sig.epoch.abs_diff(mizaru2::current_epoch()) > 2 {
        anyhow::bail!("signature from wrong epoch")
    }
    // TODO make this configurable, once we get to all the servers
    let key = match level {
        AccountLevel::Free => mizaru2::PublicKey::from_bytes(
            hex::decode("0558216cbab7a9c46f298f4c26e171add9af87d0694988b8a8fe52ee932aa754")
                .unwrap()
                .try_into()
                .unwrap(),
        ),
        AccountLevel::Plus => mizaru2::PublicKey::from_bytes(
            hex::decode("cf6f58868c6d9459b3a63bc2bd86165631b3e916bad7f62b578cd9614e0bcb3b")
                .unwrap()
                .try_into()
                .unwrap(),
        ),
    };

    let (send, recv) = oneshot::channel();
    POOL.execute(move || {
        let _ = send.send(key.blind_verify(token, &sig));
    });
    recv.await??;
    Ok(())
}
