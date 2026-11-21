# OneMix — 一键混音（分轨 / 成品 → 成品母带）

OneMix 是一个**高度自动化的本地混音系统**：既可以输入多条已分离的乐器分轨一键混成成品，也可以输入成品混音走"先分离再混"流程，最终得到响度标准化、真峰受控的成品 WAV；还能用 **StemFX 风格迁移**把参考曲的混音风格搬到自己的分轨上。全部采用**开源 SOTA 模型 + 纯 Rust DSP**，无任何云端依赖。同一套能力通过三个前端交付：**CLI 与插件共享完全相同的接口与算法**（已做逐位回归）；**GUI 是图形简化入口**，底层调用同一个引擎。

| 前端 | 形态 | 适用 |
|---|---|---|
| `mixer-app` | **Rust 原生桌面 GUI + CLI 双入口**（egui） | 终端用户图形操作（一键混音 / 风格迁移双标签页）/ 无头批处理（同一二进制） |
| `mixer-cli` | Rust 命令行 | 批处理 / 脚本 / 集成 |
| `sxseditor-plugin` | SXSEditor `.sxsp` 插件 | 在 SXSEditor 内混音，并暴露给 MCP/CLI 自动化 |

## 核心设计：一套引擎，三处消费

```
                  ┌─────────────────────────────┐
                  │   mixer-core (Rust 静态库)   │
                  │  · DSP 链（滤波/压缩/侧链）   │
                  │  · EBU R128 响度 / 真峰       │
                  │  · NN 适配层（ONNX Runtime）  │
                  │  · MixRequest → MixResult     │
                  └──────────────┬──────────────┘
          ┌────────────────────┼────────────────────┐
          ▼                    ▼                    ▼
   mixer-app (GUI)      mixer-cli (CLI)    插件 engine.js（逐算法移植）
   egui 原生桌面           JSON/批处理        ctx.fs + ctx.ort，utilityProcess
          └────────────────────┼────────────────────┘
                    共享同一 MixRequest/MixResult schema
```

**能力一致性保证**：三端使用相同的参数默认值、预设表、混音算法顺序与度量口径。已通过同一测试素材验证：Rust 与 JS 引擎均输出 **-14.00 LUFS**。

## 下载预编译版本

