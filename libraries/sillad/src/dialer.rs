use std::{pin::Pin, sync::Arc};

use async_trait::async_trait;
use futures_lite::{Future, FutureExt};

use crate::{EitherPipe, Pipe};

#[async_trait]
/// Dialers create pipes by initiating a connection to some sort of "other side". Failures are indicated by the standard I/O error type.
pub trait Dialer: Sync + Send + Sized + 'static {
    type P: Pipe;
    async fn dial(&self) -> std::io::Result<Self::P>;
}

pub trait DialerExt: Dialer {
    fn race<D: Dialer>(self, other: D) -> RaceDialer<Self, D> {
        RaceDialer(self, other)
    }

    fn dynamic(self) -> DynDialer {
        DynDialer::new(self)
    }
}

impl<T: Dialer> DialerExt for T {}

/// Because Dialer has an associated type, it is not directly object-safe, so we provide a DynDialer that is a type-erased version of Dialer that always returns type-erased Pipes.
#[derive(Clone)]
pub struct DynDialer {
    raw_dial: Arc<
        dyn Fn() -> Pin<Box<dyn Send + Future<Output = std::io::Result<Box<dyn Pipe>>>>>
            + Send
            + Sync
            + 'static,
    >,
}

impl DynDialer {
    /// Creates a new DynDialer.
    pub fn new(inner: impl Dialer) -> Self {
        let inner = Arc::new(inner);
        Self {
            raw_dial: Arc::new(move || {
                let inner = inner.clone();
                async move {
                    let b: Box<dyn Pipe> = Box::new(inner.dial().await?);
                    Ok(b)
                }
                .boxed()
            }),
        }
    }
}

#[async_trait]
impl Dialer for DynDialer {
    type P = Box<dyn Pipe>;

    async fn dial(&self) -> std::io::Result<Self::P> {
        Ok(Box::new((self.raw_dial)().await?))
    }
}

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
