use async_trait::async_trait;

use crate::{EitherPipe, Pipe};

#[async_trait]
/// Dialers create pipes by initiating a connection to some sort of "other side". Failures are indicated by the standard I/O error type.
pub trait Dialer: Sync + Send + Sized {
    type P: Pipe;
    async fn dial(&self) -> std::io::Result<Self::P>;
}

pub trait DialerExt: Dialer {
    fn race<D: Dialer>(self, other: D) -> RaceDialer<Self, D> {
        RaceDialer(self, other)
    }
}

impl<T: Dialer> DialerExt for T {}

/// RaceDialer is a dialer that races between two dialers.
pub struct RaceDialer<L: Dialer, R: Dialer>(pub L, pub R);

#[async_trait]
impl<L: Dialer, R: Dialer> Dialer for RaceDialer<L, R> {
    type P = EitherPipe<L::P, R::P>;
    async fn dial(&self) -> std::io::Result<Self::P> {
        futures_lite::future::race(
            async { Ok(EitherPipe::Left(self.0.dial().await?)) },
            async { Ok(EitherPipe::Right(self.1.dial().await?)) },
        )
        .await
    }
}
