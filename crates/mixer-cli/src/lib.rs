//! OneMix CLI 逻辑库 —— 供 mixer-cli 二进制与 mixer-app（GUI 内置 CLI）共享。
//! 支持：一键混音（两轨兼容 / 多分轨 / 分离模式）、JSON 请求、模型/预设列举。

use clap::{Parser, Subcommand};
use mixer_core::api::{Instrument, MixMode, MixRequest, Preset, ProgressEvent, StemInput};
use mixer_core::nn::registry as model_registry;
use mixer_core::{mix_with_progress, read_wav};
use std::io::Read;

#[derive(Parser)]
#[command(name = "onemix", version, about = "OneMix 一键混音（分轨 / 成品 → 成品母带）")]
pub struct Cli {
    /// 强制以命令行模式运行（供 mixer-app 双入口使用；也可放在子命令之前）。
    #[arg(long, global = true, hide = true)]
    cli: bool,
    #[command(subcommand)]
    command: Command,
}

fn parse_bit_depth(s: &str) -> std::result::Result<u16, String> {
    match s.trim() {
        "16" => Ok(16),
        "24" => Ok(24),
        "32" => Ok(32),
        _ => Err("位深只支持 16 / 24 / 32".into()),
    }
}

fn parse_sample_rate(s: &str) -> std::result::Result<u32, String> {
    let v: u32 = s.trim().parse().map_err(|_| "采样率必须是整数".to_string())?;
    if (8_000..=192_000).contains(&v) {
        Ok(v)
    } else {
        Err("采样率必须在 8000..=192000 之间".into())
    }
}

