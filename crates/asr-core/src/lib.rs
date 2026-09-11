//! `asr-core`: the UI-agnostic core of the transcription app.
//!
//! **Discipline this crate must maintain** (see the plan doc): no GPUI, no
//! HTTP server framework, no Tauri. Everything here must be drivable from a
//! plain CLI, because that's how the Phase 1 accuracy work (the whole point
//! of the MVP) gets done — and it's the retreat path to a future cloud
//! build.

pub mod asr;
pub mod audio;
pub mod diarize;
pub mod error;
pub mod export;
pub mod pipeline;
pub mod speaker;
pub mod store;
pub mod terms;

pub use error::{CoreError, Result};
