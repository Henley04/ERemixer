//! 效果器链渲染 —— 把 StemFX 预测出的 multiafx 链作用到分轨上。
//!
//! 说明：multiafx 的 85 个效果器建立在 audiomentations / sox / torchaudio /
//! scipy / librosa 之上，逐位复刻不现实。这里按"滤波器族 / 动态族 / 调制族"
//! 实现等价 DSP，参数语义与 multiafx 一致；无法在不变速前提下实现的效果
//! （变调、变速、时间拉伸）会被跳过并写入 `RenderReport::skipped`，
//! 绝不静默假装已处理。

use crate::dsp::loudness::integrated_lufs;
use crate::stemfx::chain::{Chain, FxStep};
use serde::Serialize;

#[derive(Debug, Default, Serialize)]
pub struct RenderReport {
    pub applied: Vec<String>,
    pub skipped: Vec<String>,
}

pub type Planes = Vec<Vec<f32>>;

pub fn render_stem(steps: &[FxStep], planes: &mut Planes, sr: f32, report: &mut RenderReport) {
    for step in steps {
        apply_one(step, planes, sr, report);
    }
    sanitize(planes);
}

pub fn render_chain(chain: &Chain, stems: &mut [(String, Planes)], sr: f32) -> RenderReport {
    let mut report = RenderReport::default();
    for (name, planes) in stems.iter_mut() {
        if let Some((_, steps)) = chain.stems.iter().find(|(s, _)| s == name) {
            for step in steps {
                apply_one(step, planes, sr, &mut report);
            }
        }
        sanitize(planes);
    }
    report
}

