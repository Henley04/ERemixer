//! 统一 API 数据结构 —— CLI / GUI / SXSEditor 插件三者共享同一套请求与结果 schema，
//! 从而保证"能力完全一致"。

use serde::{Deserialize, Serialize};

/// 输入模式：决定混音引擎如何处理输入。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MixMode {
    /// 分轨直入：输入是已经分离好的乐器分轨，跳过所有分离模型，直接进入混音链。
    /// 这是最快、无损于分轨质量的路径；适用于手上已有 stems 的场景。
    Stems,
    /// 分离再混：输入是一条成品混音（full mix），先用 SOTA 分离模型拆成乐器分轨，
    /// 再逐轨处理并重新混音。若分离模型权重不可用，会优雅降级并明确告知。
    Separate,
}

impl Default for MixMode {
    fn default() -> Self {
        MixMode::Stems
    }
}

impl MixMode {
    pub fn from_name(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "stems" | "stem" | "分轨" | "tracks" | "multitrack" => Some(MixMode::Stems),
            "separate" | "separation" | "split" | "分离" | "demix" => Some(MixMode::Separate),
            _ => None,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            MixMode::Stems => "stems",
            MixMode::Separate => "separate",
        }
    }
    pub fn names() -> &'static [&'static str] {
        &["stems", "separate"]
    }
}

/// 乐器类型：用于选择对应的专属处理链。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Instrument {
    /// 自动推断：引擎根据文件名/路径关键词识别乐器（默认）。
    Auto,
    /// 人声：HPF + 存在感 + 去齿音 + 压缩 + 侧链主控。
    #[serde(alias = "vocal", alias = "voice", alias = "人声")]
    Vocals,
    /// 鼓组：透明压缩 + 瞬态保留 + 轻度宽度。
    Drums,
    /// 贝斯：单声道低频 + 紧压缩 + 低频管理。
    Bass,
    /// 伴奏/器乐（含未细分的其他乐器）：中频雕刻 + 闪避。
    Instrumental,
    /// 其他乐器（吉他、键盘等）：温和 EQ + 轻压缩。
    Other,
}

impl Default for Instrument {
    fn default() -> Self {
        Instrument::Auto
    }
}

impl Instrument {
    /// 从名称解析乐器类型，用于命令行 `--stems path:vocals`。
    pub fn from_name(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" | "" => Some(Instrument::Auto),
            "vocal" | "vocals" | "voice" | "人声" | "主唱" => Some(Instrument::Vocals),
            "drum" | "drums" | "鼓" | "鼓组" => Some(Instrument::Drums),
            "bass" | "贝斯" | "低音" => Some(Instrument::Bass),
            "inst" | "instrumental" | "accompaniment" | "伴奏" | "器乐" => Some(Instrument::Instrumental),
            "other" | "其他" | "乐器" => Some(Instrument::Other),
            _ => None,
        }
    }

    /// 自动推断：根据路径/文件名关键词识别乐器。
    /// 识别失败时返回 `Other`（温和链，不会破坏素材）。
    pub fn infer(path: &str) -> Self {
        let p = path.replace('\\', "/").to_ascii_lowercase();
        // 只取文件名（去掉目录），避免目录名（如 /music/、/sub/）造成误判。
        let name = p.rsplit('/').next().unwrap_or(&p);
        // 顺序有优先级：先专有乐器，后通用词。
        // 注意：不使用 "lead"/"sub"/"music" 这类歧义词（lead guitar 不是人声，
        // sub mix 不是贝斯），否则会把乐器送进错误的处理链、甚至拿到侧链主控权。
        let has = |kw: &[&str]| kw.iter().any(|k| name.contains(k));
        if has(&["vocal", "vocals", "voice", "vox", "人声", "主唱", "sing"]) {
            Instrument::Vocals
        } else if has(&["drum", "drums", "kick", "snare", "hihat", "perc", "鼓"]) {
            Instrument::Drums
        } else if has(&["bass", "贝斯", "低音"]) {
            Instrument::Bass
        } else if has(&["inst", "instrumental", "accomp", "backing", "karaoke", "伴奏", "器乐", "bgm"]) {
            Instrument::Instrumental
        } else {
            Instrument::Other
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Instrument::Auto => "auto",
            Instrument::Vocals => "vocals",
            Instrument::Drums => "drums",
            Instrument::Bass => "bass",
            Instrument::Instrumental => "instrumental",
            Instrument::Other => "other",
        }
    }
}

