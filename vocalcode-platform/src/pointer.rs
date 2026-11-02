//! Background sampling of "would the thing under the pointer accept text?".
//!
//! UI Automation has no timeout on Windows, and a provider that stops answering
//! blocks its caller forever. Probing from the UI thread would therefore freeze
//! the whole app on a busy browser tab or a hung Word instance, so the probe
//! owns a thread and publishes its last answer.
//!
//! The answer is **advisory**: it decides whether the cursor dot arms. Delivery
//! is still gated by [`crate::inject::focused_target_is_editable`], so a probe
//! that guesses wrong can only mis-colour a dot — it can never widen where text
//! is written.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How often the worker re-asks. Fast enough that the dot arms within a blink,
/// slow enough that a provider is not hammered.
const SAMPLE: Duration = Duration::from_millis(60);

/// A probe older than this is treated as "unknown", which means "do not arm".
/// A provider that hangs must fail closed, not freeze the last answer.
const STALE_AFTER_MS: u64 = 1_000;

const UNKNOWN: u64 = u64::MAX;

pub struct PointProbe {
    point: Arc<AtomicU64>,
    armed: Arc<AtomicBool>,
    updated_ms: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    started: Instant,
    worker: Option<JoinHandle<()>>,
}

impl PointProbe {
    pub fn new() -> Self {
        let point = Arc::new(AtomicU64::new(UNKNOWN));
        let armed = Arc::new(AtomicBool::new(false));
        let updated_ms = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let started = Instant::now();
        let worker = {
            let point = point.clone();
            let armed = armed.clone();
            let updated_ms = updated_ms.clone();
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("vocalcode-point-probe".to_string())
                .spawn(move || {
                    while !stop.load(Ordering::Acquire) {
                        let packed = point.load(Ordering::Acquire);
                        if packed != UNKNOWN {
                            let x = (packed >> 32) as u32 as i32;
                            let y = packed as u32 as i32;
                            let editable = crate::inject::text_target_at_point(x, y);
                            // Publish the timestamp *after* the answer: a probe
                            // that never returns leaves the old value in place,
                            // and `armed` then reports false once it is stale.
                            armed.store(editable, Ordering::Release);
                            updated_ms
                                .store(started.elapsed().as_millis() as u64, Ordering::Release);
                        }
                        std::thread::sleep(SAMPLE);
                    }
                })
                .ok()
        };
        Self {
            point,
            armed,
            updated_ms,
            stop,
            started,
            worker,
        }
    }

    /// Ask about a new point. The worker samples it on its next pass.
    pub fn set_point(&self, x: i32, y: i32) {
        self.point.store(
            ((x as u32 as u64) << 32) | (y as u32 as u64),
            Ordering::Release,
        );
    }

    /// Whether the last completed probe found an editable target, and whether
    /// that answer is still fresh enough to trust.
    pub fn armed(&self) -> bool {
        if !self.armed.load(Ordering::Acquire) {
            return false;
        }
        let updated = self.updated_ms.load(Ordering::Acquire);
        let now = self.started.elapsed().as_millis() as u64;
        now.saturating_sub(updated) < STALE_AFTER_MS
    }
}

impl Default for PointProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for PointProbe {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // Deliberately not joined: the worker may be blocked inside a hung UIA
        // provider, and shutdown must not wait for it. It owns no resource the
        // process needs to release, and it exits by itself if the provider ever
        // answers.
        self.worker.take();
    }
}
