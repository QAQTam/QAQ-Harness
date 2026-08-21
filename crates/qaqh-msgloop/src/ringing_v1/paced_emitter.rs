//! Direct streaming output for high-frequency model deltas.
//!
//! The renderer owns frame-level coalescing. Keeping the worker transport
//! immediate prevents hidden server-side latency at high token rates.
//!
//! M3 后：仅 Ringing 线格式（`emit_domain` / `emit_timeline`）；
//! legacy `emit` / `emit_delta`（Agent2Ui 路径）已完全拆除。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use super::types::Emitter;
use super::types::WriterEvent;

pub struct PacedEmitter {
    /// 当前会话 seed（Ringing 事件信封路由键）。会话切换后经
    /// `set_seed` 更新；构造时快照的旧值在 resume 模式下为空，
    /// 必须由 Loop 在 init_session 后同步。
    seed: Arc<Mutex<String>>,
    tx: mpsc::SyncSender<WriterEvent>,
    writer_dead: Arc<AtomicBool>,
    causation: Arc<Mutex<Option<String>>>,
}

impl PacedEmitter {
    pub fn new(
        seed: impl Into<String>,
        tx: mpsc::SyncSender<WriterEvent>,
        writer_dead: Arc<AtomicBool>,
    ) -> Self {
        Self {
            seed: Arc::new(Mutex::new(seed.into())),
            tx,
            writer_dead,
            causation: Arc::new(Mutex::new(None)),
        }
    }

    /// 进入一个命令执行的作用域：期间 `emit_domain` 产出的事件携带
    /// `causation_id`。返回的 guard 在 Drop 时恢复上一个作用域（支持嵌套）。
    pub fn enter_causation(&self, causation: Option<&str>) -> CausationGuard {
        let previous = {
            let mut slot = self.causation.lock().unwrap_or_else(|e| e.into_inner());
            let previous = slot.clone();
            *slot = causation.map(str::to_string);
            previous
        };
        CausationGuard {
            slot: self.causation.clone(),
            previous,
        }
    }

    /// 同步当前会话 seed。会话创建/恢复（含 auto-create、worker 内切换）
    /// 后调用，使 Ringing 事件信封携带正确的路由键。
    pub fn set_seed(&self, seed: &str) {
        let mut slot = self.seed.lock().unwrap_or_else(|e| e.into_inner());
        *slot = seed.to_string();
    }
}

/// 命令作用域 guard：Drop 时恢复进入前的 causation。
pub struct CausationGuard {
    slot: Arc<Mutex<Option<String>>>,
    previous: Option<String>,
}

impl Drop for CausationGuard {
    fn drop(&mut self) {
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        *slot = self.previous.take();
    }
}

impl Emitter for PacedEmitter {
    fn emit_domain(&self, event: qaqh_domain::DomainEvent) {
        if self.writer_dead.load(Ordering::SeqCst) {
            return;
        }
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let seed = self.seed.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let causation = self
            .causation
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let env = qaqh_ringing::RingingWorkerEventEnvelope::new(
            seed.as_str(),
            format!("w-{seq}"),
            event.into(),
        );
        let env = match causation {
            Some(c) => env.with_causation(c),
            None => env,
        };
        let _ = self.tx.send(WriterEvent::Ringing(env));
    }

    fn emit_timeline(&self, intent: qaqh_domain::TimelineIntent) {
        if self.writer_dead.load(Ordering::SeqCst) {
            return;
        }
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let seed = self.seed.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let causation = self
            .causation
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let env = qaqh_ringing::RingingTimelineIntentEnvelope::new(
            seed.as_str(),
            format!("timeline-{seq}"),
            intent,
        );
        let env = match causation {
            Some(c) => env.with_causation(c),
            None => env,
        };
        let _ = self.tx.send(WriterEvent::Timeline(env));
    }

    fn event_tx(&self) -> Option<std::sync::mpsc::SyncSender<WriterEvent>> {
        Some(self.tx.clone())
    }
}
