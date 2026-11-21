//! 动态处理：前馈压缩器（RMS 检测 + attack/release）、侧链 ducking。
//! 全部为逐样本状态机，便于流式处理。

/// 压缩器参数。threshold/ratio/knee 采用 dB 域；attack/release 为毫秒。
#[derive(Debug, Clone, Copy)]
pub struct CompressorParams {
    pub threshold_db: f32,
    pub ratio: f32,
    pub knee_db: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
    pub makeup_db: f32,
}

impl Default for CompressorParams {
    fn default() -> Self {
        Self {
            threshold_db: -18.0,
            ratio: 3.0,
            knee_db: 6.0,
            attack_ms: 10.0,
            release_ms: 120.0,
            makeup_db: 0.0,
        }
    }
}

/// 单声道压缩器，返回增益衰减量（dB，负值）。
#[derive(Debug, Clone)]
pub struct Compressor {
    params: CompressorParams,
    attack_coef: f32,
    release_coef: f32,
    env: f32,
    sr: f32,
}

impl Compressor {
    pub fn new(sr: f32, params: CompressorParams) -> Self {
        let attack_coef = (-1.0 / (params.attack_ms.max(0.01) * 0.001 * sr)).exp();
        let release_coef = (-1.0 / (params.release_ms.max(0.01) * 0.001 * sr)).exp();
        Self {
            params,
            attack_coef,
            release_coef,
            env: 0.0,
            sr,
        }
    }

    /// 计算当前样本应施加的增益（线性）。
    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        // 电平检测：使用平方平滑近似 RMS。
        let level = x * x;
        let coef = if level > self.env {
            self.attack_coef
        } else {
            self.release_coef
        };
        self.env = coef * self.env + (1.0 - coef) * level;
        let env_db = 10.0 * (self.env + 1e-12).log10();

        let p = &self.params;
        // 软膝。
        let over = env_db - p.threshold_db;
        let reduction_db = if over <= -p.knee_db / 2.0 {
            0.0
        } else if over >= p.knee_db / 2.0 {
            over * (1.0 - 1.0 / p.ratio.max(1.0))
        } else {
            let k = p.knee_db.max(0.01);
            ((over + k / 2.0).powi(2)) / (2.0 * k) * (1.0 - 1.0 / p.ratio.max(1.0))
        };
        let gain_db = -reduction_db + p.makeup_db;
        10f32.powf(gain_db / 20.0)
    }

    pub fn sample_rate(&self) -> f32 {
        self.sr
    }
}

/// 侧链 ducking：用人声包络控制伴奏增益。
/// `reduction_db` 为负值时表示人声出现时伴奏下降。
#[derive(Debug, Clone)]
pub struct SidechainDucker {
    env: f32,
    attack_coef: f32,
    release_coef: f32,
    reduction_db: f32,
    floor: f32,
    sr: f32,
}

impl SidechainDucker {
    pub fn new(sr: f32, reduction_db: f32, attack_ms: f32, release_ms: f32) -> Self {
        Self {
            env: 0.0,
            attack_coef: (-1.0 / (attack_ms.max(0.01) * 0.001 * sr)).exp(),
            release_coef: (-1.0 / (release_ms.max(0.01) * 0.001 * sr)).exp(),
            reduction_db,
            floor: 10f32.powf(reduction_db / 20.0),
            sr,
        }
    }

    /// 依据侧链（人声）样本，返回目标通道（伴奏）的增益。
    #[inline]
    pub fn process(&mut self, sidechain_sample: f32) -> f32 {
        let level = sidechain_sample * sidechain_sample;
        let coef = if level > self.env {
            self.attack_coef
        } else {
            self.release_coef
        };
        self.env = coef * self.env + (1.0 - coef) * level;
        let env_db = 10.0 * (self.env + 1e-12).log10();
        // 人声高于 -40dBFS 开始 duck，低于 -40 则不加。
        let norm = ((env_db + 40.0) / 40.0).clamp(0.0, 1.0);
        let gain_db = self.reduction_db * norm;
        10f32.powf(gain_db / 20.0).max(self.floor.min(1.0))
    }

    pub fn sample_rate(&self) -> f32 {
        self.sr
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compressor_reduces_loud_signal() {
        let mut comp = Compressor::new(
            48000.0,
            CompressorParams {
                threshold_db: -20.0,
                ratio: 4.0,
                knee_db: 0.0,
                attack_ms: 1.0,
                release_ms: 50.0,
                makeup_db: 0.0,
            },
        );
        let mut last_gain = 1.0;
        for _ in 0..4800 {
            last_gain = comp.process(0.8);
        }
        assert!(last_gain < 0.5, "压缩器未充分衰减: {}", last_gain);
    }

    #[test]
    fn test_ducker_reduces_when_sidechain_loud() {
        let mut duck = SidechainDucker::new(48000.0, -6.0, 5.0, 100.0);
        let mut g = 1.0;
        for _ in 0..2400 {
            g = duck.process(0.7);
        }
        assert!(g < 0.7, "ducker 未衰减伴奏: {}", g);
    }
}
