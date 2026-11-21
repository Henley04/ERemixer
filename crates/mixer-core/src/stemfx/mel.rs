//! Mel 频谱与混音特征提取 —— 与 StemFX 的 `MixingFeatureExtractor` 数值对齐。
//!
//! 采样率固定 44.1 kHz、n_fft=2048、hop=512、n_mels=80（与论文 checkpoint 一致）。
//! mel 滤波器组不从代码生成，而是直接使用 checkpoint 里的 `mel_scale.fb`（导出为
//! `mel_fb.bin`），因此与训练时的 torchaudio 实现逐位一致。

use rustfft::{num_complex::Complex, FftPlanner};

pub const SAMPLE_RATE: u32 = 44_100;
pub const N_FFT: usize = 2048;
pub const HOP: usize = 512;
pub const N_MELS: usize = 80;
pub const SEGMENT_SAMPLES: usize = 441_000; // 10 s
pub const FRAMES: usize = SEGMENT_SAMPLES / HOP + 1; // 862

/// mel 滤波器组 + Hann 窗。
pub struct MelBank {
    fb: Vec<f32>, // (N_FFT/2+1) x N_MELS，行优先
    window: Vec<f32>,
}

impl MelBank {
    pub fn from_bytes(bytes: &[u8]) -> Self {
        let bins = N_FFT / 2 + 1;
        assert_eq!(bytes.len(), bins * N_MELS * 4, "mel_fb.bin 尺寸不符");
        let fb = bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        // torch.hann_window(N_FFT, periodic=True)
        let window = (0..N_FFT)
            .map(|n| {
                let x = std::f32::consts::TAU * n as f32 / N_FFT as f32;
                0.5 - 0.5 * x.cos()
            })
            .collect();
        Self { fb, window }
    }

    /// 单声道功率谱 → mel 能量，输出 (N_MELS, frames) 行优先。
    pub fn mel_power(&self, x: &[f32]) -> Vec<f32> {
        let power = self.stft_power(x);
        self.apply_fb(&power)
    }

    /// |rfft|^2，形状 (frames, N_FFT/2+1)，行优先。center=True + reflect padding。
    pub fn stft_power(&self, x: &[f32]) -> Vec<f32> {
        let bins = N_FFT / 2 + 1;
        let pad = N_FFT / 2;
        let padded_len = x.len() + 2 * pad;
        let frames = if padded_len >= N_FFT { 1 + (padded_len - N_FFT) / HOP } else { 0 };

        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(N_FFT);
        let mut buf = vec![Complex::new(0.0f32, 0.0f32); N_FFT];
        let mut out = vec![0.0f32; frames * bins];
        let n = x.len() as isize;

        for f in 0..frames {
            let start = (f * HOP) as isize - pad as isize;
            for i in 0..N_FFT {
                let idx = reflect_index(start + i as isize, n);
                buf[i] = Complex::new(x[idx as usize] * self.window[i], 0.0);
            }
            fft.process(&mut buf);
            let row = &mut out[f * bins..(f + 1) * bins];
            for (b, c) in buf.iter().take(bins).enumerate() {
                row[b] = c.re * c.re + c.im * c.im;
            }
        }
        out
    }

    fn apply_fb(&self, power: &[f32]) -> Vec<f32> {
        let bins = N_FFT / 2 + 1;
        let frames = if power.is_empty() { 0 } else { power.len() / bins };
        let mut mel = vec![0.0f64; N_MELS * frames];
        for f in 0..frames {
            let prow = &power[f * bins..(f + 1) * bins];
            for m in 0..N_MELS {
                let mut acc = 0.0f64;
                for b in 0..bins {
                    acc += prow[b] as f64 * self.fb[b * N_MELS + m] as f64;
                }
                mel[m * frames + f] = acc;
            }
        }
        mel.into_iter().map(|v| v as f32).collect()
    }
}

