//! Small, dependency-free animation helpers.
//!
//! Frames are paced by DWM (`DwmFlush`) on a helper thread that only exists
//! while something animates; idle has no clock. The thread invalidates the
//! window instead of posting messages: `WM_PAINT` is only delivered when no
//! input is pending, so animating never starves mouse/keyboard handling.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::Graphics::Gdi::InvalidateRect;

/// WinUI `ControlNormalAnimationDuration`.
pub const NORMAL_MS: u64 = 250;
/// WinUI `ControlFastAnimationDuration`.
pub const FAST_MS: u64 = 167;

#[derive(Debug, Clone, Copy)]
pub struct Animation {
    start: Instant,
    duration: Duration,
    from: f32,
    to: f32,
}

impl Animation {
    /// Fluent "fast out, slow in" motion: `cubic-bezier(0, 0, 0, 1)`.
    pub fn fluent(duration_ms: u64, from: f32, to: f32) -> Self {
        Self {
            start: Instant::now(),
            duration: Duration::from_millis(duration_ms.max(1)),
            from,
            to,
        }
    }

    pub fn delayed(mut self, delay_ms: u64) -> Self {
        self.start += Duration::from_millis(delay_ms);
        self
    }

    pub fn value_at(&self, now: Instant) -> f32 {
        let elapsed = now.saturating_duration_since(self.start).as_secs_f32();
        let t = (elapsed / self.duration.as_secs_f32()).clamp(0.0, 1.0);
        self.from + (self.to - self.from) * fluent(t)
    }

    pub fn is_finished(&self) -> bool {
        Instant::now().saturating_duration_since(self.start) >= self.duration
    }

    pub fn finished_value(&self) -> f32 {
        self.to
    }
}

/// `cubic-bezier(0, 0, 0, 1)` solved in closed form: x = s³, y = 3s² − 2s³.
pub fn fluent(t: f32) -> f32 {
    let s = t.clamp(0.0, 1.0).cbrt();
    s * s * (3.0 - 2.0 * s)
}

pub fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// Indeterminate WinUI-style progress ring: the arc rotates while its head
/// runs ahead and its tail catches up. Returns `(start, sweep)` in radians.
pub fn ring_arc(elapsed: Duration) -> (f32, f32) {
    use std::f32::consts::{FRAC_PI_2, TAU};
    const CYCLE: f32 = 1.5;
    const MIN: f32 = 0.35;
    const MAX: f32 = 4.4;
    let span = MAX - MIN;
    let smooth = |x: f32| x * x * (3.0 - 2.0 * x);
    let t = elapsed.as_secs_f32();
    let cycles = (t / CYCLE).floor();
    let u = (t / CYCLE).fract();
    let (tail, sweep) = if u < 0.5 {
        (0.0, MIN + span * smooth(u * 2.0))
    } else {
        let s = smooth((u - 0.5) * 2.0);
        (span * s, MAX - span * s)
    };
    let start = t * TAU / 2.2 + cycles * span + tail - FRAC_PI_2;
    (start.rem_euclid(TAU), sweep)
}

/// Invalidates `hwnd` once per DWM composition while running; the window
/// advances its animations in `WM_PAINT`.
pub struct FrameClock {
    hwnd: isize,
    running: Option<Arc<AtomicBool>>,
}

impl FrameClock {
    pub fn new(hwnd: HWND) -> Self {
        Self {
            hwnd: hwnd.0 as isize,
            running: None,
        }
    }

    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    pub fn start(&mut self) {
        if self.running.is_some() {
            return;
        }
        // Each thread owns its flag, so a quick stop/start cannot leave two
        // threads sharing one.
        let running = Arc::new(AtomicBool::new(true));
        let flag = running.clone();
        let hwnd = self.hwnd;
        let spawned = std::thread::Builder::new()
            .name("vtdrop-frame-clock".into())
            .spawn(move || {
                while flag.load(Ordering::Acquire) {
                    let before = Instant::now();
                    // DwmFlush blocks until the next composition (vsync).
                    if unsafe { DwmFlush() }.is_err() || before.elapsed() < Duration::from_millis(2)
                    {
                        std::thread::sleep(Duration::from_millis(16));
                    }
                    if flag.load(Ordering::Acquire) {
                        unsafe {
                            let _ = InvalidateRect(Some(HWND(hwnd as *mut _)), None, false);
                        }
                    }
                }
            });
        if spawned.is_ok() {
            self.running = Some(running);
        }
    }

    pub fn stop(&mut self) {
        if let Some(flag) = self.running.take() {
            flag.store(false, Ordering::Release);
        }
    }

}

impl Drop for FrameClock {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fluent_is_monotonic_and_bounded() {
        assert_eq!(fluent(0.0), 0.0);
        assert_eq!(fluent(1.0), 1.0);
        let mut previous = 0.0;
        for step in 0..=100 {
            let value = fluent(step as f32 / 100.0);
            assert!(value >= previous && value <= 1.0);
            previous = value;
        }
        // Fast out: most of the distance is covered early.
        assert!(fluent(0.25) > 0.65);
    }

    #[test]
    fn ring_is_continuous_across_cycles() {
        let at = |ms: u64| ring_arc(Duration::from_millis(ms));
        for boundary in [1500u64, 3000, 4500] {
            let (a0, s0) = at(boundary - 1);
            let (a1, s1) = at(boundary);
            let da = (a1 - a0).rem_euclid(std::f32::consts::TAU);
            assert!(da < 0.05 || da > std::f32::consts::TAU - 0.05, "jump {da}");
            assert!((s1 - s0).abs() < 0.05);
        }
    }

    #[test]
    fn delayed_animation_holds_start_value() {
        let animation = Animation::fluent(100, 0.0, 1.0).delayed(10_000);
        assert_eq!(animation.value_at(Instant::now()), 0.0);
        assert!(!animation.is_finished());
    }
}
