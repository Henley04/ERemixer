//! OneMix 核心库：一键混音（人声 + 伴奏 → 成品母带）。
//!
//! 架构：纯 Rust DSP 混音链 + 可选 ONNX NN 增强层。
//! CLI / GUI / SXSEditor 插件共用本库，保证能力完全一致。

pub mod api;
pub mod audio;
pub mod dsp;
pub mod error;
pub mod nn;
pub mod pipeline;

pub use api::{MixRequest, MixResult, Preset, ProgressEvent};
pub use audio::{resample_buffer, resample_linear, read_wav, to_stereo, write_wav, AudioBuffer};
pub use error::{MixError, Result};
pub use pipeline::{mix, mix_with_progress, mix_with_progress_cancel};