fn apply_one(step: &FxStep, p: &mut Planes, sr: f32, rep: &mut RenderReport) {
    let n = &step.effect;
    let g = |k: &str, d: f32| step.get_or(k, d);
    match n.as_str() {
        // ---------------- 增益 / 电平 ----------------
        "am_gain" | "sox_gain" | "ta_gain" => {
            gain(p, db_to_lin(g("gain_db", 0.0)));
            rep.applied.push(n.clone());
        }
        "sox_vol" => {
            gain(p, g("gain", 1.0));
            rep.applied.push(n.clone());
        }
        "am_gain_transition" => {
            let target = db_to_lin(g("gain_db", 0.0));
            let dur = g("duration", 1.0).max(0.01) * sr;
            let frames = p[0].len();
            for plane in p.iter_mut() {
                for (i, v) in plane.iter_mut().enumerate() {
                    let t = (i as f32 / dur).min(1.0);
                    *v *= 1.0 + (target - 1.0) * t;
                }
            }
            let _ = frames;
            rep.applied.push(n.clone());
        }
        "am_normalize" => {
            let peak = p.iter().flat_map(|x| x.iter()).fold(0.0f32, |a, b| a.max(b.abs()));
            if peak > 1e-9 {
                gain(p, 1.0 / peak);
            }
            rep.applied.push(n.clone());
        }
        "am_loudness_normalization" | "pln_loudness_normalize" => {
            let target = g("lufs", -23.0);
            normalize_lufs(p, sr, target);
            rep.applied.push(n.clone());
        }
        "sox_loudness" => {
            gain(p, db_to_lin(g("gain_db", 0.0)));
            rep.applied.push(n.clone());
        }
        "sox_contrast" | "ta_contrast" => {
            let amount = g("enhancement_amount", g("amount", 40.0));
            contrast(p, amount);
            rep.applied.push(n.clone());
        }
        "am_polarity_inversion" => {
            gain(p, -1.0);
            rep.applied.push(n.clone());
        }
        "am_reverse" => {
            for plane in p.iter_mut() {
                plane.reverse();
            }
            rep.applied.push(n.clone());
        }
        "sox_dcshift" | "ta_dcshift" => {
            let s = g("shift", 0.0);
            for plane in p.iter_mut() {
                for v in plane.iter_mut() {
                    *v += s;
                }
            }
            rep.applied.push(n.clone());
        }
        "am_bit_crush" => {
            let bits = g("bit_depth", 8.0).clamp(2.0, 16.0);
            let levels = 2f32.powf(bits) / 2.0;
            for plane in p.iter_mut() {
                for v in plane.iter_mut() {
                    *v = (*v * levels).round() / levels;
                }
            }
            rep.applied.push(n.clone());
        }
        "am_aliasing" => {
            let target = g("target_sample_rate", 8000.0).max(1000.0);
            downsample_upsample(p, sr, target);
            rep.applied.push(n.clone());
        }
        "am_add_gaussian_noise" => {
            add_noise(p, g("amplitude", 0.005), 0.0);
            rep.applied.push(n.clone());
        }
        "am_add_color_noise" => {
            add_noise(p, db_to_lin(-g("snr_db", 20.0) / 20.0) * 0.3, g("f_decay", 0.0));
            rep.applied.push(n.clone());
        }
        "am_air_absorption" => {
            let d = g("distance", 30.0);
            let f = (18000.0 / (1.0 + d / 25.0)).clamp(1500.0, 18000.0);
            butter(p, sr, Kind::Lp, f, 2);
            gain(p, 1.0 / (1.0 + d / 50.0));
            rep.applied.push(n.clone());
        }

        // ---------------- 失真 / 饱和 ----------------
        "am_tanh_distortion" => {
            let k = 1.0 + g("distortion", 0.3) * 12.0;
            shape(p, |x| (x * k).tanh() / k.tanh());
            rep.applied.push(n.clone());
        }
        "am_clipping_distortion" => {
            let pct = g("percentile_threshold", 10.0) / 100.0;
            let mut mags: Vec<f32> = p.iter().flat_map(|x| x.iter().map(|v| v.abs())).collect();
            mags.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let thr = mags[((mags.len() as f32 * (1.0 - pct)) as usize).min(mags.len() - 1)]
                .max(1e-4);
            shape(p, move |x| x.clamp(-thr, thr));
            rep.applied.push(n.clone());
        }
        "sox_overdrive" | "ta_overdrive" => {
            let gdb = g("gain", g("gain_db", 20.0));
            let colour = g("colour", 40.0);
            let drive = db_to_lin(gdb / 2.0);
            let bias = (colour / 100.0 - 0.5) * 0.4;
            shape(p, move |x| ((x + bias) * drive).tanh() - bias.tanh());
            rep.applied.push(n.clone());
        }

        // ---------------- 动态 ----------------
        "sox_compand" => {
            let attack = g("attack_time", 0.02);
            let release = g("decay_time", 0.2);
            let knee = g("soft_knee_db", 6.0);
            compress(p, sr, -18.0, 3.0, attack, release, knee);
            rep.applied.push(n.clone());
        }

        // ---------------- 滤波（audiomentations 家族） ----------------
        "am_highpass_filter" => {
            let order = (g("rolloff", 12.0) / 6.0).round().clamp(1.0, 4.0) as usize;
            butter(p, sr, Kind::Hp, g("cutoff_freq", 100.0), order * 2);
            rep.applied.push(n.clone());
        }
        "am_lowpass_filter" => {
            let order = (g("rolloff", 12.0) / 6.0).round().clamp(1.0, 4.0) as usize;
            butter(p, sr, Kind::Lp, g("cutoff_freq", 5000.0), order * 2);
            rep.applied.push(n.clone());
        }
        "am_bandpass_filter" | "am_bandstop_filter" => {
            let f = g("center_freq", 1000.0);
            let bw = g("bandwidth_fraction", 1.0).clamp(0.2, 2.0);
            let (lo, hi) = (f / (1.0 + bw * 0.5), f * (1.0 + bw * 0.5));
            let kind = if n == "am_bandpass_filter" { Kind::Bp } else { Kind::Notch };
            biquad(p, sr, kind, f, (f / (hi - lo).max(1.0)).clamp(0.3, 8.0), 0.0);
            let _ = (lo, hi);
            rep.applied.push(n.clone());
        }
        "am_high_shelf_filter" => {
            biquad(p, sr, Kind::ShelfHigh, g("center_freq", 4000.0), g("q", 0.7), g("gain_db", 0.0));
            rep.applied.push(n.clone());
        }
        "am_low_shelf_filter" => {
            biquad(p, sr, Kind::ShelfLow, g("center_freq", 200.0), g("q", 0.7), g("gain_db", 0.0));
            rep.applied.push(n.clone());
        }
        "am_peaking_filter" => {
            biquad(p, sr, Kind::Peak, g("center_freq", 1000.0), g("q", 1.0), g("gain_db", 0.0));
            rep.applied.push(n.clone());
        }
        "am_seven_band_parametric_eq" => {
            let gdb = g("gain_db", 0.0);
            for f in [100.0, 200.0, 400.0, 800.0, 1600.0, 3200.0, 6400.0] {
                biquad(p, sr, Kind::Peak, f, 1.0, gdb);
            }
            rep.applied.push(n.clone());
        }

        // ---------------- 滤波（sox 家族） ----------------
        "sox_bass" => {
            biquad(p, sr, Kind::ShelfLow, g("frequency", 100.0), g("slope", 0.5).clamp(0.2, 1.5), g("gain_db", 0.0));
            rep.applied.push(n.clone());
        }
        "sox_treble" => {
            biquad(p, sr, Kind::ShelfHigh, g("frequency", 3000.0), g("slope", 0.5).clamp(0.2, 1.5), g("gain_db", 0.0));
            rep.applied.push(n.clone());
        }
        "sox_equalizer" => {
            biquad(p, sr, Kind::Peak, g("frequency", 1000.0), g("width_q", 1.0), g("gain_db", 0.0));
            rep.applied.push(n.clone());
        }
        "sox_highpass" => {
            biquad(p, sr, Kind::Hp, g("frequency", 100.0), g("width_q", 0.707), 0.0);
            rep.applied.push(n.clone());
        }
        "sox_lowpass" => {
            biquad(p, sr, Kind::Lp, g("frequency", 5000.0), g("width_q", 0.707), 0.0);
            rep.applied.push(n.clone());
        }
        "sox_bandpass" => {
            biquad(p, sr, Kind::Bp, g("frequency", 1000.0), g("width_q", 1.0), 0.0);
            rep.applied.push(n.clone());
        }
        "sox_bandreject" => {
            biquad(p, sr, Kind::Notch, g("frequency", 1000.0), g("width_q", 1.0), 0.0);
            rep.applied.push(n.clone());
        }
        "sox_allpass" => {
            biquad(p, sr, Kind::AllPass, g("frequency", 1000.0), g("width_q", 1.0), 0.0);
            rep.applied.push(n.clone());
        }
        "sox_earwax" => {
            // sox 的 earwax 预设：轻微高切 + 低频增强，模拟"耳塞"听感
            biquad(p, sr, Kind::Hp, 22.0, 0.707, 0.0);
            biquad(p, sr, Kind::ShelfLow, 180.0, 0.7, 2.0);
            biquad(p, sr, Kind::ShelfHigh, 7000.0, 0.7, -2.0);
            biquad(p, sr, Kind::Lp, 16000.0, 0.707, 0.0);
            rep.applied.push(n.clone());
        }

        // ---------------- 滤波（torchaudio 家族） ----------------
        "ta_bass_biquad" => {
            biquad(p, sr, Kind::ShelfLow, g("central_freq", 100.0), g("Q", 0.707), g("gain_db", 0.0));
            rep.applied.push(n.clone());
        }
        "ta_treble_biquad" => {
            biquad(p, sr, Kind::ShelfHigh, g("central_freq", 3000.0), g("Q", 0.707), g("gain_db", 0.0));
            rep.applied.push(n.clone());
        }
        "ta_equalizer_biquad" => {
            biquad(p, sr, Kind::Peak, g("center_freq", 1000.0), g("Q", 1.0), g("gain_db", 0.0));
            rep.applied.push(n.clone());
        }
        "ta_highpass_biquad" => {
            biquad(p, sr, Kind::Hp, g("cutoff_freq", 100.0), g("Q", 0.707), 0.0);
            rep.applied.push(n.clone());
        }
        "ta_lowpass_biquad" => {
            biquad(p, sr, Kind::Lp, g("cutoff_freq", 5000.0), g("Q", 0.707), 0.0);
            rep.applied.push(n.clone());
        }
        "ta_bandpass_biquad" => {
            biquad(p, sr, Kind::Bp, g("central_freq", 1000.0), g("Q", 1.0), 0.0);
            rep.applied.push(n.clone());
        }
        "ta_bandreject_biquad" => {
            biquad(p, sr, Kind::Notch, g("central_freq", 1000.0), g("Q", 1.0), 0.0);
            rep.applied.push(n.clone());
        }
        "ta_allpass_biquad" => {
            biquad(p, sr, Kind::AllPass, g("central_freq", 1000.0), g("Q", 1.0), 0.0);
            rep.applied.push(n.clone());
        }
        "ta_riaa_biquad" => {
            // RIAA 放音均衡的近似：500 Hz 低架 +9.9 dB、2122 Hz 高架 −9.9 dB
            biquad(p, sr, Kind::ShelfLow, 500.0, 0.707, 9.9);
            biquad(p, sr, Kind::ShelfHigh, 2122.0, 0.707, -9.9);
            rep.applied.push(n.clone());
        }
        "ta_deemph_biquad" => {
            biquad(p, sr, Kind::Hp, 20.0, 0.707, 0.0);
            rep.applied.push(n.clone());
        }

        // ---------------- 滤波（scipy 家族） ----------------
        "sci_butter_lowpass" | "sci_cheby1_lowpass" | "sci_cheby2_lowpass" | "sci_bessel_lowpass" => {
            let order = g("order", 2.0).round().clamp(1.0, 6.0) as usize;
            butter(p, sr, Kind::Lp, g("cutoff", 5000.0), order);
            rep.applied.push(n.clone());
        }
        "sci_butter_highpass" | "sci_cheby1_highpass" | "sci_cheby2_highpass" | "sci_bessel_highpass" => {
            let order = g("order", 2.0).round().clamp(1.0, 6.0) as usize;
            butter(p, sr, Kind::Hp, g("cutoff", 100.0), order);
            rep.applied.push(n.clone());
        }
        "sci_butter_bandpass" | "sci_butter_bandstop" => {
            let (lo, hi) = (g("low_freq", 300.0), g("high_freq", 4000.0));
            let center = (lo * hi).sqrt();
            let q = (center / (hi - lo).max(1.0)).clamp(0.3, 8.0);
            let kind = if n.ends_with("bandpass") { Kind::Bp } else { Kind::Notch };
            biquad(p, sr, kind, center, q, 0.0);
            rep.applied.push(n.clone());
        }
        "sci_notch_iir" => {
            biquad(p, sr, Kind::Notch, g("cutoff", 1000.0), g("Q", 1.0), 0.0);
            rep.applied.push(n.clone());
        }
        "sci_peak_iir" => {
            biquad(p, sr, Kind::Peak, g("cutoff", 1000.0), g("Q", 1.0), g("gain_db", 0.0));
            rep.applied.push(n.clone());
        }
        "sci_fir_lowpass" | "sci_fir_highpass" => {
            let taps = g("numtaps", 64.0).clamp(8.0, 512.0) as usize;
            let kind = if n.ends_with("lowpass") { Kind::Lp } else { Kind::Hp };
            fir_sinc(p, sr, g("cutoff", 5000.0), taps, kind);
            rep.applied.push(n.clone());
        }

        // ---------------- 空间 / 调制 ----------------
        "sox_reverb" => {
            let room = g("room_scale", 50.0) / 100.0;
            let damp = g("high_freq_damping", 50.0) / 100.0;
            let wet = db_to_lin(g("wet_gain", -6.0)).clamp(0.0, 1.0);
            let predelay = g("pre_delay", 0.0);
            let depth = g("stereo_depth", 50.0) / 100.0;
            reverb(p, sr, room, damp, wet, predelay, depth);
            rep.applied.push(n.clone());
        }
        "sox_echo" | "sox_echos" => {
            let gain_in = g("gain_in", 0.8);
            let gain_out = g("gain_out", 0.5);
            let delay = g("delay_ms", 200.0);
            let decay = g("decay", 0.3);
            echo(p, sr, gain_in, gain_out, delay, decay);
            rep.applied.push(n.clone());
        }
        "sox_chorus" => {
            let delay = g("delay_ms", 40.0);
            let depth = g("depth", 2.0);
            let speed = g("speed", 1.0);
            let mix = g("gain_out", 0.5);
            chorus(p, sr, delay, depth, speed, mix);
            rep.applied.push(n.clone());
        }
        "sox_flanger" => {
            let delay = g("delay", 3.0);
            let depth = g("depth", 2.0);
            let regen = g("regen", 20.0) / 100.0;
            let speed = g("speed", 0.5);
            flanger(p, sr, delay, depth, regen, speed);
            rep.applied.push(n.clone());
        }
        "sox_phaser" => {
            let speed = g("speed", 0.5);
            let decay = g("decay", 0.3);
            phaser(p, sr, speed, decay);
            rep.applied.push(n.clone());
        }
        "sox_tremolo" => {
            let speed = g("speed", 4.0);
            let depth = g("depth", 50.0) / 100.0;
            tremolo(p, sr, speed, depth);
            rep.applied.push(n.clone());
        }
        "npy_lr_pan" => {
            let pan = g("pan", 0.0).clamp(-1.0, 1.0);
            if p.len() >= 2 {
                gain_ch(&mut p[0], (1.0 - pan) * 0.5 + 0.5);
                gain_ch(&mut p[1], (1.0 + pan) * 0.5 + 0.5);
            }
            rep.applied.push(n.clone());
        }
        "npy_stereo_widener" => {
            let w = g("width", 1.2).clamp(0.0, 2.0);
            if p.len() >= 2 {
                for i in 0..p[0].len() {
                    let (l, r) = (p[0][i], p[1][i]);
                    let (mid, side) = ((l + r) * 0.5, (l - r) * 0.5 * w);
                    p[0][i] = mid + side;
                    p[1][i] = mid - side;
                }
            }
            rep.applied.push(n.clone());
        }

        // ---------------- 预加重 / 去加重（一阶 IIR，50 µs 时间常数） ----------------
        "sox_emph" | "lib_preemphasis" | "sox_deemph" | "lib_deemphasis" => {
            // 时间常数 50 µs（FM / CD 惯例）：a = exp(-1/(τ·sr))
            let a = (-1.0f32 / (50e-6 * sr)).exp();
            let de = matches!(n.as_str(), "sox_deemph" | "lib_deemphasis");
            for plane in p.iter_mut() {
                let mut prev = 0.0f32;
                for v in plane.iter_mut() {
                    let x = *v;
                    if de {
                        // y[n] = x[n] + a·y[n-1]，按直流增益 (1-a) 归一
                        prev = x + a * prev;
                        *v = prev * (1.0 - a);
                    } else {
                        // y[n] = (x[n] - a·x[n-1]) / (1-a)
                        *v = (x - a * prev) / (1.0 - a);
                        prev = x;
                    }
                }
            }
            rep.applied.push(n.clone());
        }

        // ---------------- 无法在不变速前提下实现 ----------------
        "am_pitch_shift" | "am_time_stretch" | "sox_pitch" | "sox_tempo" | "sox_speed"
        | "lib_pitch_shift" | "lib_time_stretch" => {
            rep.skipped.push(n.clone());
        }
        _ => rep.skipped.push(n.clone()),
    }
}

