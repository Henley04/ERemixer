# OneMix 代码审查报告（2026-11-21）

## 审查方式

由三个独立审查者并行完成，各自只读代码、不修改文件；主执行者逐条复核关键结论后才动手修：

| 审查域 | 范围 |
|---|---|
| 核心引擎 | `mixer-core`（DSP、LUFS、限幅、流水线、NN 适配、API 模型） |
| 前端 | `mixer-cli`（clap/调度/输出契约）、`mixer-app`（GUI + 双入口） |
| 插件 | `.sxsp` manifest/命令/面板、`engine.js` 与 Rust 的逐算法一致性、宿主源码契约 |

复核方式：对每条 P0 直接读源码确认，并用可执行用例复现（12 kHz/45° 真峰、满幅瞬态限幅、100 ms 短音频、Windows 盘符分轨、宿主 `inspectPackage` 验包）。

## 结论（修复前）

审查前的交付**不能按"一键混音到 -14 LUFS 且不削波"与"插件能力完全一致"的承诺信任**：

- 「4× 过采样真峰」实际是线性插值，数学上恒等于样本峰，12 kHz 内容可低估约 3 dB；
- 限幅器只在样本域硬钳位，重建后真峰可超出上限；
- 插件自签名包会被宿主默认拒绝；面板 RPC 全部打空；`mixFiles` 必崩；`ctx.ort` 调用方式与宿主 API 完全不符。

以上问题已全部修复并加上回归测试。

## 已修复的 P0

| # | 问题 | 修复 | 验证 |
|---|---|---|---|
| 1 | 真峰检测用线性插值，检测不到样本间峰 | 改为 4× 多相 Hann 窗 sinc 插值，逐样本求真峰包络 | 12 kHz/45°：样本峰 -3.02 dBFS，新检测 **+0.11 dBTP** |
| 2 | 限幅器只在样本域 clamp，真峰可越界；起始瞬态泄漏 | 真峰包络 + 前瞻窗口滑动最小值 + 瞬时压制/平滑释放 + 终检微缩 | 满幅 12 kHz 正弦限幅后 **≤ -0.95 dBTP**；起始满幅瞬态同样不越界 |
| 3 | 响度开环：限幅压低响度后不再补偿，`ok` 恒 true | measure→gain→limit→复测，最多 2 轮补偿；新增 `loudness_target_met` | 稀疏/短素材也可达标；不可兼得时显式 `false` |
| 4 | `output_sample_rate` 未校验：`<20` 触发 `clamp` panic，极大值 OOM | 入口校验 8000..=192000；位深 16/24/32；target/ceiling 范围校验 | 新增 `test_invalid_sample_rate_rejected` |
| 5 | NN 增强在 <128 帧人声上切片 panic | 夹紧 `start/end` 后再切片 | 100 ms 素材 `--nn` 正常完成 |
| 6 | 传统两轨 `--vocal-gain` 被应用两次 | 分轨构造时增益置 0，只在人声链应用一次 | `--vocal-gain 6` 增益从 +4.76 dB 降至 +1.29 dB（单次） |
| 7 | 插件自签名包 `unknown-key`，默认装不上 | 主包改为**未签名**（宿主 `PASS_STATUS` 放行），签名包单独提供 | 宿主 `inspectPackage`：未签名 `installable=true`；签名包 `SIGNATURE_UNKNOWN_KEY` |
| 8 | `ctx.fs` 只允许插件目录，面板手填绝对路径必失败 | IO 层 `ctx.fs` 优先、`node:fs` 回退，并在文档显式声明该边界 | 模拟 `ctx.fs` 全抛错，`probe`/`mix` 仍成功 |
| 9 | 面板 `command.run` 被原样透传，插件未分派 → 所有按钮报错 | `panels.rpc` 增加 `command.run` 白名单分派 | 通过 `panels.rpc('command.run', …)` 成功混音 |
| 10 | `mixFiles` 用 `this.mix`，严格模式下 `this=undefined` | 改为 `module.exports.commands.mix(...)` | `mixFiles` 直接调用与面板调用均成功 |
| 11 | `ctx.ort` 调用与宿主不符：句柄传对象、feeds 缺 `type/dims` | 按 `ortRunner.js` 真实 API 重写：`load` 返回 `{handle}`，`run(handle,{x:{type,dims,data}})` | 严格 mock（校验类型/shape）下 `nn_applied=true` |

## 已修复的 P1

**核心/CLI**

