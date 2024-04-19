use std::sync::atomic::AtomicU64;

use crate::client::CtxField;

pub static STAT_TOTAL_BYTES: CtxField<AtomicU64> = |_| AtomicU64::new(0);
