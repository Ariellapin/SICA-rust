use crossbeam_channel::{unbounded, Receiver, Sender};

#[derive(Debug, Clone)]
pub struct Trigger {
    pub kind:      String,
    pub module:    String,
    pub message:   String,
    pub traceback: Option<String>,
}

#[derive(Clone)]
pub struct TriggerBus {
    pub tx: Sender<Trigger>,
    pub rx: Receiver<Trigger>,
}

impl Default for TriggerBus {
    fn default() -> Self {
        Self::new()
    }
}

impl TriggerBus {
    pub fn new() -> Self {
        let (tx, rx) = unbounded();
        Self { tx, rx }
    }

    pub fn publish(&self, t: Trigger) {
        let _ = self.tx.send(t);
    }

    pub fn subscribe(&self) -> Receiver<Trigger> {
        self.rx.clone()
    }
}
