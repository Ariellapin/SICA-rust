use std::sync::atomic::{AtomicI64, Ordering};

pub struct Counter {
    value: AtomicI64,
}

impl Counter {
    pub fn new(initial: i64) -> Self {
        Self { value: AtomicI64::new(initial) }
    }

    pub fn get(&self) -> i64 {
        self.value.load(Ordering::Relaxed)
    }

    pub fn set(&self, v: i64) {
        self.value.store(v, Ordering::Relaxed);
    }

    pub fn add(&self, by: i64) -> i64 {
        self.value.fetch_add(by, Ordering::Relaxed) + by
    }
}
