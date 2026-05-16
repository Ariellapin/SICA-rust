//! The editable surface. Modify these files, rebuild, restart -> see new
//! behavior live. The IPC plumbing intentionally lives elsewhere.

pub mod counter;
pub mod fib;

pub use counter::Counter;

pub struct BeState {
    pub counter: Counter,
}

impl BeState {
    pub fn new() -> Self {
        Self { counter: Counter::new(0) }
    }
}
