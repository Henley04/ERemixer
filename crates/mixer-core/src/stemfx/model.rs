//! StemFX 的 ONNX 推理封装（W8A32：int8 权重 + fp32 激活）。
//!
//! 模型默认**内嵌进二进制**（`include_bytes!`），无需外部文件即可运行；
//! 若 `models/stemfx/` 下有同名文件则优先使用外部文件（便于替换权重）。

use crate::error::{MixError, Result};
use crate::stemfx::mel::{MelBank, FRAMES, N_MELS};
use ort::session::Session;
use ort::value::Tensor;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

const ENCODER_ONNX: &[u8] = include_bytes!("../../../../models/stemfx/encoder_w8a32.onnx");
const GENERATOR_ONNX: &[u8] = include_bytes!("../../../../models/stemfx/generator_w8a32.onnx");
const MEL_FB: &[u8] = include_bytes!("../../../../models/stemfx/mel_fb.bin");
const META_JSON: &str = include_str!("../../../../models/stemfx/meta.json");

#[derive(Debug, Clone, Deserialize)]
pub struct ParamRange {
    pub min: f32,
    pub max: f32,
    #[serde(default)]
    pub log: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Meta {
    pub sample_rate: u32,
    pub n_fft: usize,
    pub hop_length: usize,
    pub n_mels: usize,
    pub n_subbands: usize,
    pub encoder_dim: usize,
    pub vocab_size: usize,
    pub value_token_start: usize,
    pub num_value_bins: usize,
    pub stems: Vec<String>,
    pub tokens: Vec<String>,
    #[serde(default)]
    pub fx_types: Vec<String>,
    pub params: HashMap<String, ParamRange>,
}

impl Meta {
    pub fn token(&self, id: usize) -> Option<&str> {
        self.tokens.get(id).map(|s| s.as_str())
    }
    /// VALUE_xx 归一化值 → 真实参数值。
    pub fn denormalize(&self, key: &str, norm: f32) -> Option<f32> {
        let r = self.params.get(key)?;
        let n = norm.clamp(0.0, 1.0);
        if r.log {
            let lo = r.min.max(1e-6).ln();
            let hi = r.max.max(r.min + 1e-6).ln();
            Some((lo + (hi - lo) * n).exp())
        } else {
            Some(r.min + (r.max - r.min) * n)
        }
    }
    pub fn is_value_token(&self, id: usize) -> bool {
        id >= self.value_token_start && id < self.value_token_start + self.num_value_bins
    }
}

pub struct StemFxModel {
    enc: Session,
    gen: Session,
    pub meta: Meta,
    pub bank: MelBank,
}

impl StemFxModel {
    /// 加载模型：优先 `models_dir/stemfx/*.onnx`，否则使用内嵌权重。
    pub fn load(models_dir: Option<&str>) -> Result<Self> {
        let env = crate::ort_env::env(models_dir)?;

        let dir = models_dir.map(|d| Path::new(d).join("stemfx"));
        let read = |name: &str, embedded: &'static [u8]| -> Vec<u8> {
            dir.as_ref()
                .map(|d| d.join(name))
                .filter(|p| p.exists())
                .and_then(|p| std::fs::read(p).ok())
                .unwrap_or_else(|| embedded.to_vec())
        };

        let enc_bytes = read("encoder_w8a32.onnx", ENCODER_ONNX);
        let gen_bytes = read("generator_w8a32.onnx", GENERATOR_ONNX);
        let fb_bytes = read("mel_fb.bin", MEL_FB);
        let meta: Meta = serde_json::from_str(META_JSON)
            .map_err(|e| MixError::Inference(format!("meta.json 解析失败: {}", e)))?;

        let mut b = Session::builder(&env)
            .map_err(|e| MixError::Inference(format!("会话构建器失败: {}", e)))?;
        let enc = b
            .commit_from_memory(&enc_bytes)
            .map_err(|e| MixError::Inference(format!("encoder 加载失败: {}", e)))?;
        let mut b = Session::builder(&env)
            .map_err(|e| MixError::Inference(format!("会话构建器失败: {}", e)))?;
        let gen = b
            .commit_from_memory(&gen_bytes)
            .map_err(|e| MixError::Inference(format!("generator 加载失败: {}", e)))?;

        Ok(Self {
            enc,
            gen,
            meta,
            bank: MelBank::from_bytes(&fb_bytes),
        })
    }

    /// mel(8,80,862) + 特征(64) → L2 归一化嵌入(512)。
    pub fn embed(&mut self, mel: Vec<f32>, feats: Vec<f32>) -> Result<Vec<f32>> {
        debug_assert_eq!(mel.len(), 8 * N_MELS * FRAMES);
        let mel_t = Tensor::from_array(([1usize, 8, N_MELS, FRAMES], mel))
            .map_err(|e| MixError::Inference(format!("mel 张量构造失败: {}", e)))?;
        let f_t = Tensor::from_array(([1usize, 64usize], feats))
            .map_err(|e| MixError::Inference(format!("特征张量构造失败: {}", e)))?;
        let out = self
            .enc
            .run({
                let inputs: Vec<(&str, ort::session::SessionInputValue)> =
                    vec![("mel", mel_t.into()), ("features", f_t.into())];
                inputs
            })
            .map_err(|e| MixError::Inference(format!("encoder 推理失败: {}", e)))?;
        let (_, data) = out[0]
            .try_extract_tensor::<f32>()
            .map_err(|e| MixError::Inference(format!("encoder 输出解析失败: {}", e)))?;
        Ok(data.to_vec())
    }

    /// 自回归贪心解码（与论文 `api._predict_chain` 一致）。
    pub fn predict_tokens(
        &mut self,
        emb_orig: &[f32],
        emb_target: &[f32],
        max_tokens: usize,
        cancel: &dyn Fn() -> bool,
    ) -> Result<Vec<i64>> {
        let mut tokens: Vec<i64> = vec![1];
        for _ in 0..max_tokens {
            if cancel() {
                return Err(MixError::Cancelled);
            }
            let toks = Tensor::from_array(([1usize, tokens.len()], tokens.clone()))
                .map_err(|e| MixError::Inference(format!("token 张量构造失败: {}", e)))?;
            let e1 = Tensor::from_array(([1usize, 512usize], emb_orig.to_vec()))
                .map_err(|e| MixError::Inference(format!("嵌入张量构造失败: {}", e)))?;
            let e2 = Tensor::from_array(([1usize, 512usize], emb_target.to_vec()))
                .map_err(|e| MixError::Inference(format!("嵌入张量构造失败: {}", e)))?;
            let out = self
                .gen
                .run({
                    let inputs: Vec<(&str, ort::session::SessionInputValue)> = vec![
                        ("emb_orig", e1.into()),
                        ("emb_target", e2.into()),
                        ("tokens", toks.into()),
                    ];
                    inputs
                })
                .map_err(|e| MixError::Inference(format!("generator 推理失败: {}", e)))?;
            let (_, logits) = out[0]
                .try_extract_tensor::<f32>()
                .map_err(|e| MixError::Inference(format!("generator 输出解析失败: {}", e)))?;
            let mut best = 0usize;
            let mut best_v = f32::NEG_INFINITY;
            for (i, v) in logits.iter().enumerate() {
                if *v > best_v {
                    best_v = *v;
                    best = i;
                }
            }
            tokens.push(best as i64);
            if best == 2 {
                break;
            }
        }
        Ok(tokens)
    }
}
