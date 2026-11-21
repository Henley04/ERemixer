//! NN 增强层：通过 ONNX Runtime 动态加载原生库，执行轻量音质增强模型推理。
//!
//! - 纯 DSP 混音不依赖本模块（关闭 `nn` feature 即可零原生依赖编译）。
//! - 原生库路径自动探测顺序：显式 models_dir → 可执行文件同级 models/native → 环境变量。

pub mod registry;

#[cfg(feature = "nn")]
mod imp {
    use crate::error::{MixError, Result};
    use crate::nn::registry::{find_model, ModelEntry, DEFAULT_ENHANCE_MODEL};
    use std::path::{Path, PathBuf};

    pub struct Enhanced {
        /// 干声（可能已被增强）。
        pub vocals: crate::AudioBuffer,
        pub applied: bool,
        pub model_id: Option<String>,
    }

    /// 各平台 ONNX Runtime 动态库文件名。
    #[cfg(target_os = "windows")]
    const ORT_LIB_FILE: &str = "onnxruntime.dll";
    #[cfg(target_os = "macos")]
    const ORT_LIB_FILE: &str = "libonnxruntime.dylib";
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    const ORT_LIB_FILE: &str = "libonnxruntime.so";

    /// 探测原生 ONNX Runtime 库路径（发布包布局：二进制同级 `models/native/`）。
    fn find_native_lib(models_dir: Option<&str>) -> Option<PathBuf> {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Some(d) = models_dir {
            candidates.push(Path::new(d).join("native").join(ORT_LIB_FILE));
            candidates.push(Path::new(d).join(ORT_LIB_FILE));
        }
        if let Ok(exe) = std::env::current_exe() {
            if let Some(root) = exe.parent() {
                candidates.push(root.join("models/native").join(ORT_LIB_FILE));
                candidates.push(root.join("models").join(ORT_LIB_FILE));
                candidates.push(root.join(ORT_LIB_FILE));
                candidates.push(root.join("../models/native").join(ORT_LIB_FILE));
                candidates.push(root.join("../../models/native").join(ORT_LIB_FILE));
            }
        }
        if let Ok(cwd) = std::env::current_dir() {
            candidates.push(cwd.join("models/native").join(ORT_LIB_FILE));
            candidates.push(cwd.join(ORT_LIB_FILE));
        }
        candidates.into_iter().find(|p| p.exists())
    }

    /// 定位 ONNX 权重文件。
    fn find_model_file(models_dir: Option<&str>, entry: &ModelEntry) -> Option<PathBuf> {
        let file = entry.local_file.as_ref()?;
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Some(d) = models_dir {
            candidates.push(Path::new(d).join(file));
        }
        if let Ok(exe) = std::env::current_exe() {
            if let Some(root) = exe.parent() {
                candidates.push(root.join("models").join(file));
                candidates.push(root.join("../models").join(file));
                candidates.push(root.join("../../models").join(file));
            }
        }
        if let Ok(cwd) = std::env::current_dir() {
            candidates.push(cwd.join("models").join(file));
        }
        candidates.into_iter().find(|p| p.exists())
    }

    /// 从能量包络构造输入特征：128 帧的 RMS 包络（0..1）。
    fn envelope(samples: &[f32], channels: u16, frames_target: usize) -> Vec<f32> {
        let planar = {
            let buf = crate::AudioBuffer {
                samples: samples.to_vec(),
                channels,
                sample_rate: 48000,
            };
            buf.downmix_mono()
        };
        let frames = planar.len();
        let win = (frames / frames_target).max(1);
        let mut env = Vec::with_capacity(frames_target);
        let mut peak = 0.0f32;
        for i in 0..frames_target {
            // 短音频时 win=1，i 可能越过 frames；必须夹紧后再切片。
            let start = (i * win).min(frames);
            let end = ((i + 1) * win).min(frames);
            let mut sum = 0.0f64;
            if end > start {
                for &s in &planar[start..end] {
                    sum += s as f64 * s as f64;
                }
            }
            let n = end.saturating_sub(start).max(1);
            let rms = (sum / n as f64).sqrt() as f32;
            peak = peak.max(rms);
            env.push(rms);
        }
        if peak > 0.0 {
            for v in &mut env {
                *v = (*v / peak).clamp(0.0, 1.0);
            }
        }
        env
    }

    /// 将帧级增益插值回每个采样。
    fn expand_gain(frame_gain: &[f32], total_samples: usize) -> Vec<f32> {
        let n = frame_gain.len();
        if n == 0 {
            return vec![1.0; total_samples];
        }
        let mut out = Vec::with_capacity(total_samples);
        for i in 0..total_samples {
            let pos = i as f32 / total_samples as f32 * (n - 1) as f32;
            let i0 = pos.floor() as usize;
            let f = pos - i0 as f32;
            let g0 = frame_gain[i0.min(n - 1)];
            let g1 = frame_gain[(i0 + 1).min(n - 1)];
            out.push(g0 + (g1 - g0) * f);
        }
        out
    }

