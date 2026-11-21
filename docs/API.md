# API 规范（三端统一 schema）

## MixRequest

```json
{
  "mode": "stems",
  "stems": [
    { "path": "vocal.wav", "instrument": "auto", "gain_db": 0.0 },
    { "path": "drums.wav", "instrument": "drums", "gain_db": 0.0 }
  ],
  "source_path": "",
  "separation_model": null,
  "vocals_path": "人声WAV路径",
  "accompaniment_path": "伴奏WAV路径",
  "output_path": "输出WAV路径",
  "target_lufs": -14.0,
  "true_peak_ceiling": -1.0,
  "output_sample_rate": null,
  "output_bit_depth": 24,
  "vocal_gain_db": 0.0,
  "ducking_db": -2.5,
  "vocal_presence_db": 2.0,
  "vocal_highpass_hz": 75.0,
  "accompaniment_carve_db": -2.0,
  "bus_glue": 0.35,
  "stereo_width": 1.1,
  "nn_enhance": false,
  "nn_model": null,
  "nn_mix": 0.5,
  "models_dir": null,
  "preset": "streaming",
  "verbose": true
}
```

| 字段 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `mode` | `stems`\\|`separate` | `stems` | 输入模式：分轨直入 / 分离再混 |
| `stems` | StemInput[] | `[]` | 分轨列表（仅 `stems` 模式）；非空时优先于 vocals/accompaniment |
| `source_path` | string | — | `separate` 模式的成品混音输入 |
| `separation_model` | string\\|null | null | 分离模型 id，null 用默认 |
| `vocals_path` | string | — | 人声干声 WAV（传统两轨 / separate 回退） |
| `accompaniment_path` | string | — | 伴奏 WAV（传统两轨） |
| `output_path` | string | — | 输出 WAV |
| `target_lufs` | number | -14 | 目标综合响度 |
| `true_peak_ceiling` | number | -1 | 真峰上限 dBTP |
| `output_sample_rate` | number\\|null | null | 输出采样率，null 取输入较大值 |
| `output_bit_depth` | 16\\|24\\|32 | 24 | 32 为 IEEE float |
| `vocal_gain_db` | number | 0 | 人声相对增益 |
| `ducking_db` | number | -2.5 | 侧链闪避量 |
| `vocal_presence_db` | number | 2 | 3kHz 存在感 |
| `vocal_highpass_hz` | number | 75 | 人声高通 |
| `accompaniment_carve_db` | number | -2 | 伴奏中频削减 |
| `bus_glue` | number | 0.35 | 总线压缩 0..1 |
| `stereo_width` | number | 1.1 | M/S 宽度 0..2 |
| `nn_enhance` | bool | false | 是否启用 NN 增强 |
| `nn_model` | string\\|null | null | 模型 id，null 用默认 |
| `nn_mix` | number | 0.5 | NN 湿混 0..1 |
| `models_dir` | string\\|null | null | 模型/原生库根目录 |

预设（`preset`）：`streaming`(-14) / `broadcast`(-16) / `quiet`(-20) / `loud`(-12)。预设会一次性设置 **目标响度、真峰上限、ducking、glue、立体声宽度** 五项。

> 已知局限：当前 schema 用「字段是否等于默认值」判断用户是否显式设置，因此把所有字段都显式写成默认值时，会被预设覆盖。需要精确控制时请改传具体数值并选用 `streaming`，或使用 JSON 请求逐字段赋值。CLI 的 `--preset` 只接受四个合法值。

### StemInput / 乐器类型

```json
{ "path": "vocal.wav", "instrument": "auto", "gain_db": 0.0 }
```

| `instrument` | 专属处理链 |
|---|---|
| `auto`（默认） | 根据文件名自动推断，结果写入 MixResult |
| `vocals` | HPF + 存在感 + 去齿音 + 压缩 + 侧链主控 |
| `drums` | 瞬态保留 + 透明压缩 |
| `bass` | 单声道低频 + 紧压缩 |
| `instrumental` | 中频雕刻 + 闪避 |
| `other` | 温和 EQ + 轻压缩（推断失败的兜底） |

**输入是否需要指定乐器？不需要——只输入轨道即可。** `auto` 会从文件名推断乐器；仅当文件名无关键词或需覆盖时显式指定。`stems` 为空且用传统 `vocals_path`/`accompaniment_path` 时，等价于两条分轨（vocals + instrumental）。

## MixResult

```json
{
  "ok": true,
  "output_path": "...",
  "sample_rate": 48000,
  "channels": 2,
  "duration_sec": 8.0,
  "input_vocals_lufs": -12.3,
  "input_accompaniment_lufs": -16.1,
  "premaster_lufs": -19.1,
  "output_lufs": -14.0,
  "output_true_peak_dbtp": -1.94,
  "normalization_gain_db": 5.11,
  "nn_applied": false,
  "nn_model_id": null,
  "elapsed_ms": 2505,
  "preset": "streaming",
  "stages": ["校验参数与预设", "...", "完成"],
  "mode": "stems",
  "stems_used": [
    { "path": "vocal.wav", "instrument": "vocals", "inferred": true, "gain_db": 0.0 },
    { "path": "drums.wav", "instrument": "drums", "inferred": false, "gain_db": 0.0 }
  ],
  "separation_applied": false,
  "separation_model_id": null
}
```
`mode` 为实际模式；`stems_used[].inferred=true` 表示乐器由文件名自动推断，`false` 为用户显式指定；`separation_applied` 表示是否真正运行了分离模型（分轨直入恒为 false）。

`loudness_target_met`：最终响度是否落在目标 ±0.5 LU 内。素材动态过大时可能无法同时满足响度与真峰上限，此时为 `false`（引擎优先保证不削波）。

## 调用方式

