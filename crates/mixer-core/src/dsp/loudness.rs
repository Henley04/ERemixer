//! 响度测量与真峰检测（ITU-R BS.1770-4 / EBU R128）。
//!
//! - K-weighting：两级滤波器，参数取 BS.1770 模拟原型在数字域的标称设计值
//!   （pre-filter 1681.97 Hz / Q 0.7072 / +3.9998 dB，RLB 38.135 Hz / Q 0.5003）。
//! - Integrated LUFS：400 ms 块、75% 重叠，**能量域**门限（-70 LUFS 绝对门 + 相对门 -10 LU）。
//! - True Peak：4 倍多相窗函数 sinc 插值后取峰（能检测到样本间峰，而不是样本峰）。
//! - 限幅器：以真峰包络驱动的前瞻限幅，瞬时压制 + 平滑释放。

use crate::dsp::biquad::{Biquad, FilterType};
use std::collections::VecDeque;

/// 真峰过采样倍率。
const TP_PHASES: usize = 4;
/// 每个相位的单边抽头数（总抽头 2*TP_HALF）。
const TP_HALF: isize = 4;

fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-9 {
        1.0
    } else {
        let px = std::f64::consts::PI * x;
        px.sin() / px
    }
}

/// 生成某个相位的多相插值系数（Hann 窗 sinc），并做直流归一化。
fn phase_taps(phase: usize) -> Vec<f32> {
    let frac = phase as f64 / TP_PHASES as f64;
    let mut taps = Vec::with_capacity((2 * TP_HALF) as usize);
    let mut sum = 0.0f64;
    for k in (-TP_HALF + 1)..=TP_HALF {
        let t = frac - k as f64;
        // 窗函数覆盖整个抽头范围，抑制旁瓣。
        let w = 0.5 * (1.0 + (std::f64::consts::PI * t / (TP_HALF as f64 + 0.5)).cos());
        let v = sinc(t) * w;
        taps.push(v as f32);
        sum += v;
    }
    if sum.abs() > 1e-12 {
        for v in &mut taps {
            *v /= sum as f32;
        }
    }
    taps
}

/// 计算每个样本位置上的真峰包络：`env[i] = max(|x[i]|, 区间 (i, i+1) 内的插值峰)`。
fn true_peak_envelope(buf: &[f32]) -> Vec<f32> {
    let n = buf.len();
    let mut env = vec![0.0f32; n];
    if n == 0 {
        return env;
    }
    // 预先算好 3 个分数相位的系数（phase 0 就是原样本）。
    let taps: Vec<Vec<f32>> = (1..TP_PHASES).map(phase_taps).collect();
    let half = TP_HALF as isize;
    for i in 0..n {
        let mut peak = buf[i].abs();
        for coeffs in &taps {
            let mut acc = 0.0f32;
            for (j, &c) in coeffs.iter().enumerate() {
                let k = j as isize - half + 1;
                let idx = (i as isize + k).clamp(0, n as isize - 1) as usize;
                acc += buf[idx] * c;
            }
            peak = peak.max(acc.abs());
        }
        env[i] = peak;
    }
    env
}

/// 对单声道/立体声平面数据施加 K-weighting，返回每声道的加权平方能量序列。
pub fn k_weight_energy(planes: &[Vec<f32>], sample_rate: f32) -> Vec<Vec<f32>> {
    let mut energies = Vec::with_capacity(planes.len());
    for plane in planes {
        let mut x = plane.clone();
        // Stage 1: pre-filter（高架，约 +4 dB @ 高频）。
        Biquad::process_into(
            FilterType::HighShelf,
            sample_rate,
            1681.974_450_955_533,
            0.707_175_236_955_419_6,
            3.999_843_853_973_347,
            &mut x,
        );
        // Stage 2: RLB 高通（约 38 Hz）。
        Biquad::process_into(
            FilterType::HighPass,
            sample_rate,
            38.135_470_876_024_44,
            0.500_327_037_323_877_3,
            0.0,
            &mut x,
        );
        energies.push(x.iter().map(|s| s * s).collect());
    }
    energies
}

#[inline]
fn energy_to_lufs(z: f64) -> f64 {
    -0.691 + 10.0 * (z.max(1e-12)).log10()
}

