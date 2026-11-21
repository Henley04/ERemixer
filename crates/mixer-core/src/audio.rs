//! 音频读写与基础变换：WAV 解码/编码、线性插值重采样、声道归一化、时间对齐。

use crate::error::{MixError, Result};
use std::path::Path;

/// 解码后的音频：交错（interleaved）f32 采样 + 元数据。
#[derive(Debug, Clone)]
pub struct AudioBuffer {
    /// 交错采样：L,R,L,R,...（单声道则为单序列）。
    pub samples: Vec<f32>,
    pub channels: u16,
    pub sample_rate: u32,
}

impl AudioBuffer {
    pub fn frames(&self) -> usize {
        self.samples.len() / self.channels as usize
    }
    pub fn duration_sec(&self) -> f32 {
        self.frames() as f32 / self.sample_rate as f32
    }
    /// 去交错为逐声道平面数据。
    pub fn to_planar(&self) -> Vec<Vec<f32>> {
        let ch = self.channels as usize;
        let mut planes = vec![Vec::with_capacity(self.frames()); ch];
        for frame in self.samples.chunks_exact(ch) {
            for (c, s) in frame.iter().enumerate() {
                planes[c].push(*s);
            }
        }
        planes
    }
    /// 从逐声道平面数据构造（交错化）。
    pub fn from_planar(planes: Vec<Vec<f32>>, sample_rate: u32) -> Self {
        let channels = planes.len();
        let frames = planes.first().map(|p| p.len()).unwrap_or(0);
        let mut samples = Vec::with_capacity(frames * channels);
        for i in 0..frames {
            for p in &planes {
                samples.push(p[i]);
            }
        }
        Self {
            samples,
            channels: channels as u16,
            sample_rate,
        }
    }
    /// 单声道化（取声道平均）。
    pub fn downmix_mono(&self) -> Vec<f32> {
        let planes = self.to_planar();
        let frames = self.frames();
        (0..frames)
            .map(|i| planes.iter().map(|p| p[i]).sum::<f32>() / planes.len() as f32)
            .collect()
    }
}

/// 读取 WAV 为 f32 交错数据（自动处理 PCM16/24/32 与 IEEE float）。
pub fn read_wav(path: &str) -> Result<AudioBuffer> {
    let mut reader = hound::WavReader::open(Path::new(path))
        .map_err(|e| MixError::Wav(format!("无法打开 {}: {}", path, e)))?;
    let spec = reader.spec();
    if spec.channels == 0 || spec.channels > 8 {
        return Err(MixError::UnsupportedChannels(spec.channels as usize));
    }
    let sr = spec.sample_rate;
    if !(8000..=768000).contains(&sr) {
        return Err(MixError::UnsupportedSampleRate(sr));
    }

    let channels = spec.channels as usize;
    let mut samples: Vec<f32> = Vec::new();
    match spec.sample_format {
        hound::SampleFormat::Float => {
            for s in reader.samples::<f32>() {
                samples.push(s.map_err(|e| MixError::Wav(e.to_string()))?);
            }
        }
        hound::SampleFormat::Int => {
            let bits = spec.bits_per_sample;
            let max = (1u64 << (bits.saturating_sub(1))) as f32;
            for s in reader.samples::<i32>() {
                let v = s.map_err(|e| MixError::Wav(e.to_string()))?;
                samples.push(v as f32 / max);
            }
        }
    }
    // 若样本数不是声道整数倍，截断到整帧。
    let frames = samples.len() / channels;
    samples.truncate(frames * channels);
    Ok(AudioBuffer {
        samples,
        channels: spec.channels,
        sample_rate: sr,
    })
}

/// 写入 WAV。bit_depth: 16/24 为 PCM，32 为 IEEE float。
pub fn write_wav(path: &str, buf: &AudioBuffer, bit_depth: u16) -> Result<()> {
    if let Some(parent) = Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let sample_format = if bit_depth == 32 {
        hound::SampleFormat::Float
    } else {
        hound::SampleFormat::Int
    };
    let spec = hound::WavSpec {
        channels: buf.channels,
        sample_rate: buf.sample_rate,
        bits_per_sample: bit_depth,
        sample_format,
    };
    let mut writer = hound::WavWriter::create(Path::new(path), spec)
        .map_err(|e| MixError::Wav(format!("无法创建 {}: {}", path, e)))?;
    match sample_format {
        hound::SampleFormat::Float => {
            for &s in &buf.samples {
                writer
                    .write_sample(s)
                    .map_err(|e| MixError::Wav(e.to_string()))?;
            }
        }
        hound::SampleFormat::Int => {
            let bits = bit_depth;
            let max = (1u64 << (bits - 1)) as f32 - 1.0;
            let min = -((1u64 << (bits - 1)) as f32);
            for &s in &buf.samples {
                let clamped = s.clamp(min / (max + 1.0), 1.0);
                let v = (clamped * max).round() as i32;
                writer
                    .write_sample(v)
                    .map_err(|e| MixError::Wav(e.to_string()))?;
            }
        }
    }
    writer
        .finalize()
        .map_err(|e| MixError::Wav(e.to_string()))?;
    Ok(())
}

