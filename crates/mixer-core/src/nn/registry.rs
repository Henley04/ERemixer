//! SOTA 开源模型注册表。
//! 收录音乐源分离与音质增强领域当前 SOTA / 生产级开源模型，
//! 供 NN 适配层按 id 解析本地权重或下载源（modelscope / hf-mirror）。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelEntry {
    pub id: String,
    pub name: String,
    /// 任务类型：enhance(音质增强) / separation(音源分离) / dereverb(去混响)。
    pub task: String,
    /// 架构标识。
    pub architecture: String,
    /// 本地权重文件名（位于 models_dir）。None 表示需外部下载。
    pub local_file: Option<String>,
    /// 权重下载源（modelscope / hf-mirror），可选。
    pub source_url: Option<String>,
    /// 是否随原型分发（已附带轻量权重）。
    pub bundled: bool,
    /// 输入张量名。
    pub input_name: String,
    /// 输出张量名。
    pub output_name: String,
    /// 参考指标 / 说明。
    pub note: String,
}

pub const DEFAULT_ENHANCE_MODEL: &str = "onemix-dyn-v1";

/// 内置注册表。音乐源分离 SOTA 参考（MUSDB18-HQ SDR）：
/// BS-RoFormer ≈ 9.8 dB（SOTA）、HT-Demucs/htdemucs_ft ≈ 9.0 dB（生产默认）。
pub fn registry() -> Vec<ModelEntry> {
    vec![
        ModelEntry {
            id: "onemix-dyn-v1".into(),
            name: "OneMix 轻量动态增强器".into(),
            task: "enhance".into(),
            architecture: "Conv1d-ReLU-Conv1d-Tanh".into(),
            local_file: Some("onemix_dyn_enhance_v1.onnx".into()),
            source_url: None,
            bundled: true,
            input_name: "x".into(),
            output_name: "y".into(),
            note: "随原型分发，375B，逐帧动态增益曲线；用于一键混音的 NN 增强演示。"
                .into(),
        },
        ModelEntry {
            id: "bs-roformer-vocal".into(),
            name: "BS-RoFormer 人声分离".into(),
            task: "separation".into(),
            architecture: "Band-Split RoPE Transformer".into(),
            local_file: Some("bs_roformer_vocal.onnx".into()),
            source_url: Some(
                "https://hf-mirror.com/anvuew/BS-RoFormer/resolve/main/vocal.onnx".into(),
            ),
            bundled: false,
            input_name: "input".into(),
            output_name: "output".into(),
            note: "MUSDB18-HQ 当前 SOTA，人声 SDR ≈ 10.9 dB；需自行下载 ONNX 权重。".into(),
        },
        ModelEntry {
            id: "htdemucs-ft".into(),
            name: "HT-Demucs FT 4-stem".into(),
            task: "separation".into(),
            architecture: "Hybrid Transformer Demucs (fine-tuned)".into(),
            local_file: Some("htdemucs_ft.onnx".into()),
            source_url: Some(
                "https://hf-mirror.com/StemSplitio/htdemucs-ft-onnx/resolve/main/model.onnx"
                    .into(),
            ),
            bundled: false,
            input_name: "input".into(),
            output_name: "output".into(),
            note: "生产级默认，4 源分离 ≈ 9.0 dB SDR；速度与质量均衡。".into(),
        },
        ModelEntry {
            id: "mel-band-roformer".into(),
            name: "Mel-Band RoFormer".into(),
            task: "dereverb".into(),
            architecture: "Mel-Band Split RoPE Transformer".into(),
            local_file: Some("mel_band_roformer.onnx".into()),
            source_url: Some(
                "https://hf-mirror.com/anvuew/dereverb_bs_roformer/resolve/main/model.onnx".into(),
            ),
            bundled: false,
            input_name: "input".into(),
            output_name: "output".into(),
            note: "梅尔频带划分变体，去混响/人声净化；适合混音前的人声预处理。".into(),
        },
    ]
}

pub fn find_model(id: &str) -> Option<ModelEntry> {
    registry().into_iter().find(|m| m.id == id)
}

/// 列出所有可用（含需下载）的模型 id。
pub fn list_ids() -> Vec<String> {
    registry().into_iter().map(|m| m.id).collect()
}
