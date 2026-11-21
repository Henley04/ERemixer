//! DSP 处理单元：双二阶滤波器、压缩器、侧链 ducking、均衡、立体声宽度、真峰限幅。

pub mod biquad;
pub mod dynamics;
pub mod loudness;
pub mod stereo;

pub use biquad::{Biquad, FilterType};
pub use dynamics::{Compressor, CompressorParams, SidechainDucker};
pub use stereo::StereoWidener;
