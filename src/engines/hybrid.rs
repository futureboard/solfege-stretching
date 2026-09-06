//! Hybrid: harmonic/percussive separation with a per-branch phase policy.
//!
//! Implemented as the phase vocoder with the HPSS masks switched on, which is a
//! deliberate design choice rather than a shortcut: one shared STFT means the
//! two branches are sample-aligned by construction, so there is no per-branch
//! delay to measure and compensate. What the split still cannot avoid is mask
//! leakage, so this engine stays experimental until it has been listened to
//! against the single-engine baselines (dsp.md sec.7).

pub use super::pv::PvEngine;

/// Construct the hybrid configuration of the phase vocoder.
pub fn hybrid_engine() -> PvEngine {
    PvEngine::hybrid()
}
