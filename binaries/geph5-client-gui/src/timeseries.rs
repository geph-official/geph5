use std::{collections::BTreeMap, sync::RwLock, time::Instant};

pub struct TimeSeries {
    values: RwLock<BTreeMap<Instant, f64>>,
    max_count: usize,
}

impl TimeSeries {
    pub const fn new(max_count: usize) -> Self {
        Self {
            values: RwLock::new(BTreeMap::new()),
            max_count,
        }
    }

    pub fn record(&self, value: f64) {
        let mut values = self.values.write().unwrap();
        values.insert(Instant::now(), value);
        while values.len() > self.max_count {
            values.pop_first();
        }
    }

    pub fn get_at(&self, time: Instant) -> f64 {
        let values = self.values.read().unwrap();
        values
            .range(..=time)
            .next_back()
            .map(|s| s.1)
            .copied()
            .unwrap_or(0.0)
    }
}