    /// 尝试对干声施加 NN 增强；任何失败都安全降级为不增强（返回 applied=false）。
    pub fn enhance(
        vocals: crate::AudioBuffer,
        model_id: Option<&str>,
        wet: f32,
        models_dir: Option<&str>,
    ) -> Enhanced {
        let requested = model_id.unwrap_or(DEFAULT_ENHANCE_MODEL);
        let entry = match find_model(requested) {
            Some(e) => e,
            None => {
                return Enhanced {
                    vocals,
                    applied: false,
                    model_id: None,
                }
            }
        };

        let model_file = match find_model_file(models_dir, &entry) {
            Some(p) => p,
            None => {
                return Enhanced {
                    vocals,
                    applied: false,
                    model_id: None,
                }
            }
        };

        let run = || -> Result<crate::AudioBuffer> {
            // 动态加载原生库：优先显式加载，避免全局环境变量竞争。
            let environment = if let Some(lib) = find_native_lib(models_dir) {
                ort::init_from(&lib)
                    .map_err(|e| MixError::Inference(format!("加载 ONNX Runtime 原生库失败: {}", e)))?
                    .with_name("onemix")
                    .build()
                    .map_err(|e| MixError::Inference(format!("ORT 环境初始化失败: {}", e)))?
            } else {
                ort::init()
                    .with_name("onemix")
                    .build()
                    .map_err(|e| MixError::Inference(format!("ORT 环境初始化失败(未找到原生库): {}", e)))?
            };

            let mut builder = ort::session::Session::builder(&environment)
                .map_err(|e| MixError::Inference(format!("会话构建器失败: {}", e)))?;
            let mut session = builder
                .commit_from_file(&model_file)
                .map_err(|e| MixError::Inference(format!("模型加载失败: {}", e)))?;

            let sr = vocals.sample_rate;
            let channels = vocals.channels;
            let env_feat = envelope(&vocals.samples, channels, 128);

            // 输入张量 [1, 1, 128] —— OwnedTensorArrayData 要求 (shape, Vec<T>)。
            let input = ort::value::Tensor::from_array((
                [1usize, 1usize, env_feat.len()],
                env_feat.clone(),
            ))
            .map_err(|e| MixError::Inference(format!("输入张量构造失败: {}", e)))?;

            // run 接受 [SessionInputValue; N]；Tensor 通过 .into() 转换。
            let input_value: ort::session::SessionInputValue = input.into();
            let outputs = session
                .run([input_value])
                .map_err(|e| MixError::Inference(format!("推理失败: {}", e)))?;
            // DynValue 支持 try_extract_tensor。
            let (_, gains_slice) = outputs[0]
                .try_extract_tensor::<f32>()
                .map_err(|e| MixError::Inference(format!("输出解析失败: {}", e)))?;
            let gains: Vec<f32> = gains_slice.to_vec();

            // 构造帧增益：1 + wet*model_output，并做柔和限幅。
            let frame_gain: Vec<f32> = gains
                .iter()
                .map(|g| (1.0 + wet * *g).clamp(0.5, 1.5))
                .collect();
            // 逐声道展开帧增益：交错样本上展开会让 L 只用前半段曲线、R 只用后半段。
            let planes = vocals.to_planar();
            let mut enhanced_planes = Vec::with_capacity(planes.len());
            for plane in planes {
                let sample_gain = expand_gain(&frame_gain, plane.len());
                let out: Vec<f32> = plane
                    .iter()
                    .zip(sample_gain.iter())
                    .map(|(s, g)| s * g)
                    .collect();
                enhanced_planes.push(out);
            }
            Ok(crate::AudioBuffer::from_planar(enhanced_planes, sr))
        };

        match run() {
            Ok(buf) => Enhanced {
                vocals: buf,
                applied: true,
                model_id: Some(entry.id),
            },
            Err(e) => {
                eprintln!("[onemix] NN 增强降级为关闭: {}", e);
                Enhanced {
                    vocals,
                    applied: false,
                    model_id: None,
                }
            }
        }
    }
}

#[cfg(feature = "nn")]
pub use imp::{enhance, Enhanced};

#[cfg(not(feature = "nn"))]
mod stub {
    use crate::AudioBuffer;
    pub struct Enhanced {
        pub vocals: AudioBuffer,
        pub applied: bool,
        pub model_id: Option<String>,
    }
    pub fn enhance(
        vocals: AudioBuffer,
        _model_id: Option<&str>,
        _wet: f32,
        _models_dir: Option<&str>,
    ) -> Enhanced {
        Enhanced {
            vocals,
            applied: false,
            model_id: None,
        }
    }
}
#[cfg(not(feature = "nn"))]
pub use stub::{enhance, Enhanced};
