use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;

pub mod probes;

use crate::config::Limits;

#[derive(Debug, Clone, Copy)]
pub struct ScalingPolicy {
    pub min_cpu_shares: u64,
    pub max_cpu_shares: u64,
    pub high_load_threshold: f64,
    pub low_load_threshold: f64,
    pub poll_interval: Duration,
}

impl ScalingPolicy {
    pub fn from_limits(l: &Limits) -> Self {
        Self {
            min_cpu_shares: l.min_cpu_shares,
            max_cpu_shares: l.max_cpu_shares,
            high_load_threshold: 0.80,
            low_load_threshold: 0.20,
            poll_interval: Duration::from_millis(500),
        }
    }
}

pub struct MonitorState {
    pub cpu_shares: AtomicU64,
    pub shutdown: AtomicBool,
}

impl MonitorState {
    pub fn new(policy: &ScalingPolicy) -> Arc<Self> {
        Arc::new(Self {
            cpu_shares: AtomicU64::new(policy.min_cpu_shares),
            shutdown: AtomicBool::new(false),
        })
    }
}

pub trait LoadProbe: Send + Sync {
    /// Returns host load in `[0.0, 1.0]`.
    fn load_normalized(&self) -> Result<f64>;
}

pub trait CpuShareSink: Send + Sync {
    fn set_shares(&self, shares: u64) -> Result<()>;
}

pub struct Monitor {
    pub policy: ScalingPolicy,
    pub state: Arc<MonitorState>,
    pub load: Arc<dyn LoadProbe>,
    pub sink: Arc<dyn CpuShareSink>,
}

impl Monitor {
    pub fn run_loop(self) {
        eprintln!(
            "monitor: starting (shares {}..{}, every {:?})",
            self.policy.min_cpu_shares, self.policy.max_cpu_shares, self.policy.poll_interval
        );
        // Apply initial floor so the sink sees a real value.
        let _ = self.sink.set_shares(self.policy.min_cpu_shares);
        while !self.state.shutdown.load(Ordering::Acquire) {
            self.tick();
            std::thread::sleep(self.policy.poll_interval);
        }
        eprintln!("monitor: stopped");
    }

    pub fn tick(&self) {
        let load = match self.load.load_normalized() {
            Ok(l) => l,
            Err(e) => {
                eprintln!("monitor: load probe failed: {e:#}");
                return;
            }
        };
        let current = self.state.cpu_shares.load(Ordering::Acquire);
        let next = if load >= self.policy.high_load_threshold && current < self.policy.max_cpu_shares
        {
            let headroom = self.policy.max_cpu_shares - current;
            (current + (headroom / 4).max(1)).min(self.policy.max_cpu_shares)
        } else if load <= self.policy.low_load_threshold && current > self.policy.min_cpu_shares {
            let slack = current - self.policy.min_cpu_shares;
            (current - (slack / 4).max(1)).max(self.policy.min_cpu_shares)
        } else {
            current
        };
        if next == current {
            return;
        }
        if let Err(e) = self.sink.set_shares(next) {
            eprintln!("monitor: set_shares({next}) failed: {e:#}");
            return;
        }
        self.state.cpu_shares.store(next, Ordering::Release);
        eprintln!("monitor: load={load:.2} -> cpu_shares {current} -> {next}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Mutex;

    struct ScriptedLoad {
        values: Vec<f64>,
        idx: AtomicUsize,
    }
    impl LoadProbe for ScriptedLoad {
        fn load_normalized(&self) -> Result<f64> {
            let i = self.idx.fetch_add(1, Ordering::Relaxed);
            Ok(self.values.get(i).copied().unwrap_or(*self.values.last().unwrap()))
        }
    }

    struct RecordingSink(Mutex<Vec<u64>>);
    impl CpuShareSink for RecordingSink {
        fn set_shares(&self, shares: u64) -> Result<()> {
            self.0.lock().unwrap().push(shares);
            Ok(())
        }
    }

    fn make_policy() -> ScalingPolicy {
        ScalingPolicy {
            min_cpu_shares: 256,
            max_cpu_shares: 2048,
            high_load_threshold: 0.80,
            low_load_threshold: 0.20,
            poll_interval: Duration::from_millis(0),
        }
    }

    #[test]
    fn high_load_increases_cpu_shares() {
        let policy = make_policy();
        let state = MonitorState::new(&policy);
        let sink = Arc::new(RecordingSink(Mutex::new(Vec::new())));
        let m = Monitor {
            policy,
            state,
            load: Arc::new(ScriptedLoad {
                values: vec![0.95],
                idx: AtomicUsize::new(0),
            }),
            sink: sink.clone(),
        };
        let before = m.state.cpu_shares.load(Ordering::Acquire);
        m.tick();
        assert_eq!(sink.0.lock().unwrap().len(), 1);
        assert!(m.state.cpu_shares.load(Ordering::Acquire) > before);
    }

    #[test]
    fn moderate_load_is_inert() {
        let policy = make_policy();
        let state = MonitorState::new(&policy);
        let sink = Arc::new(RecordingSink(Mutex::new(Vec::new())));
        let m = Monitor {
            policy,
            state,
            load: Arc::new(ScriptedLoad {
                values: vec![0.5],
                idx: AtomicUsize::new(0),
            }),
            sink: sink.clone(),
        };
        m.tick();
        assert!(sink.0.lock().unwrap().is_empty());
    }
}