/// 单条输入分轨。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StemInput {
    /// WAV 文件路径。
    pub path: String,
    /// 乐器类型。`Auto` 表示由引擎自动推断；推断结果会写回 MixResult。
    pub instrument: Instrument,
    /// 该轨相对增益（dB）。
    pub gain_db: f32,
}

impl Default for StemInput {
    fn default() -> Self {
        Self {
            path: String::new(),
            instrument: Instrument::Auto,
            gain_db: 0.0,
        }
    }
}

/// 分离模型的解析结果（写入 MixResult，供前端展示实际用了什么乐器轨）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StemInfo {
    pub path: String,
    pub instrument: Instrument,
    /// 乐器类型是否为自动推断（true）还是用户显式指定（false）。
    pub inferred: bool,
    pub gain_db: f32,
}

/// 一键混音请求。所有前端都序列化该结构传给核心引擎。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MixRequest {
    /// 输入模式：分轨直入 or 分离再混。
    pub mode: MixMode,
    /// 分轨列表（Stems 模式使用）。为空时回退到传统的 vocals_path + accompaniment_path 两轨。
    pub stems: Vec<StemInput>,
    /// 分离模式下的成品混音输入路径。
    pub source_path: String,
    /// 分离模型 id（Separate 模式）；None 表示注册表默认分离模型。
    pub separation_model: Option<String>,
    /// 人声干声文件路径（WAV）。
    pub vocals_path: String,
    /// 伴奏文件路径（WAV）。
    pub accompaniment_path: String,
    /// 输出混音 WAV 路径。
    pub output_path: String,

    /// 目标响度（LUFS）。流媒体常用 -14，广播 -16/-23。
    pub target_lufs: f32,
    /// 真峰上限（dBTP）。
    pub true_peak_ceiling: f32,
    /// 输出采样率；None 表示沿用输入。
    pub output_sample_rate: Option<u32>,
    /// 导出位深：16 / 24 / 32(float)。
    pub output_bit_depth: u16,

    /// 人声相对伴奏的增益（dB）。
    pub vocal_gain_db: f32,
    /// 伴奏与人声的响度平衡量：负值表示伴奏人声出现时自动让位（ducking，dB）。
    pub ducking_db: f32,
    /// 人声亮度（高频 shelf，dB）。
    pub vocal_presence_db: f32,
    /// 人声低频切除（高通 Hz）。
    pub vocal_highpass_hz: f32,
    /// 伴奏在中频（人声频段）的削减量（dB，负值）。
    pub accompaniment_carve_db: f32,
    /// 总线压缩量 0..1。
    pub bus_glue: f32,
    /// 立体声宽度 0..2（1=原始）。
    pub stereo_width: f32,

    /// 是否启用 NN 增强（去混响/音质增强）。需要可用模型。
    pub nn_enhance: bool,
    /// NN 模型 id；None 表示使用注册表默认模型。
    pub nn_model: Option<String>,
    /// NN 湿混比例 0..1。
    pub nn_mix: f32,
    /// 模型/资源根目录（用于定位 .onnx 与原生库），None 表示自动探测。
    pub models_dir: Option<String>,

    /// 预设名称（写入结果以追溯）。
    pub preset: String,
    /// 进度回调是否输出细节阶段。
    pub verbose: bool,
}

