use async_trait::async_trait;

use crate::{EitherPipe, Pipe};

#[async_trait]
/// Listeners accept incoming connections, creating streams with a "remote side". Failures are indicated by the standard I/O error type.
pub trait Listener: Sync + Send + Sized + 'static {
    type P: Pipe;
    async fn accept(&mut self) -> std::io::Result<Self::P>;
}

pub trait ListenerExt: Listener {
    fn join<L: Listener>(self, other: L) -> JoinListener<Self, L> {
        JoinListener(self, other)
    }
}

impl<T: Listener> ListenerExt for T {}

/// JoinListener is a listener that listens to two different listeners.
pub struct JoinListener<L: Listener, R: Listener>(pub L, pub R);

#[async_trait]
impl<L: Listener, R: Listener> Listener for JoinListener<L, R> {
    type P = EitherPipe<L::P, R::P>;
    async fn accept(&mut self) -> std::io::Result<Self::P> {
        futures_lite::future::race(
            async { Ok(EitherPipe::Left(self.0.accept().await?)) },
            async { Ok(EitherPipe::Right(self.1.accept().await?)) },
        )
        .await
    }
}