fn reflect_index(i: isize, n: isize) -> isize {
    if n <= 1 {
        return 0;
    }
    if i < 0 {
        (-i).min(n - 1)
    } else if i >= n {
        (2 * (n - 1) - i).max(0)
    } else {
        i
    }
}

/// 从 4 轨立体声生成模型输入 (8, N_MELS, FRAMES)。
///
/// `stems` 顺序固定 vocals/bass/drums/other，每项为 (2, T) 平面数据。
pub fn stack_mel(bank: &MelBank, stems: &[&[Vec<f32>]]) -> Vec<f32> {
    let mut out = vec![0.0f32; 8 * N_MELS * FRAMES];
    let mut ch = 0;
    for stem in stems.iter().take(4) {
        for c in 0..2 {
            if let Some(p) = stem.get(c) {
                let mel = bank.mel_power(p);
                let frames = (mel.len() / N_MELS).min(FRAMES);
                for m in 0..N_MELS {
                    let base = (ch * N_MELS + m) * FRAMES;
                    out[base..base + frames].copy_from_slice(&mel[m * frames..m * frames + frames]);
                }
            }
            ch += 1;
        }
    }
    out
}

/// 64 维混音特征 —— 复刻 `MixingFeatureExtractor.extract_all_features`。
///
/// 顺序按特征名字典序：`{bass,drums,other,vocals}_dynamics`、`..._rel_loudness`、
/// `..._spectral`、`..._stereo`，最后是 4 维 `masking`。
pub fn mixing_features(bank: &MelBank, stems: &[(&str, &[Vec<f32>])]) -> Vec<f32> {
    let mut named: Vec<(String, Vec<f32>)> = Vec::new();
    let n = stems[0].1[0].len();
    let mut mix = vec![0.0f32; n];
    for (_, planes) in stems {
        for p in planes.iter() {
            for (i, v) in p.iter().enumerate() {
                mix[i] += *v;
            }
        }
    }
    let mix_loud = loudness(&mix);

    // 每轨每声道的 mel（(N_MELS, frames)），供频谱与掩蔽复用
    let mels: Vec<Vec<Vec<f32>>> = stems
        .iter()
        .map(|(_, planes)| planes.iter().map(|p| bank.mel_power(p)).collect())
        .collect();

    for (s, (name, planes)) in stems.iter().enumerate() {
        let l = &planes[0];
        let r = &planes[1];
        let rms_l = rms(l);
        let rms_r = rms(r);
        let crest_l = 20.0 * (peak(l) / (rms_l + 1e-8)).max(1e-12).log10();
        let crest_r = 20.0 * (peak(r) / (rms_r + 1e-8)).max(1e-12).log10();
        let loud = loudness(&[l.clone(), r.clone()].concat());
        named.push((
            format!("{}_dynamics", name),
            vec![rms_l, rms_r, crest_l, crest_r, loud, loud],
        ));
        named.push((format!("{}_rel_loudness", name), vec![loud - mix_loud]));

        // 频谱：mel dB 在声道/时间上求平均 → 低/中/高频段 + 倾斜 + 平坦度
        let mut acc = vec![0.0f64; N_MELS];
        let (mut log_sum, mut lin_sum, mut count) = (0.0f64, 0.0f64, 0usize);
        for mel in &mels[s] {
            let frames = mel.len() / N_MELS;
            for (i, v) in mel.iter().enumerate() {
                acc[i % N_MELS] += 10.0 * ((*v as f64) + 1e-10).log10();
                log_sum += ((*v as f64) + 1e-10).ln();
                lin_sum += *v as f64;
                count += 1;
            }
            let _ = frames;
        }
        let denom = (count / N_MELS).max(1) as f64;
        let mel_mean: Vec<f32> = acc.iter().map(|v| (v / denom) as f32).collect();
        let q1 = N_MELS / 4;
        let q3 = N_MELS / 4 * 3;
        let low = mean(&mel_mean[..q1]);
        let mid = mean(&mel_mean[q1..q3]);
        let high = mean(&mel_mean[q3..]);
        let tilt = corr(&mel_mean, None);
        let flat = (log_sum / count as f64).exp() / (lin_sum / count as f64 + 1e-10);
        named.push((
            format!("{}_spectral", name),
            vec![low, mid, high, tilt, flat as f32],
        ));

        // 立体声：ILD / 相关系数 / Mid-Side 能量比
        let ild = 20.0 * (rms_l / (rms_r + 1e-8)).max(1e-12).log10();
        let (lc, rc) = (center(l), center(r));
        let corr_v = dot(&lc, &rc) / ((dot(&lc, &lc) * dot(&rc, &rc)).sqrt() + 1e-8);
        let mid_v: Vec<f32> = l.iter().zip(r).map(|(a, b)| (a + b) * 0.5).collect();
        let side_v: Vec<f32> = l.iter().zip(r).map(|(a, b)| (a - b) * 0.5).collect();
        let msr = mean_sq(&side_v) / (mean_sq(&mid_v) + 1e-8);
        named.push((format!("{}_stereo", name), vec![ild, corr_v, msr]));
    }

    // 掩蔽：dominance = 本轨 mel - 其他轨最大 mel，掩蔽度 = sigmoid(-dominance)
    let mut masking = vec![0.0f32; 4];
    for i in 0..4 {
        let mut sum = 0.0f64;
        let frames = mels[i][0].len() / N_MELS;
        let mut n_terms = 0usize;
        for m in 0..N_MELS {
            for t in 0..frames {
                let own = (mels[i][0][m * frames + t] + mels[i][1][m * frames + t]) * 0.5;
                let mut other = f32::NEG_INFINITY;
                for (j, mel) in mels.iter().enumerate() {
                    if j != i {
                        let v = (mel[0][m * frames + t] + mel[1][m * frames + t]) * 0.5;
                        other = other.max(v);
                    }
                }
                sum += stable_sigmoid(-(own - other)) as f64;
                n_terms += 1;
            }
        }
        masking[i] = (sum / n_terms.max(1) as f64) as f32;
    }
    named.push(("masking".to_string(), masking));

    named.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out: Vec<f32> = Vec::with_capacity(64);
    for (_, v) in named {
        out.extend(v);
    }
    out.into_iter()
        .map(|v| if v.is_nan() { 0.0 } else { v.clamp(-100.0, 100.0) })
        .collect()
}

