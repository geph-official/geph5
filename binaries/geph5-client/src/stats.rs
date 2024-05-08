use std::sync::atomic::Ordering;

use anyctx::AnyCtx;
use async_trait::async_trait;
use atomic_float::AtomicF64;
use dashmap::DashMap;
use nanorpc::nanorpc_derive;

use smol_str::SmolStr;

use crate::{client::CtxField, Config};

static NUM_STATS: CtxField<DashMap<SmolStr, AtomicF64>> = |_| DashMap::new();

pub fn stat_set_num(ctx: &AnyCtx<Config>, stat: &str, num: f64) {
    ctx.get(NUM_STATS)
        .entry(stat.into())
        .or_default()
        .store(num, Ordering::Relaxed);
}

pub fn stat_incr_num(ctx: &AnyCtx<Config>, stat: &str, num: f64) {
    ctx.get(NUM_STATS)
        .entry(stat.into())
        .or_default()
        .fetch_add(num, Ordering::Relaxed);
}

pub fn stat_get_num(ctx: &AnyCtx<Config>, stat: &str) -> f64 {
    ctx.get(NUM_STATS)
        .get(stat)
        .map(|v| v.load(Ordering::Relaxed))
        .unwrap_or(0.0)
}

pub struct ClientControlImpl(pub AnyCtx<Config>);

#[async_trait]
impl ClientControlProtocol for ClientControlImpl {
    async fn stat_num(&self, stat: SmolStr) -> f64 {
        stat_get_num(&self.0, &stat)
    }
}

/// The RPC protocol exposed by a single client.
#[nanorpc_derive]
#[async_trait]
pub trait ClientControlProtocol {
    /// Get the current statistics.
    async fn stat_num(&self, stat: SmolStr) -> f64;
}
