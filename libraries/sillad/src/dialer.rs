use std::{pin::Pin, sync::Arc};

use crate::{EitherPipe, Pipe};
use async_trait::async_trait;
use futures_lite::{Future, FutureExt};
use smol_timeout2::TimeoutExt;

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

    fn timeout(self, duration: std::time::Duration) -> TimeoutDialer<Self> {
        TimeoutDialer {
            dialer: self,
            timeout: duration,
        }
    }

    fn fallback<B: Dialer>(self, fallback: B) -> FallbackDialer<Self, B> {
        FallbackDialer {
            primary: self,
            fallback,
        }
    }

    fn delay(self, duration: std::time::Duration) -> DelayDialer<Self> {
        DelayDialer {
            dialer: self,
            delay: duration,
        }
    }

    fn dyn_delay(
        self,
        duration: impl Fn() -> std::time::Duration + Send + Sync + 'static,
    ) -> DynDelayDialer<Self> {
        DynDelayDialer {
            dialer: self,
            delay: Box::new(duration),
        }
    }
}

impl<T: Dialer> DialerExt for T {}

/// Because Dialer has an associated type, it is not directly object-safe, so we provide a DynDialer that is a type-erased version of Dialer that always returns type-erased Pipes.
#[derive(Clone)]
#[allow(clippy::type_complexity)]
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
        race_ok(
            async { Ok(EitherPipe::Left(self.0.dial().await?)) },
            async { Ok(EitherPipe::Right(self.1.dial().await?)) },
        )
        .await
    }
}

async fn race_ok<T, E, F1, F2>(f1: F1, f2: F2) -> Result<T, E>
where
    F1: Future<Output = Result<T, E>>,
    F2: Future<Output = Result<T, E>>,
{
    // unfortunately we do need to box here :(
    match futures_util::future::select(Box::pin(f1), Box::pin(f2)).await {
        futures_util::future::Either::Left((Ok(val), _)) => Ok(val),
        futures_util::future::Either::Right((Ok(val), _)) => Ok(val),
        futures_util::future::Either::Left((Err(_), f2)) => f2.await,
        futures_util::future::Either::Right((Err(_), f1)) => f1.await,
    }
}

/// FailingDialer is a dialer that always fails and never returns anything.
pub struct FailingDialer;

#[async_trait]
impl Dialer for FailingDialer {
    type P = Box<dyn Pipe>;

    async fn dial(&self) -> std::io::Result<Self::P> {
        return Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "failing dialer always fails",
        ));
    }
}

pub struct TimeoutDialer<D: Dialer> {
    dialer: D,
    timeout: std::time::Duration,
}

#[async_trait]
impl<D: Dialer> Dialer for TimeoutDialer<D> {
    type P = D::P;

    async fn dial(&self) -> std::io::Result<Self::P> {
        self.dialer
            .dial()
            .timeout(self.timeout)
            .await
            .ok_or(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("dial timed out after {:?}", self.timeout),
            ))?
    }
}

pub struct FallbackDialer<A: Dialer, B: Dialer> {
    primary: A,
    fallback: B,
}

#[async_trait]
impl<A: Dialer, B: Dialer> Dialer for FallbackDialer<A, B> {
    type P = EitherPipe<A::P, B::P>;

    async fn dial(&self) -> std::io::Result<Self::P> {
        match self.primary.dial().await {
            Ok(pipe) => Ok(EitherPipe::Left(pipe)),
            Err(_) => self.fallback.dial().await.map(EitherPipe::Right),
        }
    }
}

pub struct DelayDialer<D: Dialer> {
    dialer: D,
    delay: std::time::Duration,
}

#[async_trait]
impl<D: Dialer> Dialer for DelayDialer<D> {
    type P = D::P;

    async fn dial(&self) -> std::io::Result<Self::P> {
        async_io::Timer::after(self.delay).await;
        self.dialer.dial().await
    }
}

pub struct DynDelayDialer<D: Dialer> {
    dialer: D,
    delay: Box<dyn Fn() -> std::time::Duration + Send + Sync + 'static>,
}

#[async_trait]
impl<D: Dialer> Dialer for DynDelayDialer<D> {
    type P = D::P;

    async fn dial(&self) -> std::io::Result<Self::P> {
        async_io::Timer::after((self.delay)()).await;
        self.dialer.dial().await
    }
}
