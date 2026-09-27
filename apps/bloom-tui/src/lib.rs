pub mod app;
pub mod config;
pub mod daemon;
pub mod ui;

/// Re-exported so callers and integration tests can build key events without
/// depending on the terminal backend directly.
pub use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Coalesces redraw requests and enforces a 120 FPS maximum.
#[derive(Debug, Default)]
pub struct RedrawScheduler {
    pending: bool,
    last: Option<std::time::Instant>,
}

impl RedrawScheduler {
    const FRAME_INTERVAL: std::time::Duration = std::time::Duration::from_nanos(8_333_334);

    pub fn request(&mut self) {
        self.pending = true;
    }

    pub fn wake_after(&self, now: std::time::Instant) -> Option<std::time::Duration> {
        self.pending.then(|| {
            self.last.map_or(std::time::Duration::ZERO, |last| {
                Self::FRAME_INTERVAL.saturating_sub(now.saturating_duration_since(last))
            })
        })
    }

    pub fn take_due(&mut self, now: std::time::Instant) -> bool {
        if !self.pending
            || self
                .last
                .is_some_and(|last| now.saturating_duration_since(last) < Self::FRAME_INTERVAL)
        {
            return false;
        }
        self.pending = false;
        self.last = Some(now);
        true
    }
}

#[cfg(test)]
mod redraw_tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn coalesces_requests_and_caps_frames_at_120_fps() {
        let start = Instant::now();
        let mut scheduler = RedrawScheduler::default();
        scheduler.request();
        scheduler.request();
        assert!(scheduler.take_due(start));
        scheduler.request();
        assert!(!scheduler.take_due(start + Duration::from_millis(8)));
        assert!(scheduler.take_due(start + Duration::from_micros(8_334)));
        assert!(!scheduler.take_due(start + Duration::from_secs(1)));
        assert_eq!(scheduler.wake_after(start), None);
        scheduler.request();
        assert_eq!(
            scheduler.wake_after(start + Duration::from_micros(16_334)),
            Some(Duration::from_nanos(333_334))
        );
        assert_eq!(
            scheduler.wake_after(start + Duration::from_micros(16_668)),
            Some(Duration::ZERO)
        );
        assert!(scheduler.take_due(start + Duration::from_micros(16_668)));
        assert_eq!(scheduler.wake_after(start), None);
    }
}