### CLI
```bash
# 分轨直入（自动推断乐器）
mixer-cli mix --stems vocal.wav --stems drums.wav --stems bass.wav -o out.wav --json
# 显式指定乐器 --stems path:instrument[:gainDb]
mixer-cli mix --stems lead.wav:vocals --stems kit.wav:drums:1.5 -o out.wav
# 只给增益：--stems path:gainDb
mixer-cli mix --stems bass.wav:1.5 -o out.wav
# Windows 盘符 / 文件名含冒号：整个串在存在时直接当路径
mixer-cli mix --stems 'C:\music\vocal.wav' --stems 'C:\music\inst.wav' -o out.wav
# 传统两轨（兼容）
mixer-cli mix -v vocals.wav -a accom.wav -o out.wav --json
# 分离再混
mixer-cli mix --mode separate --source full.wav -o out.wav
mixer-cli mix-json request.json        # 或 '-' 从 stdin
mixer-cli models / modes / instruments / presets
mixer-cli probe vocals.wav              # 探测 WAV
mixer-cli config                        # 打印默认请求
# 主应用 mixer-app 同样支持上述全部子命令
mixer-app modes
```

### SXSEditor 插件（命令即自动化方法）
方法名 `onemix-mixer.<commandId>`：
- `mix` — 完整 MixRequest（含 mode / stems）
- `mixFiles` — `{vocalsPath, accompanimentPath, outputPath, preset?}`
- `probe` — `{path}` → WAV 信息
- `models` / `modes` / `instruments` / `presets` / `defaults`

CLI 的 `--json` 在成功与失败时都会向 stdout 输出 JSON（失败为 `{"ok":false,"error":"..."}`），退出码非 0，可直接交给 `jq`。

通过 MCP：
```bash
curl -s localhost:$PORT/automation -H "Authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' \
  -d '{"method":"onemix-mixer.mixFiles","params":{"vocalsPath":"...","accompanimentPath":"...","outputPath":"...","preset":"streaming"}}'
```
或通过 CLI：`SXSEditor --cli plugin call onemix-mixer.mixFiles --params '{...}' --json`

## 风格迁移（StemFX）

独立于 `MixRequest` 的一条能力线：输入源/参考两套分轨，输出"效果器链 + 渲染结果"。

### CLI

```bash
# 目录扫描（按文件名识别 vocals/bass/drums/other）
mixer-cli stemfx --source-dir my_stems --reference-dir ref_stems -o out.wav --lufs -23

# 逐轨显式指定（缺失轨按静音补齐，模型要求固定 4 轨）
mixer-cli stemfx --source a.wav:vocals --source b.wav:bass \
                 --reference r1.wav:vocals --reference r2.wav:other -o out.wav

# 只预测不渲染 + JSON；--stems-out 逐轨导出 24-bit WAV
mixer-cli stemfx --source-dir a --reference-dir b --dry-run --json
mixer-cli stemfx --source-dir a --reference-dir b -o out.wav --stems-out stems_out
```

| 参数 | 默认 | 说明 |
|---|---|---|
| `--source` / `--reference` | — | `path[:instrument]`，instrument ∈ `vocals/bass/drums/other` |
| `--source-dir` / `--reference-dir` | — | 扫描目录内 `*.wav`，按文件名推断乐器 |
| `-o, --output` | 必填 | 成品 WAV（24-bit / 44.1 kHz） |
| `--stems-out` | — | 逐轨导出目录 |
| `--dry-run` | false | 只解码效果链，不渲染、不写盘 |
| `--max-tokens` | 512 | 自回归步数上限 |
| `--lufs` | 无 | 渲染后逐轨响度归一的目标值（如 `-23`） |
| `--models-dir` | 无 | 外置 `stemfx/*.onnx` 目录；缺省用内嵌权重 |
| `--json` / `--quiet` | false | 机器可读输出 / 静默 |

### JSON 输出

```json
{
  "ok": true,
  "output_path": "out.wav",
  "dry_run": false,
  "sample_rate": 44100,
  "elapsed_ms": 2512,
  "tokens": 29,
  "chain": {"stems": [{"stem": "vocals", "chain": [{"effect": "am_lowpass_filter",
              "params": {"cutoff_freq": 1341.0, "rolloff": 23.8}}]}]},
  "applied": ["vocals:am_lowpass_filter", "bass:ta_bandreject_biquad"],
  "skipped": ["other:sox_tempo"],
  "stems": [{"stem": "vocals", "frames": 220500, "duration_sec": 5.0}]
}
```

`skipped` 列出无法在不变速/不变调前提下实现的效果器（`*pitch_shift`、`*time_stretch`、`sox_tempo`、`sox_speed`），属能力边界，不会静默丢弃。

### Rust API

```rust
use mixer_core::stemfx::{self, StemSet, StyleOptions};

let src = StemSet::from_specs(&[("vocals.wav".into(), "vocals".into())])?;
let r#ref = StemSet::from_specs(&[("ref.wav".into(), "other".into())])?;
let opts = StyleOptions { max_tokens: 512, target_lufs: None, models_dir: None, render: true };
let res = stemfx::transfer(&src, &r#ref, &opts, &|| false)?;
println!("{}", res.pretty);                       // 人可读效果链
mixer_core::write_wav("out.wav", &res.mixdown(), 24)?;
```

`StyleResult` 字段：`chain`（结构化效果链）、`pretty`（文本）、`tokens`（token 名序列）、`applied` / `skipped`、`stems`（逐轨 44.1 kHz 立体声）、`sample_rate`、`elapsed_ms`。

## ProgressEvent
```json
{ "percent": 45, "stage": "人声链(HPF/EQ/压缩/增益)", "detail": null }
```
百分比 0→100，阶段名三端一致。