// ============================================================ 基础 DSP

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Lp,
    Hp,
    Bp,
    Notch,
    Peak,
    ShelfLow,
    ShelfHigh,
    AllPass,
}

#[derive(Clone, Copy)]
struct Bq {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl Bq {
    fn new(kind: Kind, sr: f32, f: f32, q: f32, gain_db: f32) -> Self {
        let fs = sr.max(8000.0);
        let f = f.clamp(10.0, fs * 0.45);
        let w0 = std::f32::consts::TAU * f / fs;
        let (sn, cs) = (w0.sin(), w0.cos());
        let q = q.max(0.05);
        let alpha = sn / (2.0 * q);
        let a = 10f32.powf(gain_db / 40.0);
        let (b0, b1, b2, a0, a1, a2) = match kind {
            Kind::Lp => ((1.0 - cs) / 2.0, 1.0 - cs, (1.0 - cs) / 2.0, 1.0 + alpha, -2.0 * cs, 1.0 - alpha),
            Kind::Hp => ((1.0 + cs) / 2.0, -(1.0 + cs), (1.0 + cs) / 2.0, 1.0 + alpha, -2.0 * cs, 1.0 - alpha),
            Kind::Bp => (alpha, 0.0, -alpha, 1.0 + alpha, -2.0 * cs, 1.0 - alpha),
            Kind::Notch => (1.0, -2.0 * cs, 1.0, 1.0 + alpha, -2.0 * cs, 1.0 - alpha),
            Kind::AllPass => (1.0 - alpha, -2.0 * cs, 1.0 + alpha, 1.0 + alpha, -2.0 * cs, 1.0 - alpha),
            Kind::Peak => (
                1.0 + alpha * a,
                -2.0 * cs,
                1.0 - alpha * a,
                1.0 + alpha / a,
                -2.0 * cs,
                1.0 - alpha / a,
            ),
            Kind::ShelfLow => {
                let sa = 2.0 * a.sqrt() * alpha;
                (
                    a * ((a + 1.0) - (a - 1.0) * cs + sa),
                    2.0 * a * ((a - 1.0) - (a + 1.0) * cs),
                    a * ((a + 1.0) - (a - 1.0) * cs - sa),
                    (a + 1.0) + (a - 1.0) * cs + sa,
                    -2.0 * ((a - 1.0) + (a + 1.0) * cs),
                    (a + 1.0) + (a - 1.0) * cs - sa,
                )
            }
            Kind::ShelfHigh => {
                let sa = 2.0 * a.sqrt() * alpha;
                (
                    a * ((a + 1.0) + (a - 1.0) * cs + sa),
                    -2.0 * a * ((a - 1.0) + (a + 1.0) * cs),
                    a * ((a + 1.0) + (a - 1.0) * cs - sa),
                    (a + 1.0) - (a - 1.0) * cs + sa,
                    2.0 * ((a - 1.0) - (a + 1.0) * cs),
                    (a + 1.0) - (a - 1.0) * cs - sa,
                )
            }
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

    fn run(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2 - self.a1 * self.y1 - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

fn biquad(p: &mut Planes, sr: f32, kind: Kind, f: f32, q: f32, gain_db: f32) {
    for plane in p.iter_mut() {
        let mut bq = Bq::new(kind, sr, f, q, gain_db);
        for v in plane.iter_mut() {
            *v = bq.run(*v);
        }
    }
}

/// Butterworth 级联（偶数阶；奇数阶向上取偶）。
fn butter(p: &mut Planes, sr: f32, kind: Kind, f: f32, order: usize) {
    let n = order.clamp(2, 8) / 2 * 2;
    for plane in p.iter_mut() {
        let mut stages: Vec<Bq> = (0..n / 2)
            .map(|k| {
                let q = 1.0 / (2.0 * (std::f32::consts::PI * (2 * k + 1) as f32 / (2.0 * n as f32)).cos());
                Bq::new(kind, sr, f, q.abs(), 0.0)
            })
            .collect();
        for v in plane.iter_mut() {
            let mut y = *v;
            for s in stages.iter_mut() {
                y = s.run(y);
            }
            *v = y;
        }
    }
}

fn fir_sinc(p: &mut Planes, sr: f32, cutoff: f32, taps: usize, kind: Kind) {
    let fc = (cutoff / sr).clamp(0.001, 0.49);
    let m = taps | 1; // 奇数长度，线性相位
    let mid = (m - 1) as f32 / 2.0;
    let mut h: Vec<f32> = (0..m)
        .map(|i| {
            let t = i as f32 - mid;
            let sinc = if t.abs() < 1e-6 { 2.0 * fc } else { (std::f32::consts::TAU * fc * t).sin() / (std::f32::consts::PI * t) };
            let w = 0.54 - 0.46 * (std::f32::consts::TAU * i as f32 / (m - 1) as f32).cos();
            sinc * w
        })
        .collect();
    if kind == Kind::Hp {
        for (i, v) in h.iter_mut().enumerate() {
            let delta = if i == m / 2 { 1.0 } else { 0.0 };
            *v = delta - *v;
        }
    }
    let sum: f32 = h.iter().sum();
    if sum.abs() > 1e-9 {
        for v in h.iter_mut() {
            *v /= sum;
        }
    }
    for plane in p.iter_mut() {
        let n = plane.len();
        let mut out = vec![0.0f32; n];
        for i in 0..n {
            let mut acc = 0.0;
            for (k, hk) in h.iter().enumerate() {
                let idx = i as isize - k as isize + mid as isize;
                if idx >= 0 && (idx as usize) < n {
                    acc += plane[idx as usize] * hk;
                }
            }
            out[i] = acc;
        }
        *plane = out;
    }
}

fn gain(p: &mut Planes, g: f32) {
    for plane in p.iter_mut() {
        for v in plane.iter_mut() {
            *v *= g;
        }
    }
}

fn gain_ch(plane: &mut [f32], g: f32) {
    for v in plane.iter_mut() {
        *v *= g;
    }
}

fn shape<F: Fn(f32) -> f32>(p: &mut Planes, f: F) {
    for plane in p.iter_mut() {
        for v in plane.iter_mut() {
            *v = f(*v);
        }
    }
}

fn db_to_lin(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

fn contrast(p: &mut Planes, amount: f32) {
    // 软扩展：提升瞬态对比度（amount 10..100）
    let k = (amount / 100.0).clamp(0.05, 1.0) as f32;
    shape(p, move |x| {
        let s = x.signum();
        let a = x.abs();
        s * (a + k * a * a * a).min(1.5)
    });
}

fn compress(p: &mut Planes, sr: f32, thr_db: f32, ratio: f32, attack: f32, release: f32, knee: f32) {
    let atk = (-1.0 / (attack.max(1e-4) * sr)).exp();
    let rel = (-1.0 / (release.max(1e-4) * sr)).exp();
    let mut env = 0.0f32;
    for plane in p.iter_mut() {
        for v in plane.iter_mut() {
            let a = v.abs();
            env = if a > env { atk * env + (1.0 - atk) * a } else { rel * env + (1.0 - rel) * a };
            let db = 20.0 * env.max(1e-6).log10();
            let over = db - thr_db;
            let gr = if over <= -knee {
                0.0
            } else if over >= knee {
                over * (1.0 - 1.0 / ratio)
            } else {
                let t = over + knee;
                (t * t) / (4.0 * knee) * (1.0 - 1.0 / ratio)
            };
            *v *= db_to_lin(-gr);
        }
    }
}

fn normalize_lufs(p: &mut Planes, sr: f32, target: f32) {
    let l = integrated_lufs(p, sr);
    if l.is_finite() && l > -70.0 {
        gain(p, db_to_lin(target - l));
    }
}

fn add_noise(p: &mut Planes, amp: f32, tilt: f32) {
    let mut rng: u64 = 0x2545_F491_4F6C_DD1D;
    let mut next = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        ((rng >> 40) as f32 / 8388608.0) - 1.0
    };
    let mut lp = 0.0f32;
    let coef = (0.5 + tilt * 0.05).clamp(0.05, 0.95);
    for plane in p.iter_mut() {
        for v in plane.iter_mut() {
            let n = next();
            lp = lp * coef + n * (1.0 - coef);
            *v += lp * amp;
        }
    }
}

fn downsample_upsample(p: &mut Planes, sr: f32, target: f32) {
    let factor = (sr / target).max(1.0);
    for plane in p.iter_mut() {
        let n = plane.len();
        let mut out = vec![0.0f32; n];
        let m = (n as f32 / factor) as usize;
        let mut hold = 0.0;
        for i in 0..n {
            let src = (i as f32 / factor) as usize;
            if src < m {
                if (src as f32 * factor) as usize <= i {
                    hold = plane[src.min(n - 1)];
                }
                out[i] = hold;
            }
        }
        *plane = out;
    }
}

// ---- 空间类 ----

fn reverb(p: &mut Planes, sr: f32, room: f32, damp: f32, wet: f32, predelay_ms: f32, depth: f32) {
    let n = p[0].len();
    let scale = 0.6 + room * 1.2;
    let combs_ms = [29.7, 37.1, 41.1, 43.7];
    let amps = [0.805, 0.827, 0.783, 0.764];
    let damp = damp.clamp(0.0, 0.95);
    for (ch, plane) in p.iter_mut().enumerate() {
        let pd = (predelay_ms / 1000.0 * sr) as usize + ch * 37;
        let mut wetbuf = vec![0.0f32; n];
        for (ci, &d) in combs_ms.iter().enumerate() {
            let delay = ((d * scale / 1000.0 * sr) as usize).max(2);
            let mut buf = vec![0.0f32; delay];
            let mut idx = 0usize;
            let amp = amps[ci];
            let mut lp = 0.0f32;
            for i in 0..n {
                let out = buf[idx];
                lp = out * (1.0 - damp) + lp * damp;
                buf[idx] = (if i >= pd { plane[i - pd] } else { 0.0 }) + lp * amp;
                idx = (idx + 1) % delay;
                wetbuf[i] += out * 0.25;
            }
        }
        // 两个全通
        for &d in &[5.0f32, 1.7] {
            let delay = ((d * scale / 1000.0 * sr) as usize).max(1);
            let mut buf = vec![0.0f32; delay];
            let mut idx = 0usize;
            let g = 0.7;
            for i in 0..n {
                let bufout = buf[idx];
                let x = wetbuf[i] + (-g) * bufout;
                buf[idx] = x + g * bufout;
                wetbuf[i] = bufout - g * x;
                idx = (idx + 1) % delay;
            }
        }
        let d = 1.0 + depth * 0.3;
        for i in 0..n {
            plane[i] = plane[i] * (1.0 - wet) + wetbuf[i] * wet * d;
        }
    }
}

fn echo(p: &mut Planes, sr: f32, gain_in: f32, gain_out: f32, delay_ms: f32, decay: f32) {
    let n = p[0].len();
    let d = ((delay_ms / 1000.0 * sr) as usize).max(1);
    for plane in p.iter_mut() {
        let mut buf = vec![0.0f32; d];
        let mut idx = 0usize;
        for i in 0..n {
            let delayed = buf[idx];
            let x = plane[i] * gain_in + delayed * decay;
            buf[idx] = x;
            idx = (idx + 1) % d;
            plane[i] = plane[i] + delayed * gain_out;
        }
    }
}

fn chorus(p: &mut Planes, sr: f32, delay_ms: f32, depth: f32, speed: f32, mix: f32) {
    let n = p[0].len();
    let base = (delay_ms / 1000.0 * sr).max(1.0);
    let depth_s = (depth / 1000.0 * sr).max(1.0);
    for plane in p.iter_mut() {
        let src = plane.clone();
        for i in 0..n {
            let lfo = (std::f32::consts::TAU * speed * i as f32 / sr).sin();
            let d = base + depth_s * lfo;
            let pos = i as f32 - d;
            let i0 = pos.floor().max(0.0) as usize;
            let frac = (pos - i0 as f32).clamp(0.0, 1.0);
            let a = src[i0.min(n - 1)];
            let b = src[(i0 + 1).min(n - 1)];
            plane[i] = src[i] * (1.0 - mix * 0.5) + (a + (b - a) * frac) * mix * 0.5;
        }
    }
}

fn flanger(p: &mut Planes, sr: f32, delay_ms: f32, depth: f32, regen: f32, speed: f32) {
    let n = p[0].len();
    let base = (delay_ms / 1000.0 * sr).max(1.0);
    let depth_s = (depth / 1000.0 * sr).max(0.0);
    for plane in p.iter_mut() {
        let mut buf = vec![0.0f32; (base + depth_s) as usize + 2];
        let mut idx = 0usize;
        for i in 0..n {
            let lfo = (std::f32::consts::TAU * speed * i as f32 / sr).sin();
            let d = (base + depth_s * (0.5 + 0.5 * lfo)) as usize;
            let j = (idx + buf.len() - d) % buf.len();
            let delayed = buf[j];
            let x = plane[i] + delayed * regen;
            buf[idx] = x;
            idx = (idx + 1) % buf.len();
            plane[i] = plane[i] * 0.7 + delayed * 0.5;
        }
    }
}

fn phaser(p: &mut Planes, sr: f32, speed: f32, decay: f32) {
    let n = p[0].len();
    for plane in p.iter_mut() {
        let src = plane.clone();
        let mut stages = [Bq::new(Kind::AllPass, sr, 1000.0, 0.7, 0.0); 4];
        for i in 0..n {
            let lfo = (std::f32::consts::TAU * speed * i as f32 / sr).sin();
            if i % 64 == 0 {
                for (k, s) in stages.iter_mut().enumerate() {
                    let f = 400.0 * (1.0 + k as f32 * 0.6) * (1.0 + 0.5 * lfo);
                    *s = Bq::new(Kind::AllPass, sr, f, 0.7, 0.0);
                }
            }
            let mut y = src[i];
            for s in stages.iter_mut() {
                y = s.run(y);
            }
            plane[i] = src[i] + y * decay;
        }
    }
}

fn tremolo(p: &mut Planes, sr: f32, speed: f32, depth: f32) {
    let n = p[0].len();
    for plane in p.iter_mut() {
        for i in 0..n {
            let lfo = 0.5 + 0.5 * (std::f32::consts::TAU * speed * i as f32 / sr).sin();
            plane[i] *= 1.0 - depth * (1.0 - lfo);
        }
    }
}

fn sanitize(p: &mut Planes) {
    for plane in p.iter_mut() {
        for v in plane.iter_mut() {
            if !v.is_finite() {
                *v = 0.0;
            } else {
                *v = v.clamp(-8.0, 8.0);
            }
        }
    }
}
