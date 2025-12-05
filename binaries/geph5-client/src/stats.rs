use std::sync::atomic::Ordering;

use anyctx::AnyCtx;

use atomic_float::AtomicF64;
use dashmap::DashMap;

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
