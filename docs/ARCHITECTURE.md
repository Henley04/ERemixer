# 架构与模型选型

## 1. 为什么是"统一引擎 + 三前端"

需求有两层：①一个高度自动化的一键混音软件（Rust 原生）；②为 SXSEditor 提供**能力完全一致**的混音插件。

关键约束来自对 SXSEditor 源码的实测：
- SXSEditor 插件运行在 **utilityProcess**，宿主**不提供 `process.spawn`**，检测到子进程即 quarantine → 插件**无法调用外部 Rust 二进制**。
- 插件可用能力：`ctx.fs`（文件读写）、`ctx.ort`（ONNX 推理）、`ctx.models`、`ctx.project`、`ctx.ui`、`ctx.commands`（经 `command.run`）。

因此"能力完全一致"的正确实现是：
- 核心混音能力用 Rust 实现（`mixer-core`），CLI 与 GUI 直接链接它；
- 插件内用 **JS 逐算法移植**同一套 DSP（`engine.js`），NN 增强用 `ctx.ort` 跑同一个 ONNX 模型；
- 三端共享同一 `MixRequest`/`MixResult` JSON schema 与参数默认值/预设。

实测同一素材三端均输出 -14.00 LUFS，验证了能力一致性。

## 2. SOTA NN 模型选型

混音中的"AI"能力主要落在两个环节：**音源/音质预处理** 与 **响度/动态决策**。本原型采用开源、可本地运行的模型：

| 模型 | 任务 | 架构 | 指标 / 来源 | 在本项目中的角色 |
|---|---|---|---|---|
| **BS-RoFormer** | 人声分离 | Band-Split RoPE Transformer | MUSDB18-HQ 当前 SOTA，人声 SDR ≈ 10.9 dB，drums 12.91 dB（[sota2](https://www.sota2.com/research/task/music-source-separation)、[HF 合集](https://huggingface.co/collections/StemSplitio/music-source-separation-toolkit-2026)） | 人声/伴奏不纯时的重分离入口（注册表项） |
| **HT-Demucs / htdemucs_ft** | 4 源分离 | Hybrid Transformer Demucs | ≈ 9.0 dB SDR，生产级默认，速度与质量均衡 | 通用分流入（注册表项） |
| **Mel-Band RoFormer** | 去混响 / 净化 | Mel-Band Split RoPE Transformer | 频带细分变体，人声去混响优于同族 | 混音前人声净化（注册表项） |
| **onemix-dyn-v1** | 音质增强 | Conv1d-ReLU-Conv1d-Tanh | 375B，逐帧动态增益网络 | **随原型分发、默认可用**的轻量 NN 增强 |

> 说明：完整音源分离模型（BS-RoFormer/Demucs）权重体积大（数十~数百 MB），原型中作为注册表项提供下载源（modelscope / hf-mirror 镜像）与接入点；实际一键混音默认使用随包分发的轻量增强模型 + 纯 DSP 链，保证零配置即可运行。

模型注册表代码：`crates/mixer-core/src/nn/registry.rs`，插件侧镜像：`sxseditor-plugin/engine.js` 的 `MODELS`。

ONNX Runtime 采用动态加载（`ort` crate `load-dynamic`），原生库位于 `models/native/`，可脱离 Python 环境运行；关闭 `nn` feature 时整个引擎零原生依赖。

## 3. DSP 决策（无需大模型的"自动化"核心）

一键混音的"智能"不仅来自 NN，更来自一套确定性的、面向人声+伴奏场景优化的 DSP 决策：

- **频率让位（frequency carving）**：伴奏在 2.5kHz（人声存在感区）做宽 Q 削减，人声在 250Hz 削减浑浊，二者互补，避免掩蔽。
- **侧链 ducking**：用人声包络实时压低伴奏，人声出现时自动让位，是广播/流媒体混音的常规手段。
- **响度标准化**：ITU-R BS.1770-4 K-weighting + EBU R128 绝对/相对门，精确落到目标 LUFS（默认 -14 流媒体）。
- **真峰控制**：4× 多相窗函数 sinc 插值检测样本间峰（不是样本峰）+ 前瞻限幅 + 终检微缩，保证重建后不越过 dBTP 上限。

## 4. 进度与一致性

`mix_with_progress` 通过闭包上报 `ProgressEvent{percent, stage, detail}`，三个前端分别渲染为：CLI 文本进度条、GUI 进度条、插件面板/ui.progress。所有阶段名与百分比在三端一致。