#[derive(Subcommand)]
enum Command {
    /// 执行一键混音（默认分轨直入，跳过分离模型）。
    Mix {
        /// 人声干声 WAV 路径（传统两轨模式）。
        #[arg(short = 'v', long)]
        vocals: Option<String>,
        /// 伴奏 WAV 路径（传统两轨模式）。
        #[arg(short = 'a', long)]
        accompaniment: Option<String>,
        /// 输出 WAV 路径。
        #[arg(short = 'o', long)]
        output: String,
        /// 输入模式：stems(分轨直入，跳过分离) / separate(成品先分离再混)。
        #[arg(long, default_value = "stems", value_name = "MODE")]
        mode: String,
        /// 分轨输入，可重复；格式 path 或 path:instrument 或 path:instrument:gainDb。
        /// instrument ∈ vocals/drums/bass/instrumental/other/auto。
        /// 例：--stems vocal.wav:vocals --stems drum.wav:drums:1.5
        #[arg(long, value_name = "SPEC")]
        stems: Vec<String>,
        /// 分离模式的成品混音输入路径（缺省时用 --vocals）。
        #[arg(long)]
        source: Option<String>,
        /// 分离模型 id（separate 模式）。
        #[arg(long)]
        separation_model: Option<String>,
        /// 目标响度 LUFS。
        #[arg(long, default_value_t = -14.0)]
        target_lufs: f32,
        /// 真峰上限 dBTP。
        #[arg(long, default_value_t = -1.0)]
        true_peak: f32,
        /// 预设：streaming/broadcast/quiet/loud。
        #[arg(long, default_value = "streaming", value_parser = ["streaming", "broadcast", "quiet", "loud"])]
        preset: String,
        /// 人声增益 dB（对所有 vocals 轨生效；传统两轨模式同样生效）。
        #[arg(long, default_value_t = 0.0)]
        vocal_gain: f32,
        /// ducking 量 dB（负值）。
        #[arg(long, default_value_t = -2.5)]
        ducking: f32,
        /// 输出采样率（省略则自动；范围 8000..=192000）。
        #[arg(long, value_parser = parse_sample_rate)]
        sample_rate: Option<u32>,
        /// 输出位深 16/24/32。
        #[arg(long, default_value_t = 24, value_parser = parse_bit_depth)]
        bit_depth: u16,
        /// 启用 NN 音质增强（ONNX）。
        #[arg(long)]
        nn: bool,
        /// 指定 NN 模型 id。
        #[arg(long)]
        model: Option<String>,
        /// 模型资源目录。
        #[arg(long)]
        models_dir: Option<String>,
        /// 以 JSON 输出结果。
        #[arg(long)]
        json: bool,
        /// 静默模式（不输出进度）。
        #[arg(short = 'q', long)]
        quiet: bool,
    },
    /// 从 JSON 请求文件执行混音（供插件/批处理调用）。
    MixJson { path: String },
    /// 列出可用 NN 模型。
    Models,
    /// 列出可用输入模式。
    Modes,
    /// 列出可用乐器类型（用于 --stems / stems[].instrument）。
    Instruments,
    /// 列出可用预设。
    Presets,
    /// 打印默认 MixRequest JSON。
    Config,
    /// 探测 WAV 信息。
    Probe { path: String },
    /// StemFX 混音风格迁移：按参考曲目的混音风格，为自己的分轨生成并应用效果器链。
    Stemfx {
        /// 源分轨，可重复：path:instrument（instrument ∈ vocals/bass/drums/other）。
        #[arg(long, value_name = "SPEC")]
        source: Vec<String>,
        /// 源分轨目录（按文件名自动识别 vocals/bass/drums/other 四个 wav）。
        #[arg(long)]
        source_dir: Option<String>,
        /// 参考分轨，可重复：path:instrument。
        #[arg(long, value_name = "SPEC")]
        reference: Vec<String>,
        /// 参考分轨目录。
        #[arg(long)]
        reference_dir: Option<String>,
        /// 输出成品 WAV（44.1 kHz）。`--dry-run` 时可省略。
        #[arg(short = 'o', long, required_unless_present = "dry_run")]
        output: Option<String>,
        /// 同时输出四条处理后的分轨到该目录。
        #[arg(long)]
        stems_out: Option<String>,
        /// 只预测效果器链，不渲染音频。
        #[arg(long)]
        dry_run: bool,
        /// 解码步数上限。
        #[arg(long, default_value_t = 512)]
        max_tokens: usize,
        /// 渲染后按该响度归一（LUFS）；省略则保持电平。
        #[arg(long, allow_hyphen_values = true)]
        lufs: Option<f32>,
        /// 模型资源目录（含 native/ 与 stemfx/）。
        #[arg(long)]
        models_dir: Option<String>,
        /// 以 JSON 输出结果。
        #[arg(long)]
        json: bool,
        /// 静默模式。
        #[arg(short = 'q', long)]
        quiet: bool,
    },
}

fn print_progress(e: ProgressEvent) {
    let bar = "#".repeat((e.percent / 4) as usize);
    let detail = e.detail.map(|d| format!("  ({})", d)).unwrap_or_default();
    eprintln!("[{:3}%] {:<28} {}{}", e.percent, e.stage, bar, detail);
}

