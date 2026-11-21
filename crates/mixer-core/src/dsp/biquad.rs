//! RBJ Audio EQ Cookbook 双二阶滤波器（Direct Form I 转置实现）。
//! 用于高通、低频/高频 shelf、peaking（中频雕刻）。

#[derive(Debug, Clone, Copy)]
pub enum FilterType {
    HighPass,
    LowPass,
    Peaking,
    LowShelf,
    HighShelf,
}

#[derive(Debug, Clone)]
pub struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    x1: f64,
    x2: f64,
    y1: f64,
    y2: f64,
}

impl Biquad {
    /// 按 RBJ cookbook 计算系数。
    ///
    /// - `freq`: 中心/截止频率 Hz
    /// - `q`: 品质因数（peaking 时与增益共同决定带宽；shelf 常取 0.707）
    /// - `gain_db`: peaking/shelf 的增益 dB
    pub fn new(kind: FilterType, sample_rate: f32, freq: f32, q: f32, gain_db: f32) -> Self {
        let fs = sample_rate.max(1.0) as f64;
        // 避免 `clamp(min, max)` 在 min > max 时 panic（采样率极低时）。
        let nyquist_guard = (fs * 0.49).max(10.0);
        let f0 = (freq as f64).clamp(10.0, nyquist_guard);
        let w0 = 2.0 * std::f64::consts::PI * f0 / fs;
        let cosw = w0.cos();
        let sinw = w0.sin();
        let q = q.max(1e-4) as f64;
        let alpha = sinw / (2.0 * q);
        let a = 10f64.powf(gain_db as f64 / 40.0);

        let (b0, b1, b2, a0, a1, a2) = match kind {
            FilterType::LowPass => (
                (1.0 - cosw) / 2.0,
                1.0 - cosw,
                (1.0 - cosw) / 2.0,
                1.0 + alpha,
                -2.0 * cosw,
                1.0 - alpha,
            ),
            FilterType::HighPass => (
                (1.0 + cosw) / 2.0,
                -(1.0 + cosw),
                (1.0 + cosw) / 2.0,
                1.0 + alpha,
                -2.0 * cosw,
                1.0 - alpha,
            ),
            FilterType::Peaking => (
                1.0 + alpha * a,
                -2.0 * cosw,
                1.0 - alpha * a,
                1.0 + alpha / a,
                -2.0 * cosw,
                1.0 - alpha / a,
            ),
            // RBJ shelf：交叉项应为 2*sqrt(A)*alpha（alpha 已含 Q），
            // 原实现误写成 2*sqrt(2)*sqrt(A-1)*sin(w0)：A<1 时产生 NaN，
            // 且 A>1 时过渡带形状偏差可达 0.5 dB。
            FilterType::LowShelf => (
                a * ((a + 1.0) - (a - 1.0) * cosw + 2.0 * a.sqrt() * alpha),
                2.0 * a * ((a - 1.0) - (a + 1.0) * cosw),
                a * ((a + 1.0) - (a - 1.0) * cosw - 2.0 * a.sqrt() * alpha),
                (a + 1.0) + (a - 1.0) * cosw + 2.0 * a.sqrt() * alpha,
                -2.0 * ((a - 1.0) + (a + 1.0) * cosw),
                (a + 1.0) + (a - 1.0) * cosw - 2.0 * a.sqrt() * alpha,
            ),
            FilterType::HighShelf => (
                a * ((a + 1.0) + (a - 1.0) * cosw + 2.0 * a.sqrt() * alpha),
                -2.0 * a * ((a - 1.0) + (a + 1.0) * cosw),
                a * ((a + 1.0) + (a - 1.0) * cosw - 2.0 * a.sqrt() * alpha),
                (a + 1.0) - (a - 1.0) * cosw + 2.0 * a.sqrt() * alpha,
                2.0 * ((a - 1.0) - (a + 1.0) * cosw),
                (a + 1.0) - (a - 1.0) * cosw - 2.0 * a.sqrt() * alpha,
            ),
        };

        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    /// 处理单个样本（DF-I transposed 形式）。
    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        let x = x as f64;
        let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2
            - self.a1 * self.y1
            - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y as f32
    }

    /// 就地处理整段单声道。
    pub fn process_buffer(&mut self, buf: &mut [f32]) {
        for s in buf.iter_mut() {
            *s = self.process(*s);
        }
    }

    /// 无状态地处理整段（内部创建滤波器，常用于一次性 EQ）。
    pub fn process_into(kind: FilterType, sr: f32, freq: f32, q: f32, gain_db: f32, buf: &mut [f32]) {
        let mut f = Self::new(kind, sr, freq, q, gain_db);
        f.process_buffer(buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_highpass_attenuates_low() {
        let sr = 48000.0;
        // 100 Hz 正弦，经 1000 Hz 高通后能量应明显衰减。
        let sig: Vec<f32> = (0..48000)
            .map(|i| (2.0 * std::f32::consts::PI * 100.0 * i as f32 / sr).sin())
            .collect();
        let mut filtered = sig.clone();
        Biquad::process_into(FilterType::HighPass, sr, 1000.0, 0.707, 0.0, &mut filtered);
        let rms_in: f32 = (sig.iter().map(|x| x * x).sum::<f32>() / sig.len() as f32).sqrt();
        let rms_out: f32 = (filtered.iter().map(|x| x * x).sum::<f32>() / filtered.len() as f32).sqrt();
        assert!(rms_out < rms_in * 0.3, "HPF 未充分衰减低频: {} vs {}", rms_out, rms_in);
    }

    #[test]
    fn test_peaking_boost() {
        let sr = 48000.0;
        let sig: Vec<f32> = (0..48000)
            .map(|i| (2.0 * std::f32::consts::PI * 3000.0 * i as f32 / sr).sin())
            .collect();
        let mut filtered = sig.clone();
        Biquad::process_into(FilterType::Peaking, sr, 3000.0, 1.0, 6.0, &mut filtered);
        let rms_in: f32 = (sig.iter().map(|x| x * x).sum::<f32>() / sig.len() as f32).sqrt();
        let rms_out: f32 = (filtered.iter().map(|x| x * x).sum::<f32>() / filtered.len() as f32).sqrt();
        assert!(rms_out > rms_in * 1.5, "peaking 未提升 3kHz: {} vs {}", rms_out, rms_in);
    }
}