fn stable_sigmoid(x: f32) -> f32 {
    if x >= 0.0 {
        1.0 / (1.0 + (-x).exp())
    } else {
        let e = x.exp();
        e / (1.0 + e)
    }
}

fn rms(x: &[f32]) -> f32 {
    (x.iter().map(|v| (*v as f64) * (*v as f64)).sum::<f64>() / x.len().max(1) as f64).sqrt() as f32
}
fn peak(x: &[f32]) -> f32 {
    x.iter().fold(0.0f32, |a, b| a.max(b.abs()))
}
fn mean(x: &[f32]) -> f32 {
    x.iter().sum::<f32>() / x.len().max(1) as f32
}
fn mean_sq(x: &[f32]) -> f32 {
    x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32
}
fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}
fn center(x: &[f32]) -> Vec<f32> {
    let m = mean(x);
    x.iter().map(|v| v - m).collect()
}
fn corr(x: &[f32], _unused: Option<()>) -> f32 {
    let m = mean(x);
    let var = x.iter().map(|v| (v - m) * (v - m)).sum::<f32>();
    if var.sqrt() < 1e-6 {
        return 0.0;
    }
    let fx: Vec<f32> = (0..x.len()).map(|i| i as f32).collect();
    let (a, b) = (center(&fx), center(x));
    dot(&a, &b) / ((dot(&a, &a) * dot(&b, &b)).sqrt() + 1e-12)
}
fn loudness(x: &[f32]) -> f32 {
    -0.691 + 10.0 * (mean_sq(x) + 1e-10).log10()
}