/// 解析单条 `--stems` 规格：path[:instrument[:gainDb]]。
///
/// 兼容：
/// - Windows 盘符（`C:\a.wav` / `C:/a.wav`）与文件名本身含冒号（整个串是存在的文件时直接当路径）
/// - `path:1.5`（只有增益）
/// - `path:vocals` / `path:vocals:1.5`
fn parse_stem_spec(spec: &str) -> std::result::Result<StemInput, String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err("分轨路径为空".into());
    }
    // 整个字符串就是一个实际存在的文件：直接当路径（Unix 文件名可以含冒号）。
    if std::path::Path::new(spec).exists() {
        return Ok(StemInput {
            path: spec.to_string(),
            instrument: Instrument::Auto,
            gain_db: 0.0,
        });
    }

    let parts: Vec<&str> = spec.split(':').collect();
    let mut next = 1usize;
    let mut path = parts[0].to_string();
    // Windows 盘符：parts[0] 是单字母且原始串第二个字符是冒号。
    if parts.len() >= 2
        && parts[0].len() == 1
        && parts[0].chars().next().map(|c| c.is_ascii_alphabetic()).unwrap_or(false)
        && spec.as_bytes().get(1) == Some(&b':')
    {
        path = format!("{}:{}", parts[0], parts[1]);
        next = 2;
    }

    let rest = &parts[next..];
    let (mut instrument, mut gain_db) = (Instrument::Auto, 0.0f32);
    match rest.len() {
        0 => {}
        1 => {
            // 优先按增益解析（`path:1.5`），否则按乐器名。
            if let Ok(g) = rest[0].parse::<f32>() {
                gain_db = g;
            } else if let Some(i) = Instrument::from_name(rest[0]) {
                instrument = i;
            } else {
                return Err(format!("未知乐器类型或增益: {}", rest[0]));
            }
        }
        2 => {
            instrument = Instrument::from_name(rest[0])
                .ok_or_else(|| format!("未知乐器类型: {}", rest[0]))?;
            gain_db = rest[1]
                .parse::<f32>()
                .map_err(|_| format!("无效增益: {}", rest[1]))?;
        }
        _ => return Err("分轨规格段数过多（最多 path:instrument:gainDb）".into()),
    }
    Ok(StemInput { path, instrument, gain_db })
}

/// 判断给定参数是否应进入 CLI 模式（否则 GUI）。
/// 无参数或 `--gui` → GUI；其余一律交给 CLI（包括 `--help` 与错误参数），
/// 避免"漏写 mix 子命令却静默开窗"。
pub fn is_cli_invocation(args: &[String]) -> bool {
    if args.len() <= 1 {
        return false;
    }
    if args[1] == "--gui" || args[1] == "-g" {
        return false;
    }
    true
}

/// 运行 CLI（参数含程序名）。返回进程退出码。
pub fn run_with_args(args: Vec<String>) -> i32 {
    // 允许 `--cli` 出现在子命令前（mixer-app 的双入口约定）。
    let args: Vec<String> = args
        .into_iter()
        .enumerate()
        .filter(|(i, a)| !(*i > 0 && a == "--cli"))
        .map(|(_, a)| a)
        .collect();
    let cli = match Cli::try_parse_from(args) {
        Ok(c) => c,
        Err(e) => {
            // clap 的 --help/--version 以 Err(DisplayHelp/DisplayVersion) 返回，按成功处理。
            if e.use_stderr() {
                eprintln!("{}", e);
                return 2;
            } else {
                println!("{}", e);
                return 0;
            }
        }
    };
    let _ = cli.cli;
    dispatch(cli)
}