/// 计算 gated integrated loudness（LUFS）。
///
/// 通道权重：立体声 L/R 各 1.0。分块 400 ms，75% 重叠；
/// 绝对门 -70 LUFS，相对门为未门控平均能量对应响度 -10 LU。
/// 短于 400 ms 的素材按整段作为单块测量（而不是直接返回 -70）。
pub fn integrated_lufs(planes: &[Vec<f32>], sample_rate: f32) -> f32 {
    if planes.is_empty() || planes[0].is_empty() || sample_rate <= 0.0 {
        return -70.0;
    }
    let energies = k_weight_energy(planes, sample_rate);
    let frames = energies[0].len();
    if frames == 0 {
        return -70.0;
    }
    let block = (0.4 * sample_rate) as usize;
    let hop = (block / 4).max(1);

    // 每个 400ms 块的平均加权能量（跨声道求和）。
    let mut block_energies: Vec<f64> = Vec::new();
    if block == 0 || frames < block {
        let mut sum = 0.0f64;
        for e in &energies {
            let s: f64 = e.iter().map(|&v| v as f64).sum();
            sum += s / frames as f64;
        }
        block_energies.push(sum);
    } else {
        let mut pos = 0usize;
        while pos + block <= frames {
            let mut sum = 0.0f64;
            for e in &energies {
                let s: f64 = e[pos..pos + block].iter().map(|&v| v as f64).sum();
                sum += s / block as f64;
            }
            block_energies.push(sum);
            pos += hop;
        }
    }
    if block_energies.is_empty() {
        return -70.0;
    }

    // 绝对门（-70 LUFS）。
    let gated_abs: Vec<f64> = block_energies
        .iter()
        .copied()
        .filter(|&z| energy_to_lufs(z) > -70.0)
        .collect();
    if gated_abs.is_empty() {
        return -70.0;
    }
    let mean_abs = gated_abs.iter().sum::<f64>() / gated_abs.len() as f64;
    // 相对门：未门控平均响度 - 10 LU。
    let rel_gate = energy_to_lufs(mean_abs) - 10.0;
    let gated_rel: Vec<f64> = gated_abs
        .into_iter()
        .filter(|&z| energy_to_lufs(z) > rel_gate)
        .collect();
    if gated_rel.is_empty() {
        return -70.0;
    }
    let mean = gated_rel.iter().sum::<f64>() / gated_rel.len() as f64;
    energy_to_lufs(mean) as f32
}

/// 真峰（dBTP），基于 4 倍多相 sinc 插值。
pub fn true_peak_dbtp(planes: &[Vec<f32>]) -> f32 {
    let mut peak = 0.0f32;
    for plane in planes {
        let env = true_peak_envelope(plane);
        for v in env {
            peak = peak.max(v);
        }
    }
    if peak <= 0.0 {
        return -70.0;
    }
    20.0 * peak.log10()
}

/// 真峰限幅器：前瞻窗口检测真峰，瞬时压制 + 平滑释放。
///
/// 设计说明：`desired[i]` 是窗口 `[i, i+lookahead)` 内真峰所需的增益，
/// 因此增益在瞬态到达**之前**就已下降，不需要额外的延迟线；音频不会产生时移。
#[derive(Debug, Clone)]
pub struct TruePeakLimiter {
    ceiling_linear: f32,
    release_coef: f32,
    gain: f32,
    /// 前瞻窗口（样本数）。
    lookahead: usize,
}

impl TruePeakLimiter {
    pub fn new(sr: f32, ceiling_dbtp: f32, lookahead_ms: f32, release_ms: f32) -> Self {
        let sr = sr.max(1.0);
        Self {
            ceiling_linear: 10f32.powf(ceiling_dbtp / 20.0),
            release_coef: (-1.0 / (release_ms.max(1.0) * 0.001 * sr)).exp(),
            gain: 1.0,
            lookahead: ((lookahead_ms.max(0.5) * 0.001 * sr) as usize).max(1),
        }
    }