/// 线性插值重采样（重采样质量对混音影响有限，线性已足够且零依赖）。
pub fn resample_linear(input: &[f32], from_sr: u32, to_sr: u32) -> Vec<f32> {
    if from_sr == to_sr {
        return input.to_vec();
    }
    let ratio = to_sr as f64 / from_sr as f64;
    let out_len = ((input.len() as f64) * ratio).round() as usize;
    if out_len == 0 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src_pos = i as f64 / ratio;
        let i0 = src_pos.floor() as usize;
        let frac = (src_pos - i0 as f64) as f32;
        let s0 = input.get(i0).copied().unwrap_or(0.0);
        let s1 = input.get(i0 + 1).copied().unwrap_or(s0);
        out.push(s0 + (s1 - s0) * frac);
    }
    out
}

/// 整段音频重采样（保持声道与交错结构）。
pub fn resample_buffer(buf: &AudioBuffer, to_sr: u32) -> AudioBuffer {
    if buf.sample_rate == to_sr {
        return buf.clone();
    }
    let planes = buf.to_planar();
    let new_planes: Vec<Vec<f32>> = planes
        .iter()
        .map(|p| resample_linear(p, buf.sample_rate, to_sr))
        .collect();
    AudioBuffer::from_planar(new_planes, to_sr)
}

/// 归一化到立体声：单声道复制到双声道；多声道按简化 ITU 系数下混。
pub fn to_stereo(buf: &AudioBuffer) -> AudioBuffer {
    if buf.channels == 2 {
        return buf.clone();
    }
    let planes = buf.to_planar();
    let (l, r) = if buf.channels == 1 {
        (planes[0].clone(), planes[0].clone())
    } else if buf.channels >= 2 {
        // WAV 5.1 顺序：FL, FR, FC, LFE, BL, BR。
        // 丢弃 LFE、中置与环绕按 0.707 并入，避免人声落在中置时整轨消失。
        let n = planes.iter().map(|p| p.len()).min().unwrap_or(0);
        let mut l = planes[0].clone();
        let mut r = planes[1].clone();
        for (c, p) in planes.iter().enumerate().skip(2) {
            let (wl, wr) = match c {
                2 => (0.707, 0.707), // FC
                3 => (0.0, 0.0),     // LFE
                4 => (0.707, 0.0),   // BL
                5 => (0.0, 0.707),   // BR
                _ => {
                    if c % 2 == 0 { (0.5, 0.0) } else { (0.0, 0.5) }
                }
            };
            for i in 0..n.min(p.len()) {
                l[i] += p[i] * wl;
                r[i] += p[i] * wr;
            }
        }
        (l, r)
    } else {
        unreachable!()
    };
    AudioBuffer::from_planar(vec![l, r], buf.sample_rate)
}

/// 将两段音频对齐到相同帧数（长截短、短补零）。
pub fn match_length(a: &mut AudioBuffer, b: &AudioBuffer) {
    let ch = a.channels.max(b.channels) as usize;
    let frames_a = a.frames();
    let frames_b = b.frames();
    let target = frames_a.max(frames_b);
    if frames_a < target || a.channels as usize != ch {
        let mut planes = a.to_planar();
        while planes.len() < ch {
            planes.push(vec![0.0; frames_a]);
        }
        for p in &mut planes {
            p.resize(target, 0.0);
        }
        *a = AudioBuffer::from_planar(planes, a.sample_rate);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resample_identity() {
        let x = vec![0.0, 0.5, 1.0, 0.5];
        let y = resample_linear(&x, 48000, 48000);
        assert_eq!(x, y);
    }

    #[test]
    fn test_resample_down_up() {
        let x: Vec<f32> = (0..100).map(|i| (i as f32 / 100.0).sin()).collect();
        let y = resample_linear(&x, 44100, 48000);
        assert!(y.len() > x.len());
        let z = resample_linear(&y, 48000, 44100);
        assert!((z.len() as i32 - x.len() as i32).abs() <= 2);
    }

    #[test]
    fn test_to_stereo_mono() {
        let buf = AudioBuffer {
            samples: vec![0.1, 0.2, 0.3],
            channels: 1,
            sample_rate: 48000,
        };
        let st = to_stereo(&buf);
        assert_eq!(st.channels, 2);
        assert_eq!(st.samples, vec![0.1, 0.1, 0.2, 0.2, 0.3, 0.3]);
    }
}