fn dispatch(cli: Cli) -> i32 {
    match cli.command {
        Command::Mix {
            vocals, accompaniment, output, mode, stems, source, separation_model,
            target_lufs, true_peak, preset, vocal_gain, ducking, sample_rate, bit_depth,
            nn, model, models_dir, json, quiet,
        } => {
            let mix_mode = match MixMode::from_name(&mode) {
                Some(m) => m,
                None => {
                    eprintln!("✗ 未知模式: {}（可选: stems / separate）", mode);
                    return 2;
                }
            };
            // 解析多轨规格。
            let mut parsed_stems = Vec::new();
            for spec in &stems {
                match parse_stem_spec(spec) {
                    Ok(s) => parsed_stems.push(s),
                    Err(e) => {
                        eprintln!("✗ 分轨规格错误 '{}': {}", spec, e);
                        return 2;
                    }
                }
            }

            // 模式与输入的组合校验：不允许静默丢弃任何一条输入。
            match mix_mode {
                MixMode::Stems => {
                    if !parsed_stems.is_empty() && (vocals.is_some() || accompaniment.is_some()) {
                        eprintln!("✗ --stems 与 -v/--accompaniment 不能同时使用；请只选一种输入方式");
                        return 2;
                    }
                    if source.is_some() || separation_model.is_some() {
                        eprintln!("✗ --source / --separation-model 仅用于 --mode separate");
                        return 2;
                    }
                    if parsed_stems.is_empty() && vocals.is_none() && accompaniment.is_none() {
                        eprintln!("✗ 没有输入：请用 --stems 指定分轨，或用 -v/-a 指定人声与伴奏");
                        return 2;
                    }
                }
                MixMode::Separate => {
                    if !parsed_stems.is_empty() || accompaniment.is_some() {
                        eprintln!("✗ --mode separate 只接受一条成品混音（--source）；不能同时给 --stems/-a");
                        return 2;
                    }
                    if source.is_none() && vocals.is_none() {
                        eprintln!("✗ --mode separate 需要 --source 指定成品混音");
                        return 2;
                    }
                }
            }

            let req = MixRequest {
                mode: mix_mode,
                stems: parsed_stems,
                source_path: source.unwrap_or_default(),
                separation_model,
                vocals_path: vocals.unwrap_or_default(),
                accompaniment_path: accompaniment.unwrap_or_default(),
                output_path: output,
                target_lufs,
                true_peak_ceiling: true_peak,
                output_sample_rate: sample_rate,
                output_bit_depth: bit_depth,
                vocal_gain_db: vocal_gain,
                ducking_db: ducking,
                nn_enhance: nn,
                nn_model: model,
                models_dir,
                preset,
                ..Default::default()
            };
            // 预设统一由 mixer-core 应用（此处不再重复，避免三端行为漂移）。

            let result = if quiet {
                mixer_core::mix(&req)
            } else {
                mix_with_progress(&req, Some(print_progress))
            };

            match result {
                Ok(r) => {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&r).unwrap());
                    } else {
                        println!("\n✓ 混音完成: {}", r.output_path);
                        println!("  模式     : {}", r.mode.as_str());
                        println!("  分轨     : {}", r.stems_used.iter()
                            .map(|s| format!("{}[{}]", s.path.rsplit('/').next().unwrap_or(&s.path), s.instrument.as_str()))
                            .collect::<Vec<_>>().join(" + "));
                        println!("  输出响度 : {:.1} LUFS", r.output_lufs);
                        println!("  真峰     : {:.2} dBTP", r.output_true_peak_dbtp);
                        println!("  时长     : {:.1}s @ {} Hz", r.duration_sec, r.sample_rate);
                        println!("  分离模型 : {}", if r.separation_applied { "已应用" } else { "未应用(分轨直入或降级)" });
                        println!("  NN 增强  : {}", if r.nn_applied { "已应用" } else { "未应用" });
                        println!("  耗时     : {} ms", r.elapsed_ms);
                    }
                    0
                }
                Err(e) => {
                    // --json 契约：失败也必须在 stdout 给出可解析的 JSON，
                    // 否则调用方（jq/脚本）会把空输入误判为成功。
                    if json {
                        println!("{}", serde_json::json!({"ok": false, "error": e.to_string()}));
                    }
                    eprintln!("✗ 混音失败: {}", e);
                    1
                }
            }
        }
        Command::MixJson { path } => {
            let text = if path == "-" {
                let mut s = String::new();
                let mut stdin = std::io::stdin().lock();
                if let Err(e) = stdin.read_to_string(&mut s) {
                    eprintln!("✗ 读取 stdin 失败: {}", e);
                    return 1;
                }
                s
            } else {
                match std::fs::read_to_string(&path) {
                    Ok(t) => t,
                    Err(e) => {
                        eprintln!("✗ 无法读取请求文件: {}", e);
                        return 1;
                    }
                }
            };
            let req: MixRequest = match serde_json::from_str(&text) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("✗ 请求 JSON 解析失败: {}", e);
                    return 1;
                }
            };
            match mix_with_progress(&req, Some(print_progress)) {
                Ok(r) => {
                    println!("{}", serde_json::to_string(&r).unwrap());
                    0
                }
                Err(e) => {
                    println!("{}", serde_json::json!({"ok": false, "error": e.to_string()}));
                    1
                }
            }
        }
        Command::Models => {
            for m in model_registry::registry() {
                println!("{:<20} {:<12} bundled={:<5} {}", m.id, m.task, m.bundled, m.name);
                println!("    {}", m.note);
                if let Some(u) = m.source_url {
                    println!("    下载: {}", u);
                }
            }
            0
        }
        Command::Modes => {
            for m in MixMode::names() {
                let desc = match MixMode::from_name(m).unwrap() {
                    MixMode::Stems => "分轨直入：输入已分离的乐器分轨，跳过所有分离模型（默认）",
                    MixMode::Separate => "分离再混：输入成品混音，先用 SOTA 分离模型拆分再混",
                };
                println!("{:<12} {}", m, desc);
            }
            0
        }
        Command::Instruments => {
            for (name, desc) in [
                ("auto", "自动推断（根据文件名关键词，默认）"),
                ("vocals", "人声：HPF + 存在感 + 压缩 + 侧链主控"),
                ("drums", "鼓组：瞬态保留 + 透明压缩"),
                ("bass", "贝斯：单声道低频 + 紧压缩"),
                ("instrumental", "伴奏/器乐：中频雕刻 + 闪避"),
                ("other", "其他乐器：温和 EQ + 轻压缩"),
            ] {
                println!("{:<14} {}", name, desc);
            }
            0
        }
        Command::Presets => {
            for p in Preset::names() {
                println!("{}", p);
            }
            0
        }
        Command::Config => {
            println!("{}", serde_json::to_string_pretty(&MixRequest::default()).unwrap());
            0
        }
        Command::Stemfx {
            source, source_dir, reference, reference_dir, output, stems_out,
            dry_run, max_tokens, lufs, models_dir, json, quiet,
        } => cmd_stemfx(source, source_dir, reference, reference_dir, output, stems_out,
                        dry_run, max_tokens, lufs, models_dir, json, quiet),
        Command::Probe { path } => {
            match read_wav(&path) {
                Ok(buf) => {
                    let json = serde_json::json!({
                        "path": path,
                        "sample_rate": buf.sample_rate,
                        "channels": buf.channels,
                        "frames": buf.frames(),
                        "duration_sec": buf.duration_sec(),
                    });
                    println!("{}", serde_json::to_string_pretty(&json).unwrap());
                    0
                }
                Err(e) => {
                    eprintln!("✗ 探测失败: {}", e);
                    1
                }
            }
        }
    }
}