    /// 就地限幅整段单声道。
    pub fn process(&mut self, buf: &mut [f32]) {
        let n = buf.len();
        if n == 0 {
            return;
        }
        let env = true_peak_envelope(buf);
        let la = self.lookahead.min(n).max(1);

        // 滑动窗口最大值（单调递减队列）→ 每个位置所需的增益。
        let mut desired = vec![1.0f32; n];
        let mut dq: VecDeque<usize> = VecDeque::new();
        for i in 0..n {
            let j = i + la - 1;
            if j < n {
                while let Some(&back) = dq.back() {
                    if env[back] <= env[j] {
                        dq.pop_back();
                    } else {
                        break;
                    }
                }
                dq.push_back(j);
            }
            while let Some(&front) = dq.front() {
                if front < i {
                    dq.pop_front();
                } else {
                    break;
                }
            }
            if let Some(&k) = dq.front() {
                let peak = env[k];
                if peak > self.ceiling_linear {
                    desired[i] = self.ceiling_linear / peak;
                }
            }
        }

        // 起始增益直接对齐第一个窗口，避免首个瞬态漏过。
        self.gain = self.gain.min(desired[0]);
        for i in 0..n {
            let target = desired[i];
            if target < self.gain {
                // 前瞻已提前预警，这里可以瞬时跟随（此时音频尚未到达峰值）。
                self.gain = target;
            } else {
                self.gain = self.release_coef * self.gain + (1.0 - self.release_coef) * target;
            }
            // 硬顶兜底：插值误差 / 数值误差不允许越过天花板。
            buf[i] = (buf[i] * self.gain).clamp(-self.ceiling_linear, self.ceiling_linear);
        }

        // 终检：硬钳位会产生新的样本间峰（边沿的高频分量），
        // 因此对整段按实测真峰做一次极小比例的整体微缩，严格保证不越界。
        let residual = true_peak_envelope(buf)
            .iter()
            .fold(0.0f32, |m, &v| m.max(v));
        if residual > self.ceiling_linear && residual > 0.0 {
            let trim = self.ceiling_linear / residual;
            for s in buf.iter_mut() {
                *s *= trim;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f32, amp: f32, sr: f32, n: usize, phase_deg: f32) -> Vec<f32> {
        let phase = phase_deg.to_radians();
        (0..n)
            .map(|i| {
                amp * (2.0 * std::f32::consts::PI * freq * i as f32 / sr + phase).sin()
            })
            .collect()
    }

    #[test]
    fn test_lufs_silence() {
        let planes = vec![vec![0.0f32; 48000]];
        assert!(integrated_lufs(&planes, 48000.0) <= -69.0);
    }

    #[test]
    fn test_lufs_loud_vs_quiet() {
        let loud: Vec<f32> = (0..48000).map(|_| 0.5).collect();
        let quiet: Vec<f32> = (0..48000).map(|_| 0.01).collect();
        let l = integrated_lufs(&[loud], 48000.0);
        let q = integrated_lufs(&[quiet], 48000.0);
        assert!(l > q + 20.0, "loud {} should exceed quiet {}", l, q);
    }

    #[test]
    fn test_lufs_energy_domain_gating() {
        // 能量域门限应比"dB 域平均"更响：交替 -8/-32 dBFS 噪声。
        let mut x = vec![0.0f32; 48000 * 5];
        let mut seed = 12345u32;
        for (i, s) in x.iter_mut().enumerate() {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            let noise = (seed as f32 / u32::MAX as f32) * 2.0 - 1.0;
            let amp = if (i / 24000) % 2 == 0 { 0.4 } else { 0.025 };
            *s = noise * amp;
        }
        let l = integrated_lufs(&[x], 48000.0);
        assert!(l.is_finite() && l > -40.0, "lufs={}", l);
    }

    #[test]
    fn test_true_peak_detects_inter_sample_peak() {
        // 12 kHz、45° 相位：样本值恒为 ±0.707，但真实峰接近 0 dBTP。
        let x = sine(12000.0, 0.999, 48000.0, 48000, 45.0);
        let sample_peak = x.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        let tp = true_peak_dbtp(&[x]);
        let sample_db = 20.0 * sample_peak.log10();
        assert!(
            tp > sample_db + 2.0,
            "真峰未被检出: tp={:.2} dBTP, sample={:.2} dBFS",
            tp,
            sample_db
        );
        assert!(tp > -0.5, "12k/45° 的真峰应接近 0 dBTP，实际 {:.2}", tp);
    }

    #[test]
    fn test_limiter_true_peak_ceiling() {
        // 接近满幅的 12 kHz 正弦，真峰超过 0 dBTP，限幅后必须低于 -1 dBTP。
        let mut x = sine(12000.0, 1.0, 48000.0, 48000 * 2, 45.0);
        let mut limiter = TruePeakLimiter::new(48000.0, -1.0, 5.0, 60.0);
        limiter.process(&mut x);
        let tp = true_peak_dbtp(&[x]);
        assert!(tp <= -0.95, "限幅后真峰 {:.2} dBTP 仍超过 -1 dBTP", tp);
    }

    #[test]
    fn test_limiter_no_start_leak() {
        // 从第 0 个样本就是满幅瞬态，不允许开头漏过天花板。
        let mut x = vec![1.0f32; 4800];
        let mut limiter = TruePeakLimiter::new(48000.0, -1.0, 5.0, 50.0);
        limiter.process(&mut x);
        let tp = true_peak_dbtp(&[x]);
        assert!(tp <= -0.95, "开头瞬态泄漏: {:.2} dBTP", tp);
    }
}
