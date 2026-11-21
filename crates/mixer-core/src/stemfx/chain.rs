//! 词表解码：token 序列 → 每轨效果器链（multiafx 格式）。
//!
//! 与 `FXChainTokenizer.decode_fx_chain` 完全等价：`[SEP]` 结束当前轨，
//! 轨名 token 开新轨，效果器 token 开新效果，参数名 token 后紧跟 VALUE token。

use crate::stemfx::model::Meta;
use serde::Serialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize)]
pub struct FxStep {
    pub effect: String,
    pub params: Vec<(String, f32)>,
}

impl FxStep {
    pub fn get(&self, name: &str) -> Option<f32> {
        self.params.iter().find(|(k, _)| k == name).map(|(_, v)| *v)
    }
    pub fn get_or(&self, name: &str, default: f32) -> f32 {
        self.get(name).unwrap_or(default)
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Chain {
    /// 固定 4 轨，顺序 vocals / bass / drums / other。
    pub stems: Vec<(String, Vec<FxStep>)>,
}

impl Chain {
    pub fn empty(meta: &Meta) -> Self {
        Self {
            stems: meta.stems.iter().map(|s| (s.clone(), Vec::new())).collect(),
        }
    }

    pub fn steps(&self, stem: &str) -> &[FxStep] {
        self.stems
            .iter()
            .find(|(s, _)| s == stem)
            .map(|(_, v)| v.as_slice())
            .unwrap_or(&[])
    }

    /// 单行可读摘要，用于 CLI / GUI 展示。
    pub fn pretty(&self) -> String {
        let mut out = String::new();
        for (stem, steps) in &self.stems {
            if steps.is_empty() {
                out.push_str(&format!("  {}: (无效果)\n", stem));
                continue;
            }
            let parts: Vec<String> = steps
                .iter()
                .map(|s| {
                    if s.params.is_empty() {
                        s.effect.clone()
                    } else {
                        let ps: Vec<String> =
                            s.params.iter().map(|(k, v)| format!("{}={:.3}", k, v)).collect();
                        format!("{}({})", s.effect, ps.join(", "))
                    }
                })
                .collect();
            out.push_str(&format!("  {}: {}\n", stem, parts.join(" → ")));
        }
        out
    }

    pub fn total_steps(&self) -> usize {
        self.stems.iter().map(|(_, v)| v.len()).sum()
    }
}

/// token 解码。返回链与逐 token 的可读形式（便于日志排查）。
pub fn decode(meta: &Meta, tokens: &[i64]) -> (Chain, Vec<String>) {
    let fx: std::collections::HashSet<&str> =
        meta.tokens.iter().skip(8).take(meta.tokens.len()).map(|s| s.as_str()).collect();
    let mut chain = Chain::empty(meta);
    let mut names = Vec::with_capacity(tokens.len());

    let mut cur_stem: Option<usize> = None;
    let mut cur_fx: Option<usize> = None;
    let mut cur_key: Option<String> = None;

    for (i, t) in tokens.iter().enumerate() {
        let id = *t as usize;
        let tok = meta.token(id).unwrap_or("");
        names.push(tok.to_string());
        if i == 0 && tok == "[BOS]" {
            continue;
        }
        match tok {
            "[EOS]" => break,
            "[SEP]" => {
                cur_stem = None;
                cur_fx = None;
                cur_key = None;
            }
            "[PAD]" | "[BOS]" => {}
            _ if meta.stems.iter().any(|s| s == tok) => {
                let idx = meta.stems.iter().position(|s| s == tok).unwrap();
                if chain.stems[idx].1.is_empty() {
                    cur_stem = Some(idx);
                } else {
                    cur_stem = Some(idx);
                }
                cur_fx = None;
                cur_key = None;
            }
            // 效果器名：不是任何参数键的前缀形式（参数键都形如 `效果_参数`）
            _ if meta.fx_types.iter().any(|f| f == tok) => {
                if let Some(si) = cur_stem {
                    chain.stems[si].1.push(FxStep {
                        effect: tok.to_string(),
                        params: Vec::new(),
                    });
                    cur_fx = Some(chain.stems[si].1.len() - 1);
                }
                cur_key = None;
            }
            _ => {
                // 参数名 token：`{effect}_{param}`
                if let Some(fi) = cur_fx {
                    let fx_name = chain.stems[cur_stem.unwrap()].1[fi].effect.clone();
                    if tok.starts_with(&format!("{}_", fx_name)) {
                        cur_key = Some(tok.to_string());
                        continue;
                    }
                }
                if meta.is_value_token(id) {
                    if let (Some(si), Some(fi), Some(key)) = (cur_stem, cur_fx, cur_key.take()) {
                        let fx_name = chain.stems[si].1[fi].effect.clone();
                        if let Some(pname) = key.strip_prefix(&format!("{}_", fx_name)) {
                            let n = (id - meta.value_token_start) as f32
                                / (meta.num_value_bins - 1) as f32;
                            if let Some(v) = meta.denormalize(&key, n) {
                                chain.stems[si].1[fi].params.push((pname.to_string(), v));
                            }
                        }
                    }
                }
            }
        }
    }
    let _ = fx;
    (chain, names)
}

/// 汇总链中出现的所有效果器名（用于告警/统计）。
pub fn effect_histogram(chain: &Chain) -> HashMap<String, usize> {
    let mut h = HashMap::new();
    for (_, steps) in &chain.stems {
        for s in steps {
            *h.entry(s.effect.clone()).or_insert(0) += 1;
        }
    }
    h
}