/// 使用进程实际参数运行 CLI（含程序名）。
pub fn run() -> i32 {
    let args: Vec<String> = std::env::args().collect();
    run_with_args(args)
}

/// 收集分轨：显式 `path:instrument` 规格 + 目录自动识别。
fn collect_stems(
    specs: &[String],
    dir: Option<&str>,
    label: &str,
) -> std::result::Result<mixer_core::stemfx::StemSet, String> {
    let mut pairs: Vec<(String, String)> = Vec::new();
    for spec in specs {
        let si = parse_stem_spec(spec)?;
        let inst = if si.instrument == Instrument::Auto {
            Instrument::infer(&si.path)
        } else {
            si.instrument
        };
        pairs.push((si.path, inst.as_str().to_string()));
    }
    if let Some(d) = dir {
        let rd = std::fs::read_dir(d).map_err(|e| format!("无法读取目录 {}: {}", d, e))?;
        let mut found = 0;
        for entry in rd.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()).map(|e| e.eq_ignore_ascii_case("wav")) != Some(true) {
                continue;
            }
            let p = path.to_string_lossy().to_string();
            let inst = Instrument::infer(&p);
            pairs.push((p, inst.as_str().to_string()));
            found += 1;
        }
        if found == 0 {
            return Err(format!("目录 {} 中没有 wav 文件", d));
        }
    }
    if pairs.is_empty() {
        return Err(format!("没有 {} 分轨输入", label));
    }
    mixer_core::stemfx::StemSet::from_specs(&pairs).map_err(|e| e.to_string())
}

