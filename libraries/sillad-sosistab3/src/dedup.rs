use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

pub struct Dedup<T> {
    bucket: HashMap<T, Instant>,
    last_size: usize,
    expiry: Duration,
}

impl<T> Dedup<T> {
    pub fn new(expiry: Duration) -> Self {
        Self {
            bucket: HashMap::new(),
            last_size: 1,
            expiry,
        }
    }
}

impl<T: std::hash::Hash + Eq> Dedup<T> {
    pub fn insert(&mut self, key: T) {
        self.bucket.insert(key, Instant::now());
        if self.bucket.len() > self.last_size * 2 {
            self.cleanup();
        }
    }

    pub fn cleanup(&mut self) {
        self.bucket.retain(|_, v| v.elapsed() < self.expiry);
        self.last_size = self.bucket.len();
    }

    pub fn contains(&self, key: &T) -> bool {
        self.bucket.contains_key(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_insert_and_contains() {
        let mut dedup = Dedup::new(Duration::from_secs(5));
        dedup.insert("test_key");
        assert!(dedup.contains(&"test_key"), "The key should be present.");
    }

    #[test]
    fn test_auto_cleanup() {
        let mut dedup = Dedup::new(Duration::from_millis(500));
        for i in 0..1000 {
            dedup.insert(i);
        }

        std::thread::sleep(Duration::from_secs(1));

        dedup.insert(1001);
        dedup.cleanup();

        for i in 0..1000 {
            assert!(
                !dedup.contains(&i),
                "Keys should have expired and been removed from the map."
            );
        }

        assert!(
            dedup.contains(&1001),
            "This key should still be present as it was inserted after the sleep."
        );
        assert!(
            dedup.bucket.len() == 1,
            "Only the last inserted key should remain."
        );
    }
}