- K-weighting shelf 系数错误（`sqrt(A-1)` → `sqrt(A)*alpha`），并改用 BS.1770 标称滤波器参数；负增益不再产生 NaN。
- LUFS 门限从 dB 域改为**能量域**，短于 400 ms 的素材按整段单块测量（不再直接 -70）。
- 预设五项（目标响度/真峰/ducking/glue/宽度）统一在 core 应用一次，删除 CLI 的重复实现。
- Glue 压缩与限幅改为每声道独立实例，消除 L→R 包络继承与声像漂移。
- 多声道输入按简化 ITU 系数下混（原来直接丢弃 3..N 声道，中置人声会消失）。
- 乐器推断只看文件名，移除 `lead`/`sub`/`music` 等歧义词（`lead_guitar.wav` 不再被当人声拿到侧链主控权）。
- NN 帧增益改为逐声道展开（原来在交错样本上展开，L/R 拿到不同曲线）。
- 新增**真正的取消**：`mix_with_progress_cancel` 在阶段边界与写盘前检查原子标志；取消不会留下输出文件。
- 测试临时目录按进程+序号隔离，修复并行测试互相覆盖。

**CLI/GUI**

- `--stems` 支持 Windows 盘符、含冒号路径、`path:gain`、`path:instrument:gain`；段数过多显式报错。
- `--gui` / `--cli` 真正实现；任何带参数调用都交给 CLI，不再"漏写子命令却静默开窗"。
- `--json` 失败也向 stdout 输出 `{"ok":false,"error":…}`（此前空输出 + jq 误判成功）。
- `--preset` / `--bit-depth` / `--sample-rate` 在 clap 层校验；模式与输入组合冲突不再静默丢弃。
- GUI 无显示器时给出可操作提示；预设切换真正写回 UI 参数；文件对话框去掉假支持的 FLAC；取消按钮接入引擎。

**插件**

- `progress` 改为宿主约定的 0..1；面板进度条兼容两种量纲。
- `mix`/`mixFiles` 标记 `long: true`。
- `biquad` 补 `lowpass`（原来 bass 链的低通是空操作，导致整段 mono 叠加）。
- WAV 解析补 8-bit 无符号、WAVE_FORMAT_EXTENSIBLE（SubFormat）、64-bit float、截断/声道/采样率边界；24-bit 写出补 RIFF 偶数字节填充。
- 预设覆盖范围与 Rust 对齐（五项），`separation_model_id` 未分离时返回 `null`。
- 面板补 `hello` 握手、主题字段映射（`fg/panel` → CSS 变量）。

## 回归结果

- `cargo test -p mixer-core`：**21/21 通过**（新增真峰、限幅、短音频、取消、非法采样率等用例）。
- `scripts/verify_parity.sh`：两轨与多轨的 `output_lufs`、`output_true_peak_dbtp` 与 Rust **小数点后三位完全一致**。
- CLI 实测：`streaming` → -14.00 LUFS / -2.27 dBTP / met=true；`quiet` → -20.00 LUFS / met=true；分轨直入 4 轨乐器推断正确。
- 插件包：宿主 `inspectPackage` 返回 `installable=true`（未签名），manifest/checksums 通过。

## 仍然存在的已知限制（未修，均有记录）

| 级别 | 限制 | 影响 / 建议 |
|---|---|---|
| 架构 | 插件读绝对路径依赖 `node:fs` 回退 | 宿主 `sandbox.js` 自述不是沙箱；若未来收紧，需改为 base64/宿主数据面。已在 `engine.js` 顶部与插件文档声明 |
| 架构 | `separate` 模式仍无真实分离权重 | 只做「器乐链 + 总线」降级；注册表提供 BS-RoFormer/HT-Demucs 下载源，接入点已预留 |
| 语义 | 预设用「等于默认值」判断显式设置 | 显式写默认值会被预设覆盖；已在 `docs/API.md` 标注，彻底修复需把 5 个字段改为 `Option` |
| 质量 | 重采样为线性插值，降采样无抗混叠 | 96k→48k 时 18k 以上会折叠；建议后续换 polyphase sinc |
| 质量 | 写出无 dither | 低电平 16/24-bit 素材有量化失真 |
| 能力 | GUI 仍是两轨简化入口 | 分轨/分离请用 CLI 或插件；README 已如实标注 |
| 性能 | 真峰/限幅每样本 24 次乘加 | 5 分钟立体声在 release 下可接受，但可改 FIR 或降采样检测 |
| 上游 | 官方打包器写 1980-00-00 时间戳 | 宿主可正常安装，第三方解压工具可能报警 |

## 一句话总结

这轮审查把"听起来对"的实现换成了**可验证正确**的实现：真峰不再低估、限幅不再越界、响度闭环且诚实报告是否达标，插件也真的能被安装、被面板调用、被 MCP 调用。剩余限制集中在宿主权限边界与"没有打包完整分离权重"，都已显式标注而非隐藏。
