//! 一键混音流水线（多轨版）。
//!
//! 输入模式：
//!   · Stems（分轨直入）：直接输入已分离好的乐器分轨，跳过所有分离模型。
//!   · Separate（分离再混）：输入成品混音，先经 SOTA 分离模型拆分；权重不可用时优雅降级。
//!
//! 链路：解析分轨 → 乐器推断 → 统一采样率/声道/长度 → (NN 增强)
//!   → 逐轨专属 DSP 链 → 以人声为侧链的 ducking → 总线混合 + glue
//!   → 立体声宽度 → 响度标准化 + 真峰限幅 → 导出。

use crate::api::{Instrument, MixMode, MixRequest, MixResult, Preset, ProgressEvent, StemInfo, StemInput};
use crate::audio::{match_length, read_wav, resample_buffer, to_stereo, write_wav, AudioBuffer};
use crate::dsp::biquad::{Biquad, FilterType};
use crate::dsp::dynamics::{Compressor, CompressorParams, SidechainDucker};
use crate::dsp::loudness::{integrated_lufs, true_peak_dbtp, TruePeakLimiter};
use crate::dsp::stereo::StereoWidener;
use crate::error::{MixError, Result};
use std::time::Instant;

fn emit<F: FnMut(ProgressEvent)>(cb: &mut Option<F>, percent: u32, stage: &str, detail: Option<String>) {
    if let Some(f) = cb.as_mut() {
        f(ProgressEvent {
            percent,
            stage: stage.to_string(),
            detail,
        });
    }
}