#[allow(clippy::too_many_arguments)]
fn cmd_stemfx(
    source: Vec<String>,
    source_dir: Option<String>,
    reference: Vec<String>,
    reference_dir: Option<String>,
    output: Option<String>,
    stems_out: Option<String>,
    dry_run: bool,
    max_tokens: usize,
    lufs: Option<f32>,
    models_dir: Option<String>,
    json: bool,
    quiet: bool,
) -> i32 {
    let output = match (dry_run, output) {
        (false, Some(p)) if !p.trim().is_empty() => p,
        (false, _) => {
            eprintln!("✗ 需要 -o/--output 指定输出文件（或加 --dry-run 只预览效果链）");
            return 2;
        }
        (true, p) => p.unwrap_or_default(),
    };
    let src = match collect_stems(&source, source_dir.as_deref(), "源") {
        Ok(s) => s,
        Err(e) => {
            eprintln!("✗ {}", e);
            return 2;
        }
    };
    let refs = match collect_stems(&reference, reference_dir.as_deref(), "参考") {
        Ok(s) => s,
        Err(e) => {
            eprintln!("✗ {}", e);
            return 2;
        }
    };

    let opts = mixer_core::stemfx::StyleOptions {
        max_tokens,
        target_lufs: lufs,
        models_dir: models_dir.clone(),
        render: !dry_run,
    };
    if !quiet && !json {
        eprintln!("[stemfx] 编码风格嵌入 + 自回归预测效果链 …");
    }
    let res = match mixer_core::stemfx::transfer(&src, &refs, &opts, &|| false) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("✗ StemFX 迁移失败: {}", e);
            return 1;
        }
    };

    if !dry_run {
        let mix = res.mixdown();
        if let Err(e) = mixer_core::write_wav(&output, &mix, 24) {
            eprintln!("✗ 写出失败: {}", e);
            return 1;
        }
        if let Some(dir) = stems_out.as_deref() {
            if let Err(e) = std::fs::create_dir_all(dir) {
                eprintln!("✗ 创建目录失败: {}", e);
                return 1;
            }
            for (name, buf) in &res.stems {
                let p = format!("{}/{}.wav", dir.trim_end_matches(['/', '\\']), name);
                if let Err(e) = mixer_core::write_wav(&p, buf, 24) {
                    eprintln!("✗ 写出 {} 失败: {}", p, e);
                    return 1;
                }
            }
        }
    }

    if json {
        let out = serde_json::json!({
            "ok": true,
            "output_path": if dry_run { "" } else { output.as_str() },
            "dry_run": dry_run,
            "sample_rate": res.sample_rate,
            "elapsed_ms": res.elapsed_ms,
            "tokens": res.tokens.len(),
            "chain": res.chain,
            "applied": res.applied,
            "skipped": res.skipped,
            "stems": res.stems.iter().map(|(n, b)| serde_json::json!({
                "stem": n, "frames": b.frames(), "duration_sec": b.duration_sec(),
            })).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&out).unwrap());
    } else {
        println!("✓ StemFX 风格迁移完成（{} ms, {} 个 token）", res.elapsed_ms, res.tokens.len());
        println!("预测效果链：");
        print!("{}", res.pretty);
        if !res.skipped.is_empty() {
            println!("⚠ 已跳过的效果器（本实现不覆盖，多为变调/变速类）: {}", res.skipped.join(", "));
        }
        if !dry_run {
            println!("成品: {}  (44.1 kHz, 24-bit)", output);
        }
    }
    0
}
