use std::{future::Future, pin::Pin, sync::Arc};

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

    fn dynamic(self) -> DynListener {
        DynListener::new(self)
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

/// EitherListener is a listener that can switch between two different listeners dynamically.
pub enum EitherListener<L: Listener, R: Listener> {
    Left(L),
    Right(R),
}

#[async_trait]
impl<L: Listener, R: Listener> Listener for EitherListener<L, R> {
    type P = EitherPipe<L::P, R::P>;

    async fn accept(&mut self) -> std::io::Result<Self::P> {
        match self {
            EitherListener::Left(listener) => listener.accept().await.map(EitherPipe::Left),
            EitherListener::Right(listener) => listener.accept().await.map(EitherPipe::Right),
        }
    }
}

type DynListenerFuture = Pin<Box<dyn Send + Future<Output = std::io::Result<Box<dyn Pipe>>>>>;

/// A type-erased `Listener` that always returns a type-erased `Pipe`.
#[derive(Clone)]
pub struct DynListener {
    raw_accept: Arc<futures_util::lock::Mutex<dyn FnMut() -> DynListenerFuture + Send + 'static>>,
}

impl DynListener {
    /// Wrap a concrete `Listener` into a `DynListener`.
    pub fn new<L>(listener: L) -> Self
    where
        L: Listener,
        L::P: 'static,
    {
        let arc_listener = Arc::new(futures_util::lock::Mutex::new(listener));

        let closure = move || {
            let arc_listener = arc_listener.clone();
            Box::pin(async move {
                let mut guard = arc_listener.lock().await;
                let pipe = guard.accept().await?;
                Ok(Box::new(pipe) as Box<dyn Pipe>)
            }) as DynListenerFuture
        };

        Self {
            raw_accept: Arc::new(futures_util::lock::Mutex::new(closure)),
        }
    }
}

#[async_trait]
impl Listener for DynListener {
    type P = Box<dyn Pipe>;

    async fn accept(&mut self) -> std::io::Result<Self::P> {
        let mut closure = self.raw_accept.lock().await;
        closure().await
    }
}