fn db_to_linear(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// 对每条声道独立限幅（真峰驱动）。独立实例避免跨声道状态泄漏。
fn limit_planes(planes: &mut [Vec<f32>], sr: f32, ceiling: f32) {
    for plane in planes.iter_mut() {
        let mut limiter = TruePeakLimiter::new(sr, ceiling, 5.0, 60.0);
        limiter.process(plane);
    }
}

/// 把一条分轨解析为「实际乐器类型」+ 是否推断。
fn resolve_instrument(input: &StemInput) -> (Instrument, bool) {
    if input.instrument == Instrument::Auto {
        (Instrument::infer(&input.path), true)
    } else {
        (input.instrument, false)
    }
}

/// 单一乐器的专属处理链（就地处理每个声道的样本平面）。
/// 各链相互独立，便于专家用户理解与扩展。
fn process_instrument(planes: &mut [Vec<f32>], inst: Instrument, sr: f32, req: &MixRequest) {
    match inst {
        Instrument::Vocals => {
            // 人声：HPF 去隆隆声 → 浑浊削减 → 存在感 → 空气感 → 压缩 → 用户增益。
            let gain = db_to_linear(req.vocal_gain_db);
            for plane in planes.iter_mut() {
                if req.vocal_highpass_hz > 0.0 {
                    Biquad::process_into(FilterType::HighPass, sr, req.vocal_highpass_hz, 0.707, 0.0, plane);
                }
                Biquad::process_into(FilterType::Peaking, sr, 250.0, 0.9, -2.0, plane);
                Biquad::process_into(FilterType::Peaking, sr, 3000.0, 1.0, req.vocal_presence_db, plane);
                Biquad::process_into(FilterType::HighShelf, sr, 10000.0, 0.707, 1.5, plane);
                let mut comp = Compressor::new(
                    sr,
                    CompressorParams {
                        threshold_db: -18.0,
                        ratio: 3.0,
                        knee_db: 6.0,
                        attack_ms: 8.0,
                        release_ms: 120.0,
                        makeup_db: 1.5,
                    },
                );
                for s in plane.iter_mut() {
                    *s *= comp.process(*s);
                }
                for s in plane.iter_mut() {
                    *s *= gain;
                }
            }
        }
        Instrument::Drums => {
            // 鼓：保留低频冲击与瞬态，轻 HPF 去次声，快速透明压缩。
            for plane in planes.iter_mut() {
                Biquad::process_into(FilterType::HighPass, sr, 28.0, 0.707, 0.0, plane);
                Biquad::process_into(FilterType::Peaking, sr, 120.0, 1.0, 1.5, plane);
                Biquad::process_into(FilterType::HighShelf, sr, 9000.0, 0.707, 1.0, plane);
                let mut comp = Compressor::new(
                    sr,
                    CompressorParams {
                        threshold_db: -14.0,
                        ratio: 2.5,
                        knee_db: 4.0,
                        attack_ms: 3.0,
                        release_ms: 90.0,
                        makeup_db: 1.0,
                    },
                );
                for s in plane.iter_mut() {
                    *s *= comp.process(*s);
                }
            }
        }
        Instrument::Bass => {
            // 贝斯：单声道低频重心，紧压缩稳定电平，轻微低中频清晰度。
            let mono_low: Option<Vec<f32>> = if planes.len() >= 2 {
                let n = planes[0].len().min(planes[1].len());
                let mut m = vec![0.0f32; n];
                for i in 0..n {
                    m[i] = (planes[0][i] + planes[1][i]) * 0.5;
                }
                // 对 mono 低频做低通（160Hz）。
                Biquad::process_into(FilterType::LowPass, sr, 160.0, 0.707, 0.0, &mut m);
                Some(m)
            } else {
                None
            };
            for (ci, plane) in planes.iter_mut().enumerate() {
                Biquad::process_into(FilterType::HighPass, sr, 30.0, 0.707, 0.0, plane);
                Biquad::process_into(FilterType::Peaking, sr, 220.0, 0.9, -1.0, plane);
                let mut comp = Compressor::new(
                    sr,
                    CompressorParams {
                        threshold_db: -20.0,
                        ratio: 4.0,
                        knee_db: 3.0,
                        attack_ms: 10.0,
                        release_ms: 130.0,
                        makeup_db: 1.5,
                    },
                );
                for s in plane.iter_mut() {
                    *s *= comp.process(*s);
                }
                // 用单声道低频替换各声道低频，保证低频重心稳定（相位一致）。
                if let Some(m) = &mono_low {
                    let mut hi = plane.clone();
                    Biquad::process_into(FilterType::HighPass, sr, 160.0, 0.707, 0.0, &mut hi);
                    let _ = ci;
                    for i in 0..plane.len().min(m.len()) {
                        plane[i] = m[i] + hi[i];
                    }
                }
            }
        }
        Instrument::Instrumental => {
            // 伴奏/器乐：中频雕刻为人声让路 + 稳定压缩（保留原两轨兼容行为）。
            for plane in planes.iter_mut() {
                if req.accompaniment_carve_db != 0.0 {
                    Biquad::process_into(FilterType::Peaking, sr, 2500.0, 0.7, req.accompaniment_carve_db, plane);
                }
                let mut comp = Compressor::new(
                    sr,
                    CompressorParams {
                        threshold_db: -20.0,
                        ratio: 2.0,
                        knee_db: 6.0,
                        attack_ms: 15.0,
                        release_ms: 180.0,
                        makeup_db: 0.5,
                    },
                );
                for s in plane.iter_mut() {
                    *s *= comp.process(*s);
                }
            }
        }
        Instrument::Other => {
            // 其他乐器（吉他/键盘等）：温和 EQ + 轻压缩。
            for plane in planes.iter_mut() {
                Biquad::process_into(FilterType::HighPass, sr, 60.0, 0.707, 0.0, plane);
                Biquad::process_into(FilterType::Peaking, sr, 1200.0, 0.8, 0.8, plane);
                Biquad::process_into(FilterType::HighShelf, sr, 8000.0, 0.707, 1.0, plane);
                let mut comp = Compressor::new(
                    sr,
                    CompressorParams {
                        threshold_db: -20.0,
                        ratio: 2.0,
                        knee_db: 8.0,
                        attack_ms: 20.0,
                        release_ms: 200.0,
                        makeup_db: 0.5,
                    },
                );
                for s in plane.iter_mut() {
                    *s *= comp.process(*s);
                }
            }
        }
        Instrument::Auto => {
            // 理论上不会到这里（已在 resolve_instrument 解析），安全兜底。
        }
    }
}

/// 执行一键混音（无进度回调）。
pub fn mix(req: &MixRequest) -> Result<MixResult> {
    mix_with_progress(req, None::<fn(ProgressEvent)>)
}

/// 执行一键混音，并通过闭包上报进度。
pub fn mix_with_progress<F>(req: &MixRequest, progress: Option<F>) -> Result<MixResult>
where
    F: FnMut(ProgressEvent),
{
    mix_with_progress_cancel(req, progress, None)
}

/// 带取消标志的混音。取消会在阶段边界与写盘前生效，不会留下半成品文件。
pub fn mix_with_progress_cancel<F>(
    req: &MixRequest,
    mut progress: Option<F>,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> Result<MixResult>
where
    F: FnMut(ProgressEvent),
{
    use std::sync::atomic::Ordering;
    macro_rules! check_cancel {
        () => {
            if let Some(flag) = cancel {
                if flag.load(Ordering::Relaxed) {
                    return Err(MixError::Cancelled);
                }
            }
        };
    }
    let started = Instant::now();
    let mut stages: Vec<String> = Vec::new();

    // 0. 入口校验（必须在任何 DSP 之前，否则非法采样率会触发滤波器 panic 或巨量分配）。
    if let Some(sr) = req.output_sample_rate {
        if !(8_000..=192_000).contains(&sr) {
            return Err(MixError::InvalidParam(format!(
                "output_sample_rate 必须在 8000..=192000 之间，收到 {}",
                sr
            )));
        }
    }
    if !matches!(req.output_bit_depth, 16 | 24 | 32) {
        return Err(MixError::InvalidParam(format!(
            "output_bit_depth 只支持 16 / 24 / 32(float)，收到 {}",
            req.output_bit_depth
        )));
    }
    if !(-40.0..=0.0).contains(&req.target_lufs) {
        return Err(MixError::InvalidParam(format!(
            "target_lufs 必须在 -40..=0 之间，收到 {}",
            req.target_lufs
        )));
    }
    if !(-12.0..=0.0).contains(&req.true_peak_ceiling) {
        return Err(MixError::InvalidParam(format!(
            "true_peak_ceiling 必须在 -12..=0 dBTP 之间，收到 {}",
            req.true_peak_ceiling
        )));
    }

    // 1. 应用预设（一次性、五项全应用）。用"是否等于默认值"区分用户是否显式设置：
    //    这是当前 schema 的已知局限（没有 Option 标记），文档中已如实说明。
    let mut req = req.clone();
    if !req.preset.trim().is_empty() {
        let p = Preset::from_name(&req.preset).ok_or_else(|| {
            MixError::InvalidParam(format!(
                "未知预设: {}（可选: streaming / broadcast / quiet / loud）",
                req.preset
            ))
        })?;
        let mut base = MixRequest::default();
        p.apply(&mut base);
        let d = MixRequest::default();
        if req.target_lufs == d.target_lufs {
            req.target_lufs = base.target_lufs;
        }
        if req.true_peak_ceiling == d.true_peak_ceiling {
            req.true_peak_ceiling = base.true_peak_ceiling;
        }
        if req.ducking_db == d.ducking_db {
            req.ducking_db = base.ducking_db;
        }
        if req.bus_glue == d.bus_glue {
            req.bus_glue = base.bus_glue;
        }
        if req.stereo_width == d.stereo_width {
            req.stereo_width = base.stereo_width;
        }
    }

    check_cancel!();

    // 2. 解析输入分轨列表。
    //    Stems 模式：优先 stems[]；为空则回退到传统 vocals + accompaniment 两轨。
    //    Separate 模式：输入成品 source_path，先尝试分离（权重不可用时降级）。
    let mut inputs: Vec<StemInput> = Vec::new();
    let mut separation_applied = false;
    let mut separation_model_id: Option<String> = None;

    let mut stage_note: Option<&str> = None;
    match req.mode {
        MixMode::Stems => {
            if !req.stems.is_empty() {
                inputs = req.stems.clone();
            } else {
                // 向后兼容：传统两轨。
                if req.vocals_path.is_empty() && req.accompaniment_path.is_empty() {
                    return Err(MixError::InvalidParam("分轨列表与人声/伴奏路径均为空".into()));
                }
                if !req.vocals_path.is_empty() {
                    inputs.push(StemInput {
                        path: req.vocals_path.clone(),
                        instrument: Instrument::Vocals,
                        // 人声增益由人声专属链统一应用一次；这里再放会变成双倍增益。
                        gain_db: 0.0,
                    });
                }
                if !req.accompaniment_path.is_empty() {
                    inputs.push(StemInput {
                        path: req.accompaniment_path.clone(),
                        instrument: Instrument::Instrumental,
                        gain_db: 0.0,
                    });
                }
            }
        }
        MixMode::Separate => {
            let src = if req.source_path.is_empty() {
                req.vocals_path.clone()
            } else {
                req.source_path.clone()
            };
            if src.is_empty() {
                return Err(MixError::InvalidParam("分离模式需要指定 source_path（成品混音）".into()));
            }
            // 分离模型权重未随原型分发：把成品当作一条器乐轨，走「器乐链 + 总线母带链」，
            // 并明确告知未真正分离。分离字段保持 false/None，绝不假装跑过模型。
            let model_id = req.separation_model.clone().unwrap_or_else(|| "htdemucs-ft".to_string());
            let _ = model_id;
            inputs.push(StemInput {
                path: src,
                instrument: Instrument::Instrumental,
                gain_db: 0.0,
            });
            emit(
                &mut progress,
                8,
                "分离模型检查",
                Some("分离权重未随原型分发，已降级为器乐链 + 总线母带；如需真正分离请参考文档下载 SOTA 权重".into()),
            );
            stage_note = Some("分离模型不可用(降级为器乐链 + 总线母带)");
            separation_applied = false;
            separation_model_id = None;
        }
    }

    if inputs.is_empty() {
        return Err(MixError::InvalidParam("没有任何输入分轨".into()));
    }
    for inp in &inputs {
        if inp.path.is_empty() {
            return Err(MixError::InvalidParam("存在路径为空的分轨".into()));
        }
        if !std::path::Path::new(&inp.path).exists() {
            return Err(MixError::InvalidParam(format!("分轨文件不存在: {}", inp.path)));
        }
    }
    if req.output_path.is_empty() {
        return Err(MixError::InvalidParam("输出路径不能为空".into()));
    }
    if let Some(note) = stage_note {
        stages.push(note.to_string());
    }
    let mut mark = |s: &str, cb: &mut Option<F>, p: u32| {
        stages.push(s.to_string());
        emit(cb, p, s, None);
    };
    mark("校验参数与预设", &mut progress, 3);

    // 2. 读取所有分轨。
    emit(&mut progress, 6, "读取音频", Some(format!("解码 {} 条分轨", inputs.len())));
    let mut bufs: Vec<AudioBuffer> = Vec::with_capacity(inputs.len());
    for inp in &inputs {
        bufs.push(read_wav(&inp.path)?);
    }

    check_cancel!();

    // 3. 统一采样率与声道。
    let sr = req.output_sample_rate.unwrap_or_else(|| {
        bufs.iter().map(|b| b.sample_rate).max().unwrap_or(48000)
    });
    for b in &mut bufs {
        *b = resample_buffer(b, sr);
        *b = to_stereo(b);
    }
    // 以最长轨为准对齐所有轨。
    let max_frames = bufs.iter().map(|b| b.frames()).max().unwrap_or(0);
    for b in &mut bufs {
        let ref_buf = AudioBuffer::from_planar(vec![vec![0.0f32; max_frames]; 2], sr);
        match_length(b, &ref_buf);
    }
    mark("统一采样率/声道/长度", &mut progress, 12);

    // 记录乐器解析结果。
    let mut stems_used: Vec<StemInfo> = Vec::with_capacity(inputs.len());
    let mut instruments: Vec<Instrument> = Vec::with_capacity(inputs.len());
    for (inp, _buf) in inputs.iter().zip(bufs.iter()) {
        let (inst, inferred) = resolve_instrument(inp);
        instruments.push(inst);
        stems_used.push(StemInfo {
            path: inp.path.clone(),
            instrument: inst,
            inferred,
            gain_db: inp.gain_db,
        });
    }

    // 输入响度度量（人声轨与器乐轨分别合并度量，兼容原字段语义）。
    let input_vocals_lufs = {
        let idx: Vec<usize> = instruments
            .iter()
            .enumerate()
            .filter(|(_, i)| **i == Instrument::Vocals)
            .map(|(k, _)| k)
            .collect();
        if idx.is_empty() {
            -100.0
        } else {
            let p = bufs[idx[0]].to_planar();
            integrated_lufs(&p, sr as f32)
        }
    };
    let input_accompaniment_lufs = {
        let idx = instruments
            .iter()
            .position(|i| *i != Instrument::Vocals)
            .unwrap_or(0);
        let p = bufs[idx].to_planar();
        integrated_lufs(&p, sr as f32)
    };

    check_cancel!();

    // 4. NN 增强（对人声轨应用；其余轨保持原样）。
    let nn_applied;
    let nn_model_id;
    #[cfg(feature = "nn")]
    {
        if req.nn_enhance {
            emit(&mut progress, 15, "NN 音质增强", Some("ONNX 推理中".into()));
            let mut any = false;
            let mut mid: Option<String> = None;
            for (k, inst) in instruments.iter().enumerate() {
                if *inst == Instrument::Vocals {
                    let enhanced = crate::nn::enhance(
                        bufs[k].clone(),
                        req.nn_model.as_deref(),
                        req.nn_mix,
                        req.models_dir.as_deref(),
                    );
                    bufs[k] = enhanced.vocals;
                    if enhanced.applied {
                        any = true;
                        mid = enhanced.model_id;
                    }
                }
            }
            nn_applied = any;
            nn_model_id = mid;
        } else {
            nn_applied = false;
            nn_model_id = None;
        }
    }
    #[cfg(not(feature = "nn"))]
    {
        nn_applied = false;
        nn_model_id = None;
    }
    mark(if nn_applied { "NN 音质增强(完成)" } else { "NN 增强(跳过)" }, &mut progress, 25);

    // 5. 逐轨专属处理链。
    let pct_base = 30.0;
    let pct_step = 25.0 / inputs.len() as f32;
    let mut processed: Vec<AudioBuffer> = Vec::with_capacity(bufs.len());
    for (k, buf) in bufs.iter_mut().enumerate() {
        check_cancel!();
        let inst = instruments[k];
        let gain = db_to_linear(stems_used[k].gain_db);
        let mut planes = buf.to_planar();
        process_instrument(&mut planes, inst, sr as f32, &req);
        if gain != 1.0 {
            for plane in &mut planes {
                for s in plane.iter_mut() {
                    *s *= gain;
                }
            }
        }
        processed.push(AudioBuffer::from_planar(planes, sr));
        emit(
            &mut progress,
            (pct_base + pct_step * (k + 1) as f32) as u32,
            "处理分轨",
            Some(format!("{} → {}", inputs[k].path.rsplit('/').next().unwrap_or("?"), inst.as_str())),
        );
    }
    mark("逐轨专属 DSP 链", &mut progress, 55);

    // 6. 侧链 ducking：以所有人声轨的合成单声道为侧链，对非人声轨做让位。
    emit(&mut progress, 60, "侧链 ducking", None);
    let vocal_mono: Option<Vec<f32>> = {
        let idx: Vec<usize> = instruments
            .iter()
            .enumerate()
            .filter(|(_, i)| **i == Instrument::Vocals)
            .map(|(k, _)| k)
            .collect();
        if idx.is_empty() {
            None
        } else {
            let n = processed.iter().map(|b| b.frames()).min().unwrap_or(0);
            let mut m = vec![0.0f32; n];
            for k in idx {
                let vm = processed[k].downmix_mono();
                for i in 0..n.min(vm.len()) {
                    m[i] += vm[i];
                }
            }
            Some(m)
        }
    };
    if let Some(vm) = &vocal_mono {
        for (k, buf) in processed.iter_mut().enumerate() {
            if instruments[k] == Instrument::Vocals {
                continue;
            }
            let mut planes = buf.to_planar();
            for plane in &mut planes {
                let mut ducker = SidechainDucker::new(sr as f32, req.ducking_db, 20.0, 250.0);
                for i in 0..plane.len() {
                    let g = ducker.process(vm.get(i).copied().unwrap_or(0.0));
                    plane[i] *= g;
                }
            }
            *buf = AudioBuffer::from_planar(planes, sr);
        }
    }
    mark("侧链 ducking(人声主控)", &mut progress, 65);

    check_cancel!();

    // 7. 总线混合（所有分轨相加）。
    emit(&mut progress, 70, "总线混合", None);
    let all_planes: Vec<Vec<Vec<f32>>> = processed.iter().map(|b| b.to_planar()).collect();
    let frames = all_planes.iter().map(|p| p.first().map(|v| v.len()).unwrap_or(0)).min().unwrap_or(0);
    let mut mix_planes = vec![vec![0.0f32; frames]; 2];
    for planes in &all_planes {
        for c in 0..2 {
            if let Some(src) = planes.get(c) {
                for i in 0..frames.min(src.len()) {
                    mix_planes[c][i] += src[i];
                }
            }
        }
    }
    mark("总线混合", &mut progress, 75);

    // 8. Glue 压缩。每个声道独立实例，避免 L→R 继承包络造成声像漂移。
    if req.bus_glue > 0.0 {
        emit(&mut progress, 78, "总线 glue 压缩", None);
        for plane in &mut mix_planes {
            let mut glue = Compressor::new(
                sr as f32,
                CompressorParams {
                    threshold_db: -16.0,
                    ratio: 1.0 + req.bus_glue * 2.0,
                    knee_db: 8.0,
                    attack_ms: 20.0,
                    release_ms: 200.0,
                    makeup_db: 0.0,
                },
            );
            for s in plane.iter_mut() {
                *s *= glue.process(*s);
            }
        }
    }

    // 9. 立体声宽度。
    if req.stereo_width != 1.0 {
        let mut buf = AudioBuffer::from_planar(mix_planes.clone(), sr);
        StereoWidener::new(req.stereo_width).process(&mut buf.samples);
        mix_planes = buf.to_planar();
    }
    mark("总线压缩与立体声", &mut progress, 82);

    // 10. 响度标准化（闭环：限幅会降低响度，因此测→增益→限幅→复测，迭代补偿）。
    emit(&mut progress, 85, "响度标准化", Some(format!("目标 {} LUFS", req.target_lufs)));
    let premaster_lufs = integrated_lufs(&mix_planes, sr as f32);
    let mut gain_db = (req.target_lufs - premaster_lufs).clamp(-24.0, 24.0);
    let gain_lin = db_to_linear(gain_db);
    for plane in &mut mix_planes {
        for s in plane.iter_mut() {
            *s *= gain_lin;
        }
    }

    // 11. 真峰限幅（逐声道独立实例）。
    emit(&mut progress, 92, "真峰限幅", None);
    limit_planes(&mut mix_planes, sr as f32, req.true_peak_ceiling);
    let mut output_lufs = integrated_lufs(&mix_planes, sr as f32);

    // 若限幅把响度压下去了，再补增益并重新限幅；最多 2 轮，避免无限拉锯。
    for _ in 0..2 {
        let err = req.target_lufs - output_lufs;
        if err.abs() <= 0.3 {
            break;
        }
        let extra = err.clamp(-6.0, 6.0);
        let extra_lin = db_to_linear(extra);
        for plane in &mut mix_planes {
            for s in plane.iter_mut() {
                *s *= extra_lin;
            }
        }
        limit_planes(&mut mix_planes, sr as f32, req.true_peak_ceiling);
        gain_db += extra;
        output_lufs = integrated_lufs(&mix_planes, sr as f32);
    }
    mark("响度标准化与限幅", &mut progress, 96);

    check_cancel!();

    // 12. 导出。
    emit(&mut progress, 97, "导出 WAV", None);
    let out_buf = AudioBuffer::from_planar(mix_planes, sr);
    write_wav(&req.output_path, &out_buf, req.output_bit_depth)?;

    // 最终度量（从写出文件读回）。
    let final_buf = read_wav(&req.output_path)?;
    let final_planes = final_buf.to_planar();
    let output_lufs = integrated_lufs(&final_planes, sr as f32);
    let output_true_peak = true_peak_dbtp(&final_planes);
    let loudness_target_met = (output_lufs - req.target_lufs).abs() <= 0.5;

    emit(&mut progress, 100, "完成", None);

    Ok(MixResult {
        ok: true,
        output_path: req.output_path.clone(),
        sample_rate: sr,
        channels: 2,
        duration_sec: out_buf.duration_sec(),
        input_vocals_lufs,
        input_accompaniment_lufs,
        premaster_lufs,
        output_lufs,
        output_true_peak_dbtp: output_true_peak,
        normalization_gain_db: gain_db,
        nn_applied,
        nn_model_id,
        elapsed_ms: started.elapsed().as_millis() as u64,
        preset: req.preset.clone(),
        stages,
        mode: req.mode,
        stems_used,
        separation_applied,
        separation_model_id,
        loudness_target_met,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f32, secs: f32, sr: u32, amp: f32) -> AudioBuffer {
        let n = (secs * sr as f32) as usize;
        let mut s = Vec::with_capacity(n * 2);
        for i in 0..n {
            let v = (2.0 * std::f32::consts::PI * freq * i as f32 / sr as f32).sin() * amp;
            s.push(v);
            s.push(v);
        }
        AudioBuffer {
            samples: s,
            channels: 2,
            sample_rate: sr,
        }
    }

    fn tmp() -> std::path::PathBuf {
        // 每个测试用独立目录，避免并行执行时互相覆盖输出文件。
        use std::sync::atomic::{AtomicUsize, Ordering};
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("onemix_test_{}_{}", std::process::id(), n));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn test_legacy_two_track_still_works() {
        let d = tmp();
        let v = d.join("vocals.wav");
        let a = d.join("accom.wav");
        let o = d.join("out_legacy.wav");
        write_wav(v.to_str().unwrap(), &sine(440.0, 2.0, 48000, 0.2), 16).unwrap();
        write_wav(a.to_str().unwrap(), &sine(220.0, 2.0, 48000, 0.15), 16).unwrap();
        let req = MixRequest {
            vocals_path: v.to_str().unwrap().into(),
            accompaniment_path: a.to_str().unwrap().into(),
            output_path: o.to_str().unwrap().into(),
            preset: "streaming".into(),
            ..Default::default()
        };
        let res = mix(&req).unwrap();
        assert!(res.ok);
        assert_eq!(res.mode, MixMode::Stems);
        assert_eq!(res.stems_used.len(), 2);
        assert_eq!(res.stems_used[0].instrument, Instrument::Vocals);
        assert_eq!(res.stems_used[1].instrument, Instrument::Instrumental);
        assert!(!res.separation_applied);
        assert!(res.output_lufs > -30.0 && res.output_lufs < -5.0);
    }

    #[test]
    fn test_stems_mode_four_tracks_with_inference() {
        let d = tmp();
        let specs = [
            ("vocals.wav", 440.0, Instrument::Vocals),
            ("drums.wav", 120.0, Instrument::Drums),
            ("bass.wav", 80.0, Instrument::Bass),
            ("guitar_other.wav", 330.0, Instrument::Other),
        ];
        let mut stems = Vec::new();
        for (name, freq, _inst) in &specs {
            let p = d.join(name);
            write_wav(p.to_str().unwrap(), &sine(*freq, 2.0, 48000, 0.12), 16).unwrap();
            stems.push(StemInput {
                path: p.to_str().unwrap().into(),
                instrument: Instrument::Auto,
                gain_db: 0.0,
            });
        }
        let o = d.join("out_stems.wav");
        let req = MixRequest {
            mode: MixMode::Stems,
            stems,
            output_path: o.to_str().unwrap().into(),
            preset: "streaming".into(),
            ..Default::default()
        };
        let res = mix(&req).unwrap();
        assert!(res.ok);
        assert_eq!(res.stems_used.len(), 4);
        // 验证文件名推断。
        assert_eq!(res.stems_used[0].instrument, Instrument::Vocals);
        assert_eq!(res.stems_used[1].instrument, Instrument::Drums);
        assert_eq!(res.stems_used[2].instrument, Instrument::Bass);
        assert_eq!(res.stems_used[3].instrument, Instrument::Other);
        assert!(res.stems_used.iter().all(|s| s.inferred));
        assert!(!res.separation_applied);
        assert!((res.output_lufs - (-14.0)).abs() < 0.5);
        assert!(res.output_true_peak_dbtp < 0.0);
    }

    #[test]
    fn test_separate_mode_graceful_degradation() {
        let d = tmp();
        let src = d.join("fullmix.wav");
        write_wav(src.to_str().unwrap(), &sine(330.0, 1.5, 44100, 0.2), 16).unwrap();
        let o = d.join("out_sep.wav");
        let req = MixRequest {
            mode: MixMode::Separate,
            source_path: src.to_str().unwrap().into(),
            output_path: o.to_str().unwrap().into(),
            preset: "streaming".into(),
            ..Default::default()
        };
        let res = mix(&req).unwrap();
        assert!(res.ok);
        assert_eq!(res.mode, MixMode::Separate);
        assert!(!res.separation_applied, "原型分离权重未打包，应降级");
        assert_eq!(res.stems_used.len(), 1);
        assert!((res.output_lufs - (-14.0)).abs() < 0.5);
    }

    #[test]
    fn test_cancel_before_write_leaves_no_file() {
        use std::sync::atomic::AtomicBool;
        let d = tmp();
        let v = d.join("cv.wav");
        let a = d.join("ca.wav");
        let o = d.join("cancelled.wav");
        write_wav(v.to_str().unwrap(), &sine(440.0, 1.0, 48000, 0.2), 16).unwrap();
        write_wav(a.to_str().unwrap(), &sine(220.0, 1.0, 48000, 0.15), 16).unwrap();
        let req = MixRequest {
            vocals_path: v.to_str().unwrap().into(),
            accompaniment_path: a.to_str().unwrap().into(),
            output_path: o.to_str().unwrap().into(),
            ..Default::default()
        };
        let flag = AtomicBool::new(true);
        let res = mix_with_progress_cancel(&req, None::<fn(ProgressEvent)>, Some(&flag));
        assert!(matches!(res, Err(MixError::Cancelled)), "应返回 Cancelled");
        assert!(!o.exists(), "取消后不应留下输出文件");
    }

    #[test]
    fn test_invalid_sample_rate_rejected() {
        let d = tmp();
        let v = d.join("sv.wav");
        let a = d.join("sa.wav");
        write_wav(v.to_str().unwrap(), &sine(440.0, 0.5, 48000, 0.2), 16).unwrap();
        write_wav(a.to_str().unwrap(), &sine(220.0, 0.5, 48000, 0.1), 16).unwrap();
        let req = MixRequest {
            vocals_path: v.to_str().unwrap().into(),
            accompaniment_path: a.to_str().unwrap().into(),
            output_path: d.join("bad.wav").to_str().unwrap().into(),
            output_sample_rate: Some(1),
            ..Default::default()
        };
        assert!(matches!(mix(&req), Err(MixError::InvalidParam(_))));
    }

    #[test]
    fn test_short_audio_still_normalizes() {
        let d = tmp();
        let v = d.join("shortv.wav");
        let a = d.join("shorta.wav");
        let o = d.join("short.wav");
        write_wav(v.to_str().unwrap(), &sine(440.0, 0.15, 48000, 0.3), 16).unwrap();
        write_wav(a.to_str().unwrap(), &sine(220.0, 0.15, 48000, 0.2), 16).unwrap();
        let req = MixRequest {
            vocals_path: v.to_str().unwrap().into(),
            accompaniment_path: a.to_str().unwrap().into(),
            output_path: o.to_str().unwrap().into(),
            ..Default::default()
        };
        let r = mix(&req).unwrap();
        assert!(r.loudness_target_met, "短音频也应达到目标响度，实际 {:.2}", r.output_lufs);
        assert!(r.output_true_peak_dbtp <= -0.95, "真峰 {:.2}", r.output_true_peak_dbtp);
    }

}
