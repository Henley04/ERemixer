//! ONNX Runtime 进程级环境：原生库探测 + 单例 Environment。
//!
//! ORT 的 C API 要求一个进程只能创建一个 Environment，因此这里用 `OnceLock`
//! 共享；首次调用决定原生库路径，后续调用直接复用。

use crate::error::{MixError, Result};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[cfg(target_os = "windows")]
const LIB: &str = "onnxruntime.dll";
#[cfg(target_os = "macos")]
const LIB: &str = "libonnxruntime.dylib";
#[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
const LIB: &str = "libonnxruntime.so";

/// 探测原生 ONNX Runtime 库路径（发布包布局：二进制同级 `models/native/`）。
pub(crate) fn find_native_lib(models_dir: Option<&str>) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(d) = models_dir {
        candidates.push(Path::new(d).join("native").join(LIB));
        candidates.push(Path::new(d).join(LIB));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(root) = exe.parent() {
            candidates.push(root.join("models/native").join(LIB));
            candidates.push(root.join("models").join(LIB));
            candidates.push(root.join(LIB));
            candidates.push(root.join("../models/native").join(LIB));
            candidates.push(root.join("../../models/native").join(LIB));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("models/native").join(LIB));
        candidates.push(cwd.join(LIB));
    }
    candidates.into_iter().find(|p| p.exists())
}

/// 获取（或初始化）进程级 Environment。
pub(crate) fn env(models_dir: Option<&str>) -> Result<&'static ort::environment::Environment> {
    static ENV: OnceLock<std::result::Result<ort::environment::Environment, String>> = OnceLock::new();
    let entry = ENV.get_or_init(|| {
        let built = match find_native_lib(models_dir) {
            Some(lib) => ort::init_from(&lib)
                .map_err(|e| format!("加载 ONNX Runtime 原生库失败: {}", e))?
                .with_name("onemix")
                .build(),
            None => ort::init().with_name("onemix").build(),
        };
        built.map_err(|e| format!("ORT 环境初始化失败: {}", e))
    });
    match entry {
        Ok(e) => Ok(e),
        Err(msg) => Err(MixError::Inference(msg.clone())),
    }
}
