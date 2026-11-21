//! StemFX 风格迁移：把"参考歌曲的混音风格"迁移到自己的分轨上。
//!
//! 流程（与论文一致）：
//! 1. 4 轨分轨（44.1 kHz 立体声）→ mel + 64 维混音特征 → BSFiLM 编码器 → 512 维风格嵌入
//! 2. 源/参考两个嵌入 → 6 层 Transformer 解码器自回归预测 multiafx 效果器链
//! 3. 把效果器链渲染到源分轨上 → 输出成品
//!
//! 模型为 W8A32（int8 权重 + fp32 激活），默认内嵌在二进制中。

pub mod chain;
pub mod mel;
pub mod model;
pub mod render;

use crate::audio::{resample_buffer, to_stereo, AudioBuffer};
use crate::error::{MixError, Result};
pub use chain::{Chain, FxStep};
pub use model::StemFxModel;
use std::time::Instant;

/// 四轨分轨集合。缺失的轨用静音补齐（编码器需要固定 4 轨输入）。
#[derive(Debug, Clone, Default)]
pub struct StemSet {
    pub vocals: Option<AudioBuffer>,
    pub bass: Option<AudioBuffer>,
    pub drums: Option<AudioBuffer>,
    pub other: Option<AudioBuffer>,
}

impl StemSet {
    pub fn is_complete(&self) -> bool {
        self.vocals.is_some() && self.bass.is_some() && self.drums.is_some() && self.other.is_some()
    }

    /// 从 `(path, instrument)` 列表构建。instrument ∈ vocals/bass/drums/other/instrumental。
    pub fn from_specs(specs: &[(String, String)]) -> Result<Self> {
        let mut set = StemSet::default();
        for (path, inst) in specs {
            let buf = crate::read_wav(path)?;
            match inst.to_ascii_lowercase().as_str() {
                "vocals" | "vocal" | "voice" => set.vocals = Some(buf),
                "bass" => set.bass = Some(buf),
                "drums" | "drum" => set.drums = Some(buf),
                "other" | "instrumental" | "inst" | "accompaniment" => set.other = Some(buf),
                _ => set.other = Some(buf),
            }
        }
        if set.vocals.is_none() && set.bass.is_none() && set.drums.is_none() && set.other.is_none()
        {
            return Err(MixError::InvalidParam("没有可用的分轨输入".into()));
        }
        Ok(set)
    }

    fn planes(&self, which: Stem) -> Vec<Vec<f32>> {
        match which {
            Stem::Vocals => self.vocals.as_ref(),
            Stem::Bass => self.bass.as_ref(),
            Stem::Drums => self.drums.as_ref(),
            Stem::Other => self.other.as_ref(),
        }
        .map(to_441_stereo)
        .unwrap_or_default()
    }
}

#[derive(Clone, Copy)]
enum Stem {
    Vocals,
    Bass,
    Drums,
    Other,
}

impl Stem {
    fn name(self) -> &'static str {
        match self {
            Stem::Vocals => "vocals",
            Stem::Bass => "bass",
            Stem::Drums => "drums",
            Stem::Other => "other",
        }
    }
    fn all() -> [Stem; 4] {
        [Stem::Vocals, Stem::Bass, Stem::Drums, Stem::Other]
    }
}

#[derive(Debug, Clone)]
pub struct StyleOptions {
    /// 单轨效果器数量上限（解码步数）。论文默认 512。
    pub max_tokens: usize,
    /// 渲染后把每条分轨归一到该响度（LUFS）；None 表示保持原电平。
    pub target_lufs: Option<f32>,
    /// 模型目录（含 stemfx/ 子目录）。None 时使用内嵌权重。
    pub models_dir: Option<String>,
    /// 是否渲染音频（false 只返回预测出的效果链）。
    pub render: bool,
}

