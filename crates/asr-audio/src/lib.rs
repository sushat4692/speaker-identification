//! Realtime playback transport for the editor UI.
//!
//! [`Transport`] owns a `cpal` output stream and applies pitch-preserving
//! rate change via `signalsmith-stretch`, validated by ear against real
//! Japanese speech in `bin/playback_spike.rs` before being generalized here
//! (see docs/plan.md's playback-integration section).

mod transport;
pub use transport::Transport;

/// Playback rate, always paired with pitch held at 1.0 — see the plan's
/// audio architecture section for why this can't be a mixer effect and must
/// live inside the source.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlaybackRate(f32);

impl PlaybackRate {
    pub const MIN: f32 = 0.5;
    pub const MAX: f32 = 2.0;

    pub fn new(rate: f32) -> Option<Self> {
        if (Self::MIN..=Self::MAX).contains(&rate) {
            Some(Self(rate))
        } else {
            None
        }
    }

    pub fn get(self) -> f32 {
        self.0
    }
}

/// A loop region in milliseconds, `[start, end)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoopRegion {
    pub start_ms: u64,
    pub end_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_out_of_range_is_rejected() {
        assert!(PlaybackRate::new(0.4).is_none());
        assert!(PlaybackRate::new(2.1).is_none());
        assert!(PlaybackRate::new(1.0).is_some());
    }
}