impl Default for MixRequest {
    fn default() -> Self {
        Self {
            mode: MixMode::Stems,
            stems: Vec::new(),
            source_path: String::new(),
            separation_model: None,
            vocals_path: String::new(),
            accompaniment_path: String::new(),
            output_path: String::new(),
            target_lufs: -14.0,
            true_peak_ceiling: -1.0,
            output_sample_rate: None,
            output_bit_depth: 24,
            vocal_gain_db: 0.0,
            ducking_db: -2.5,
            vocal_presence_db: 2.0,
            vocal_highpass_hz: 75.0,
            accompaniment_carve_db: -2.0,
            bus_glue: 0.35,
            stereo_width: 1.1,
            nn_enhance: false,
            nn_model: None,
            nn_mix: 0.5,
            models_dir: None,
            preset: "streaming".to_string(),
            verbose: true,
        }
    }
}

/// 混音完成后返回的度量与产物信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MixResult {
    pub ok: bool,
    pub output_path: String,
    pub sample_rate: u32,
    pub channels: usize,
    pub duration_sec: f32,
    /// 混音前输入响度。
    pub input_vocals_lufs: f32,
    pub input_accompaniment_lufs: f32,
    /// 混音后、响度标准化前响度。
    pub premaster_lufs: f32,
    /// 最终输出响度（LUFS）。
    pub output_lufs: f32,
    /// 最终真峰（dBTP）。
    pub output_true_peak_dbtp: f32,
    /// 响度标准化增益（dB）。
    pub normalization_gain_db: f32,
    /// 实际是否使用了 NN 增强。
    pub nn_applied: bool,
    /// NN 使用的模型 id。
    pub nn_model_id: Option<String>,
    /// 耗时（毫秒）。
    pub elapsed_ms: u64,
    /// 预设。
    pub preset: String,
    /// 流水线阶段记录。
    pub stages: Vec<String>,
    /// 实际使用的输入模式。
    pub mode: MixMode,
    /// 实际参与混音的分轨（含推断出的乐器类型）。
    pub stems_used: Vec<StemInfo>,
    /// 是否真正运行了分离模型（Stems 模式恒为 false；Separate 模式取决于模型是否可用）。
    pub separation_applied: bool,
    /// 分离模型 id（若运行了分离）。
    pub separation_model_id: Option<String>,
    /// 最终响度是否落在目标 ±0.5 LU 内。限幅过重时可能无法同时满足响度与真峰上限，
    /// 此时为 false，前端应提示用户而不是假装达标。
    #[serde(default)]
    pub loudness_target_met: bool,
}

/// 进度事件，供 CLI/GUI/插件统一展示。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgressEvent {
    pub percent: u32,
    pub stage: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// 预设：一组对 MixRequest 默认值的覆盖。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum Preset {
    /// 流媒体（Spotify/Apple Music）：-14 LUFS，-1 dBTP。
    Streaming,
    /// 广播：-16 LUFS，-1 dBTP。
    Broadcast,
    /// 安静环境：-20 LUFS。
    Quiet,
    /// 响亮/短视频：-12 LUFS。
    Loud,
}

impl Preset {
    pub fn apply(self, req: &mut MixRequest) {
        let (lufs, tp, duck, glue, width) = match self {
            Preset::Streaming => (-14.0, -1.0, -2.5, 0.35, 1.10),
            Preset::Broadcast => (-16.0, -1.0, -2.0, 0.30, 1.05),
            Preset::Quiet => (-20.0, -1.5, -1.5, 0.25, 1.00),
            Preset::Loud => (-12.0, -1.0, -3.5, 0.45, 1.15),
        };
        req.target_lufs = lufs;
        req.true_peak_ceiling = tp;
        req.ducking_db = duck;
        req.bus_glue = glue;
        req.stereo_width = width;
        req.preset = match self {
            Preset::Streaming => "streaming",
            Preset::Broadcast => "broadcast",
            Preset::Quiet => "quiet",
            Preset::Loud => "loud",
        }
        .to_string();
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "streaming" | "default" => Some(Preset::Streaming),
            "broadcast" => Some(Preset::Broadcast),
            "quiet" => Some(Preset::Quiet),
            "loud" | "shorts" => Some(Preset::Loud),
            _ => None,
        }
    }

    pub fn names() -> &'static [&'static str] {
        &["streaming", "broadcast", "quiet", "loud"]
    }
}