impl Default for StyleOptions {
    fn default() -> Self {
        Self {
            max_tokens: 512,
            target_lufs: None,
            models_dir: None,
            render: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct StyleResult {
    pub chain: Chain,
    pub pretty: String,
    pub tokens: Vec<String>,
    /// 实际渲染的效果器。
    pub applied: Vec<String>,
    /// 因不支持（变调/变速等）而跳过的效果器。
    pub skipped: Vec<String>,
    /// 每条分轨的渲染结果（44.1 kHz 立体声）。
    pub stems: Vec<(String, AudioBuffer)>,
    pub sample_rate: u32,
    pub elapsed_ms: u64,
}

impl StyleResult {
    /// 所有分轨相加得到的成品混音。
    pub fn mixdown(&self) -> AudioBuffer {
        let frames = self.stems.iter().map(|(_, b)| b.frames()).max().unwrap_or(0);
        let mut acc = vec![0.0f32; frames * 2];
        for (_, b) in &self.stems {
            let planes = b.to_planar();
            for (c, plane) in planes.iter().enumerate().take(2) {
                for (i, v) in plane.iter().enumerate() {
                    acc[i * 2 + c] += *v;
                }
            }
        }
        AudioBuffer {
            samples: acc,
            channels: 2,
            sample_rate: self.sample_rate,
        }
    }
}

/// 执行一次风格迁移。
pub fn transfer(
    source: &StemSet,
    reference: &StemSet,
    opts: &StyleOptions,
    cancel: &dyn Fn() -> bool,
) -> Result<StyleResult> {
    let t0 = Instant::now();
    let mut model = StemFxModel::load(opts.models_dir.as_deref())?;

    // 1) 编码风格嵌入（10 s 片段）
    let src_planes: Vec<Vec<Vec<f32>>> = Stem::all().iter().map(|s| source.planes(*s)).collect();
    let ref_planes: Vec<Vec<Vec<f32>>> = Stem::all().iter().map(|s| reference.planes(*s)).collect();
    if src_planes.iter().all(|p| p.is_empty()) {
        return Err(MixError::InvalidParam("源分轨为空".into()));
    }
    if ref_planes.iter().all(|p| p.is_empty()) {
        return Err(MixError::InvalidParam("参考分轨为空".into()));
    }

    let emb_src = embed(&mut model, &src_planes)?;
    if cancel() {
        return Err(MixError::Cancelled);
    }
    let emb_ref = embed(&mut model, &ref_planes)?;
    if cancel() {
        return Err(MixError::Cancelled);
    }

    // 2) 预测效果链
    let tokens_i64 = model.predict_tokens(&emb_src, &emb_ref, opts.max_tokens, cancel)?;
    let (chain, token_names) = chain::decode(&model.meta, &tokens_i64);

    let mut result = StyleResult {
        chain,
        pretty: String::new(),
        tokens: token_names,
        applied: Vec::new(),
        skipped: Vec::new(),
        stems: Vec::new(),
        sample_rate: mel::SAMPLE_RATE,
        elapsed_ms: 0,
    };
    result.pretty = result.chain.pretty();

    // 3) 渲染
    if opts.render {
        let mut stems: Vec<(String, render::Planes)> = Vec::new();
        for (i, s) in Stem::all().iter().enumerate() {
            stems.push((s.name().to_string(), src_planes[i].clone()));
        }
        let report = render::render_chain(&result.chain, &mut stems, mel::SAMPLE_RATE as f32);
        result.applied = report.applied;
        result.skipped = report.skipped;

        for (name, mut planes) in stems {
            if let Some(target) = opts.target_lufs {
                let l = crate::dsp::loudness::integrated_lufs(&planes, mel::SAMPLE_RATE as f32);
                if l.is_finite() && l > -70.0 {
                    let g = 10f32.powf((target - l) / 20.0);
                    for p in planes.iter_mut() {
                        for v in p.iter_mut() {
                            *v *= g;
                        }
                    }
                }
            }
            result.stems.push((name, AudioBuffer::from_planar(planes, mel::SAMPLE_RATE)));
        }
    }

    result.elapsed_ms = t0.elapsed().as_millis() as u64;
    Ok(result)
}

/// 仅计算风格嵌入（供调试/工具使用）。
#[doc(hidden)]
pub fn embed_stems(model: &mut StemFxModel, set: &StemSet) -> Result<Vec<f32>> {
    let planes: Vec<Vec<Vec<f32>>> = Stem::all().iter().map(|s| set.planes(*s)).collect();
    embed(model, &planes)
}

fn embed(model: &mut StemFxModel, planes: &[Vec<Vec<f32>>]) -> Result<Vec<f32>> {
    // 统一到 10 s，缺失轨补静音
    let mut seg: Vec<Vec<f32>> = Vec::with_capacity(4);
    for p in planes {
        if p.len() >= 2 {
            seg.push(segment(&p[0]));
            seg.push(segment(&p[1]));
        } else {
            seg.push(vec![0.0; mel::SEGMENT_SAMPLES]);
            seg.push(vec![0.0; mel::SEGMENT_SAMPLES]);
        }
    }
    while seg.len() < 4 {
        seg.push(vec![0.0; mel::SEGMENT_SAMPLES]);
    }
    let stems: Vec<&[Vec<f32>]> = seg.chunks(2).map(|c| c).collect();
    let mel_data = mel::stack_mel(&model.bank, &stems);

    let named: Vec<(&str, &[Vec<f32>])> = Stem::all()
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let slice: &[Vec<f32>] = if i * 2 + 2 <= seg.len() {
                &seg[i * 2..i * 2 + 2]
            } else {
                &seg[0..2]
            };
            (s.name(), slice)
        })
        .collect();
    let feats = mel::mixing_features(&model.bank, &named);
    model.embed(mel_data, feats)
}

fn segment(x: &[f32]) -> Vec<f32> {
    let mut out = vec![0.0f32; mel::SEGMENT_SAMPLES];
    let n = x.len().min(mel::SEGMENT_SAMPLES);
    out[..n].copy_from_slice(&x[..n]);
    out
}

fn to_441_stereo(buf: &AudioBuffer) -> Vec<Vec<f32>> {
    to_stereo(&resample_buffer(buf, mel::SAMPLE_RATE)).to_planar()
}
