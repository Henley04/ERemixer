//! OneMix 核心错误类型。

use thiserror::Error;

#[derive(Debug, Error)]
pub enum MixError {
    #[error("I/O 错误: {0}")]
    Io(#[from] std::io::Error),

    #[error("WAV 解码/编码失败: {0}")]
    Wav(String),

    #[error("参数校验失败: {0}")]
    InvalidParam(String),

    #[error("音频通道不受支持: {0}")]
    UnsupportedChannels(usize),

    #[error("采样率不受支持: {0}")]
    UnsupportedSampleRate(u32),

    #[error("NN 推理失败: {0}")]
    Inference(String),

    #[error("模型不可用: {0}")]
    ModelUnavailable(String),

    #[error("流水线被取消")]
    Cancelled,

    #[error("内部错误: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, MixError>;