[GitHub Releases](https://github.com/Henley04/ERemixer/releases) 提供 Windows x64 与 Linux x86_64 发布包，内含 `mixer-cli`、`mixer-app`、ONNX Runtime 与增强模型，解压即用：

| 平台 | 文件 |
|---|---|
| Windows x64 | `ERemixer-v*-windows-x86_64.zip` |
| Linux x86_64 | `ERemixer-v*-linux-x86_64.tar.gz` |

推送 `v*` tag（或在 Actions 页面手动触发 `Release`）会自动完成双平台构建、打包与发布。

## 快速开始

### 1. 构建
```bash
# 在仓库根目录执行
cargo build --release            # 构建全部三个成员
# 或分别构建：
cargo build --release -p mixer-cli
cargo build --release -p mixer-app
```

### 2. 命令行一键混音

**分轨直入（推荐，跳过所有分离模型）：**

```bash
# 自动推断乐器类型（根据文件名：vocals/drums/bass/...）
./target/release/mixer-cli mix \
  --stems vocal.wav \
  --stems drums.wav \
  --stems bass.wav \
  --stems guitar.wav \
  -o final_mix.wav --json

# 或显式指定乐器：--stems path:instrument[:gainDb]
./target/release/mixer-cli mix \
  --stems lead.wav:vocals \
  --stems kit.wav:drums:1.5 \
  --stems bassline.wav:bass \
  --stems pad.wav:other \
  -o final_mix.wav
```

**传统两轨（向后兼容，等价于 vocals + instrumental 两条分轨）：**

```bash
./target/release/mixer-cli mix -v vocals.wav -a accompaniment.wav -o final_mix.wav --preset streaming
```

**成品分离再混（分离权重不可用时会明确降级）：**

```bash
./target/release/mixer-cli mix --mode separate --source full_song.wav -o final_mix.wav
```

**主应用 `mixer-app` 也支持 CLI**（同一二进制，检测到子命令即进入命令行，无参数则启动 GUI）：

```bash
./target/release/mixer-app mix --stems a.wav:vocals --stems b.wav:drums -o out.wav --json
./target/release/mixer-app modes        # 列出输入模式
./target/release/mixer-app instruments  # 列出乐器类型
```

### 3. 桌面 GUI
```bash
./target/release/mixer-app          # 或 ./target/release/mixer-app --gui 强制 GUI
```
选择人声、伴奏、输出路径 → 调参 → 点「⚡ 一键混音」。

### 4. SXSEditor 插件
将 `dist-plugins/onemix-mixer-0.1.0.sxsp`（**未签名主包**）拖入「插件 → 插件管理器 → 安装插件」即可。宿主默认放行未签名包；`-signed.sxsp` 用的是自签名密钥，需要先在管理器里核对指纹并「信任并安装」，否则会被 `unknown-key` 拦截。

插件读写音频依赖宿主当前的运行时边界（`ctx.fs` 只允许插件目录，绝对路径回退 `node:fs`），详见 `docs/SXSEDITOR_PLUGIN.md`。安装后可通过 MCP 调用：
```
onemix-mixer.mixFiles  {vocalsPath, accompanimentPath, outputPath, preset}
onemix-mixer.mix       {request: <MixRequest JSON>}
```

### 5. 风格迁移（StemFX）

把**参考曲的混音风格**迁移到自己的分轨上：模型读入源/参考两套 4 轨分轨的 mel 谱与 64 维混音特征，编码成 512 维风格向量，再由 6 层 Transformer 自回归"说出"一条 multiafx 效果器链，最后用纯 Rust DSP 把这条链渲染到自己的分轨上。

```bash
# 目录里放 vocals/bass/drums/other.wav（按文件名识别乐器）
./target/release/mixer-cli stemfx \
  --source-dir    my_song_stems \
  --reference-dir reference_stems \
  -o my_song_styled.wav --lufs -23

# 也可以逐轨显式指定（缺失轨按静音补齐——模型要求固定 4 轨输入）
./target/release/mixer-cli stemfx \
  --source bass.wav:bass --source drums.wav:drums \
  --reference ref_vocals.wav:vocals --reference ref_other.wav:other \
  -o out.wav --stems-out out_stems

# 只看模型给的效果链，不渲染（几秒出结果）
./target/release/mixer-cli stemfx --source-dir a --reference-dir b --dry-run --json
```

GUI：`mixer-app` → 「风格迁移 StemFX」标签页 → 选源分轨 / 参考分轨 / 输出 → 「🎛 迁移风格」或「仅看效果链」。

**模型与体积**：StemFX（ISMIR 2026；BSFiLM 编码器 + 6 层 Transformer 解码器，28.6M 参数）已转成 **W8A32**（int8 权重 + fp32 激活，`MatMulNBits` bits=8、block=32、对称量化），并**用 `include_bytes!` 内嵌进二进制**，体积增量约 36 MB（encoder 4.6 MB + generator 31.8 MB + mel 滤波器组 0.3 MB）。**开箱即用、不联网、无需下载权重**；想外置覆盖时，把 `*.onnx` 放进 `--models-dir` 指向的 `stemfx/` 子目录即可优先加载。

| 类别 | 覆盖情况 |
|---|---|
| 滤波 | `*_lowpass/highpass/bandpass/bandreject/peaking/shelf/allpass`（RBJ biquad、Butterworth 级联、sinc FIR） |
| 动态 | 压缩 / 限幅 / 门限 / 响度归一（软拐点 + BS.1770 K-weighting） |
| 空间与调制 | 混响（Schroeder）、延时、合唱、镶边、相位、颤音 |
| 跳过 | 变调 / 变速 / 时间拉伸（`am_pitch_shift`、`sox_tempo`、`lib_time_stretch` 等）：纯 DSP 无高质量实现，**显式记入结果的 `skipped`**，不假装处理 |

> 量化精度：编码器嵌入与 PyTorch 参考的余弦相似度 **0.994**；解码器在相同嵌入下的贪心 token 序列与 fp32 模型**完全一致**（见 `crates/mixer-core/tests/stemfx.rs`）。

重建模型（可选，需要 110 MB 权重与 CPU 版 Torch）：

```bash
python scripts/fetch_stemfx.py   # hf-mirror 下权重 + gh-proxy 拉上游代码 + 导出量化
```

## 输入模式与乐器指定

### 两种输入模式（`--mode`）

| 模式 | 输入 | 是否跑分离模型 | 适用 |
|---|---|---|---|
| `stems`（默认） | N 条已分离好的乐器分轨 | **否，完全跳过** | 手上已有分轨；最快、无分离损失 |
| `separate` | 1 条成品混音 | 是（权重可用时） | 只有成品，想先拆轨再智能重混 |

> **分离模式的现实说明**：BS-RoFormer / HT-Demucs 完整权重达数十~数百 MB，未随原型打包。当权重不可用时，`separate` 模式会**优雅降级**为单轨母带处理（glue/立体声/响度/限幅），并在结果的 `separation_applied=false`、`stages` 中明确标注，绝不假装分离。下载权重与接入方法见 `docs/ARCHITECTURE.md`。

### 输入到底要不要指定乐器？—— **不需要，只输入轨道即可**

这是本系统的核心设计取舍：**默认自动推断，可选显式覆盖**。

- **只输入轨道就行（推荐）**：引擎根据文件名/路径关键词自动识别乐器——
  - 含 `vocal / voice / vox / 人声 / 主唱` → **人声**（HPF + 存在感 + 压缩 + 侧链主控）
  - 含 `drum / kick / snare / 鼓` → **鼓**（瞬态保留 + 透明压缩）
  - 含 `bass / 贝斯 / 低音` → **贝斯**（单声道低频 + 紧压缩）
  - 含 `inst / accompaniment / backing / 伴奏 / 器乐` → **伴奏**（中频雕刻 + 闪避）
  - 识别不出 → **其他乐器**（温和 EQ + 轻压缩，安全兜底）
  - 推断结果会写回 `MixResult.stems_used[].instrument` 和 `inferred=true`，可追溯。
- **何时才需要显式指定**：文件名不含乐器关键词（如 `track1.wav`）、或自动推断不符合你的编排意图时，用 `--stems path:instrument` 或 JSON 的 `stems[].instrument` 显式指定；此时 `inferred=false`。

之所以这样做：乐器专属处理链对音质很关键（人声切低频、贝斯做单声道低频、鼓保瞬态），但让用户逐条标注会破坏"一键"体验。**自动推断让 95% 的场景零配置，显式覆盖留给专家用户**——两者通过同一 `Instrument` 枚举统一，三端（CLI/GUI/插件）行为一致。

## 混音链（算法顺序）

1. **解码 / 重采样 / 立体声归一 / 长度对齐**
2. **NN 音质增强**（可选，ONNX，逐帧动态增益网络）
3. **人声链**：75Hz 高通 → 250Hz 浑浊削减 → 3kHz 存在感 → 10kHz 空气感 → 压缩 → 用户增益
4. **伴奏链**：2.5kHz 中频雕刻（为人声让位）→ 轻压缩 → **侧链 ducking**（人声出现时伴奏自动衰减）
5. **总线**：相加 → glue 压缩 → M/S 立体声宽度
6. **响度标准化**：BS.1770 K-weighting + 能量域门限 integrated LUFS → 目标增益 → 限幅后复测迭代（结果含 `loudness_target_met`）
7. **真峰限幅**：4× 多相窗函数 sinc 插值检测样本间峰 + 前瞻限幅（瞬时压制/平滑释放 + 终检微缩），严格不越 dBTP 上限
8. **导出**：24-bit PCM WAV（可选 16/24/32F）

> 度量口径：真峰用 4× 多相 sinc 插值重建后取峰，不是样本峰；响度门限在能量域计算；
> 若素材动态过大（例如稀疏底鼓）导致"响度达标"与"真峰不越界"不可兼得，引擎会优先保证真峰，
> 并把 `loudness_target_met` 置为 `false`，不会假装达标。

## 目录结构
```
onemix/
├── Cargo.toml                       # workspace
├── crates/
│   ├── mixer-core/                  # 共享引擎（DSP + NN + API）
│   ├── mixer-cli/                   # 命令行
│   └── mixer-app/                   # Rust 原生 GUI（egui）
├── models/
│   ├── onemix_dyn_enhance_v1.onnx   # 随原型分发的轻量 NN 增强模型
│   ├── stemfx/                      # StemFX W8A32 ONNX（include_bytes! 内嵌，入库）
│   ├── native/libonnxruntime.so     # 动态加载的 ONNX Runtime 原生库（CI 提取，不入库）
│   └── registry                     # 见 mixer-core/src/nn/registry.rs
├── sxseditor-plugin/                # .sxsp 插件源码（apiVersion 2）
│   ├── manifest.json
│   ├── main.js                      # 命令 + 自动化 + 面板 RPC
│   ├── engine.js                    # 与 Rust 逐算法对齐的 JS 移植
│   └── panel/                       # 插件面板 UI
├── dist-plugins/onemix-mixer-0.1.0.sxsp  # 未签名主包（默认可装）
├── dist-plugins/onemix-mixer-0.1.0-signed.sxsp  # 自签名包（需先信任密钥）
├── tools/stemfx_export/             # StemFX → ONNX(W8A32) 导出脚本
├── scripts/                         # 模型下载/导出、测试音频生成、发布打包
├── docs/                            # 架构、模型选型、API、插件说明
└── examples/fixtures/               # 合成测试音频
```

## 许可证
MIT
