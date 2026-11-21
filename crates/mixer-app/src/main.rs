//! OneMix 原生桌面 GUI（Rust + egui/eframe）。
//! 两个标签页：一键混音（人声 + 伴奏 → 母带）、风格迁移（StemFX，参考曲风格 → 效果链）。
//! 与 CLI、SXSEditor 插件共享 mixer-core，能力完全一致。

use eframe::egui;
use mixer_core::api::{MixRequest, Preset, ProgressEvent};
use mixer_core::nn::registry as model_registry;
use mixer_core::stemfx::{self, StemSet, StyleOptions, StyleResult};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;

/// 加载中文字体（嵌入的裁剪子集），保证中文界面正常显示。
fn install_chinese_font(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "onemix_cjk".to_owned(),
        egui::FontData::from_static(include_bytes!("../assets/NotoSansCJK-subset.ttf")),
    );
    fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default()
        .insert(0, "onemix_cjk".to_owned());
    fonts
        .families
        .entry(egui::FontFamily::Monospace)
        .or_default()
        .push("onemix_cjk".to_owned());
    ctx.set_fonts(fonts);
}

#[derive(Debug, Clone)]
enum WorkerMsg {
    Progress(ProgressEvent),
    Done(Result<mixer_core::api::MixResult, String>),
    StyleDone(Box<Result<StyleResult, String>>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Mix,
    Style,
}

/// StemFX 四轨的显示名与传给核心库的 instrument 键。
const STEM_LABELS: [(&str, &str); 4] = [
    ("人声", "vocals"),
    ("贝斯", "bass"),
    ("鼓组", "drums"),
    ("其他", "other"),
];

/// StemFX 面板状态。
struct StylePanel {
    src: [String; 4],
    reference: [String; 4],
    output: String,
    stems_out: String,
    max_tokens: usize,
    lufs_on: bool,
    lufs: f32,
    result: Option<StyleResult>,
    dry_chain: Option<String>,
    error: Option<String>,
}

impl Default for StylePanel {
    fn default() -> Self {
        Self {
            src: Default::default(),
            reference: Default::default(),
            output: String::new(),
            stems_out: String::new(),
            max_tokens: 512,
            lufs_on: false,
            lufs: -23.0,
            result: None,
            dry_chain: None,
            error: None,
        }
    }
}

impl StylePanel {
    /// 把非空路径转成 `(路径, instrument)` 规格。
    fn specs(&self, which: bool) -> Vec<(String, String)> {
        let paths = if which { &self.reference } else { &self.src };
        paths
            .iter()
            .enumerate()
            .filter(|(_, p)| !p.trim().is_empty())
            .map(|(i, p)| (p.trim().to_string(), STEM_LABELS[i].1.to_string()))
            .collect()
    }
}

struct OneMixApp {
    tab: Tab,
    // 输入路径
    vocals_path: String,
    accom_path: String,
    output_path: String,
    // 参数
    preset: String,
    target_lufs: f32,
    true_peak: f32,
    vocal_gain_db: f32,
    ducking_db: f32,
    vocal_presence_db: f32,
    stereo_width: f32,
    nn_enhance: bool,
    nn_model: String,
    // 状态
    running: bool,
    progress: u32,
    stage: String,
    result: Option<mixer_core::api::MixResult>,
    error: Option<String>,
    rx: Option<Receiver<WorkerMsg>>,
    tx: Option<Sender<WorkerMsg>>,
    models_open: bool,
    cancel: Option<Arc<AtomicBool>>,
    style: StylePanel,
}

impl OneMixApp {
    fn new(ctx: &egui::Context) -> Self {
        install_chinese_font(ctx);
        Self {
            tab: Tab::Mix,
            vocals_path: String::new(),
            accom_path: String::new(),
            output_path: String::new(),
            preset: "streaming".into(),
            target_lufs: -14.0,
            true_peak: -1.0,
            vocal_gain_db: 0.0,
            ducking_db: -2.5,
            vocal_presence_db: 2.0,
            stereo_width: 1.1,
            nn_enhance: false,
            nn_model: mixer_core::nn::registry::DEFAULT_ENHANCE_MODEL.to_string(),
            running: false,
            progress: 0,
            stage: "就绪".into(),
            result: None,
            error: None,
            rx: None,
            tx: None,
            models_open: false,
            cancel: None,
            style: StylePanel::default(),
        }
    }

    fn build_request(&self) -> MixRequest {
        MixRequest {
            vocals_path: self.vocals_path.clone(),
            accompaniment_path: self.accom_path.clone(),
            output_path: self.output_path.clone(),
            target_lufs: self.target_lufs,
            true_peak_ceiling: self.true_peak,
            output_bit_depth: 24,
            vocal_gain_db: self.vocal_gain_db,
            ducking_db: self.ducking_db,
            vocal_presence_db: self.vocal_presence_db,
            stereo_width: self.stereo_width,
            nn_enhance: self.nn_enhance,
            nn_model: if self.nn_enhance { Some(self.nn_model.clone()) } else { None },
            models_dir: None,
            preset: self.preset.clone(),
            verbose: true,
            ..Default::default()
        }
    }

    fn start_mix(&mut self) {
        if self.vocals_path.is_empty() || self.accom_path.is_empty() || self.output_path.is_empty() {
            self.error = Some("请先选择人声、伴奏与输出路径".into());
            return;
        }
        if !std::path::Path::new(&self.vocals_path).exists() {
            self.error = Some(format!("人声文件不存在：{}", self.vocals_path));
            return;
        }
        if !std::path::Path::new(&self.accom_path).exists() {
            self.error = Some(format!("伴奏文件不存在：{}", self.accom_path));
            return;
        }

        let (tx, rx) = mpsc::channel();
        self.tx = Some(tx.clone());
        self.rx = Some(rx);
        self.running = true;
        self.progress = 0;
        self.stage = "准备中…".into();
        self.result = None;
        self.error = None;

        let req = self.build_request();
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = Some(cancel.clone());
        thread::spawn(move || {
            let res = mixer_core::mix_with_progress_cancel(
                &req,
                Some(|e: ProgressEvent| {
                    let _ = tx.send(WorkerMsg::Progress(e));
                }),
                Some(cancel.as_ref()),
            );
            let _ = tx.send(WorkerMsg::Done(res.map_err(|e| e.to_string())));
        });
    }

    /// 启动一次风格迁移。`dry_run = true` 时只预测效果链，不渲染、不写盘。
    fn start_style(&mut self, dry_run: bool) {
        let src = self.style.specs(false);
        let reference = self.style.specs(true);
        if src.is_empty() {
            self.style.error = Some("请至少选择一个源分轨".into());
            return;
        }
        if reference.is_empty() {
            self.style.error = Some("请至少选择一个参考分轨".into());
            return;
        }
        if !dry_run && self.style.output.trim().is_empty() {
            self.style.error = Some("请先选择输出路径".into());
            return;
        }
        for (p, _) in src.iter().chain(reference.iter()) {
            if !std::path::Path::new(p).exists() {
                self.style.error = Some(format!("文件不存在：{}", p));
                return;
            }
        }

        let opts = StyleOptions {
            max_tokens: self.style.max_tokens,
            target_lufs: if self.style.lufs_on { Some(self.style.lufs) } else { None },
            models_dir: None,
            render: !dry_run,
        };
        let output = self.style.output.trim().to_string();
        let stems_out = self.style.stems_out.trim().to_string();

        let (tx, rx) = mpsc::channel();
        self.tx = Some(tx.clone());
        self.rx = Some(rx);
        self.running = true;
        self.progress = 0;
        self.stage = if dry_run { "预测效果链…".into() } else { "风格迁移中…".into() };
        self.style.result = None;
        self.style.dry_chain = None;
        self.style.error = None;

        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = Some(cancel.clone());
        thread::spawn(move || {
            let job = || -> Result<StyleResult, String> {
                let src_set = StemSet::from_specs(&src).map_err(|e| e.to_string())?;
                let ref_set = StemSet::from_specs(&reference).map_err(|e| e.to_string())?;
                let res = stemfx::transfer(&src_set, &ref_set, &opts, &|| {
                    cancel.load(Ordering::Relaxed)
                })
                .map_err(|e| e.to_string())?;

                if !dry_run {
                    let mix = res.mixdown();
                    mixer_core::write_wav(&output, &mix, 24)
                        .map_err(|e| format!("写出成品失败: {}", e))?;
                    if !stems_out.is_empty() {
                        std::fs::create_dir_all(&stems_out)
                            .map_err(|e| format!("创建分轨目录失败: {}", e))?;
                        for (name, buf) in &res.stems {
                            let p = format!(
                                "{}/{}.wav",
                                stems_out.trim_end_matches(['/', '\\']),
                                name
                            );
                            mixer_core::write_wav(&p, buf, 24)
                                .map_err(|e| format!("写出 {} 失败: {}", p, e))?;
                        }
                    }
                }
                Ok(res)
            };
            let _ = tx.send(WorkerMsg::StyleDone(Box::new(job())));
        });
    }

    fn poll(&mut self) {
        let mut events = Vec::new();
        if let Some(rx) = &self.rx {
            while let Ok(msg) = rx.try_recv() {
                events.push(msg);
            }
        }
        for msg in events {
            match msg {
                WorkerMsg::Progress(e) => {
                    self.progress = e.percent;
                    self.stage = e.stage;
                }
                WorkerMsg::StyleDone(res) => {
                    self.running = false;
                    self.cancel = None;
                    match *res {
                        Ok(r) => {
                            self.stage = if r.stems.is_empty() {
                                "效果链已预测".into()
                            } else {
                                "完成".into()
                            };
                            self.progress = 100;
                            if r.stems.is_empty() {
                                self.style.dry_chain = Some(r.pretty.clone());
                            }
                            self.style.result = Some(r);
                        }
                        Err(e) if e.contains("取消") => {
                            self.stage = "已取消".into();
                            self.style.error = None;
                        }
                        Err(e) => {
                            self.stage = "失败".into();
                            self.style.error = Some(e);
                        }
                    }
                    self.rx = None;
                    self.tx = None;
                }
                WorkerMsg::Done(res) => {
                    self.running = false;
                    self.cancel = None;
                    match res {
                        Ok(r) => {
                            self.stage = "完成".into();
                            self.progress = 100;
                            self.result = Some(r);
                        }
                        Err(e) if e.contains("取消") => {
                            self.stage = "已取消".into();
                            self.error = None;
                        }
                        Err(e) => {
                            self.stage = "失败".into();
                            self.error = Some(e);
                        }
                    }
                    self.rx = None;
                    self.tx = None;
                }
            }
        }
    }

    /// 一键混音面板。
    fn ui_mix(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.label("① 选择音频");
        path_row(ui, "人声：", &mut self.vocals_path, false, "选择人声 WAV…");
        path_row(ui, "伴奏：", &mut self.accom_path, false, "选择伴奏 WAV…");
        path_row(ui, "输出：", &mut self.output_path, true, "选择输出 WAV…");
        ui.separator();

        ui.label("② 混音参数");
        let mut preset_changed = false;
        ui.horizontal(|ui| {
            ui.label("预设：");
            egui::ComboBox::from_label("")
                .selected_text(&self.preset)
                .show_ui(ui, |ui| {
                    for p in Preset::names() {
                        if ui
                            .selectable_value(&mut self.preset, (*p).to_string(), *p)
                            .clicked()
                        {
                            preset_changed = true;
                        }
                    }
                });
        });
        // 预设变更时把五项参数写回 UI，避免"界面显示 -14、实际按 -20 混音"。
        if preset_changed {
            if let Some(p) = Preset::from_name(&self.preset) {
                let mut base = MixRequest::default();
                p.apply(&mut base);
                self.target_lufs = base.target_lufs;
                self.true_peak = base.true_peak_ceiling;
                self.ducking_db = base.ducking_db;
                self.stereo_width = base.stereo_width;
            }
        }
        ui.add(egui::Slider::new(&mut self.target_lufs, -24.0..=-8.0).text("目标响度 LUFS"));
        ui.add(egui::Slider::new(&mut self.true_peak, -6.0..=0.0).text("真峰上限 dBTP"));
        ui.add(egui::Slider::new(&mut self.vocal_gain_db, -12.0..=12.0).text("人声增益 dB"));
        ui.add(egui::Slider::new(&mut self.ducking_db, -12.0..=0.0).text("伴奏闪避 dB"));
        ui.add(egui::Slider::new(&mut self.vocal_presence_db, -6.0..=8.0).text("人声存在感 dB"));
        ui.add(egui::Slider::new(&mut self.stereo_width, 0.0..=2.0).text("立体声宽度"));
        ui.checkbox(&mut self.nn_enhance, "NN 音质增强（ONNX）");
        if self.nn_enhance {
            ui.horizontal(|ui| {
                ui.label("模型：");
                egui::ComboBox::from_label("")
                    .selected_text(&self.nn_model)
                    .show_ui(ui, |ui| {
                        for id in model_registry::list_ids() {
                            ui.selectable_value(&mut self.nn_model, id.clone(), id);
                        }
                    });
                if ui.button("模型说明").clicked() {
                    self.models_open = !self.models_open;
                }
            });
            if self.models_open {
                egui::Window::new("NN 模型注册表")
                    .resizable(true)
                    .default_size((560.0, 360.0))
                    .show(ctx, |ui| {
                        for m in model_registry::registry() {
                            ui.heading(&m.id);
                            ui.label(format!("{} · {} · {}", m.name, m.task, m.architecture));
                            ui.label(&m.note);
                            if let Some(u) = &m.source_url {
                                ui.label(format!("下载：{}", u));
                            }
                            ui.separator();
                        }
                    });
            }
        }
        ui.separator();

        ui.horizontal(|ui| {
            let btn = egui::Button::new("▶ 一键混音").fill(egui::Color32::from_rgb(16, 185, 129));
            if ui.add_enabled(!self.running, btn).clicked() {
                self.start_mix();
            }
            cancel_button(self, ui);
        });

        ui_status(ui, self.progress, &self.stage);
        if let Some(err) = &self.error {
            ui.colored_label(egui::Color32::RED, format!("错误：{}", err));
        }

        if let Some(r) = &self.result {
            ui.separator();
            ui.heading("✓ 混音完成");
            egui::Grid::new("result_grid").num_columns(2).striped(true).show(ui, |ui| {
                ui.label("输出文件"); ui.label(&r.output_path);
                ui.label("输出响度"); ui.label(format!("{:.2} LUFS（目标 {:.1}）", r.output_lufs, r.normalization_gain_db + r.premaster_lufs));
                ui.label("真峰"); ui.label(format!("{:.2} dBTP", r.output_true_peak_dbtp));
                ui.label("时长 / 采样率"); ui.label(format!("{:.1}s / {} Hz", r.duration_sec, r.sample_rate));
                ui.label("响度增益"); ui.label(format!("{:+.2} dB", r.normalization_gain_db));
                ui.label("NN 增强"); ui.label(if r.nn_applied { format!("已应用 ({})", r.nn_model_id.as_deref().unwrap_or("?")) } else { "未应用".into() });
                ui.label("耗时"); ui.label(format!("{} ms", r.elapsed_ms));
                ui.label("预设"); ui.label(&r.preset);
            });
            if ui.button("打开输出目录").clicked() {
                if let Some(parent) = std::path::Path::new(&r.output_path).parent() {
                    let _ = open::that(parent);
                }
            }
        }
    }

    /// 风格迁移面板。
    fn ui_style(&mut self, ui: &mut egui::Ui) {
        ui.label("① 源分轨（要被「调」的干声，缺失轨按静音补齐）");
        for i in 0..4 {
            path_row(ui, &format!("{}：", STEM_LABELS[i].0), &mut self.style.src[i], false, "选择该轨 WAV…");
        }
        ui.separator();

        ui.label("② 参考分轨（想模仿的混音风格）");
        for i in 0..4 {
            path_row(ui, &format!("{}：", STEM_LABELS[i].0), &mut self.style.reference[i], false, "选择该轨 WAV…");
        }
        ui.separator();

        ui.label("③ 输出与参数");
        path_row(ui, "成品：", &mut self.style.output, true, "选择输出 WAV…");
        ui.horizontal(|ui| {
            ui.label("分轨导出：");
            if ui.button("选择目录…").clicked() {
                if let Some(p) = rfd::FileDialog::new().pick_folder() {
                    self.style.stems_out = p.to_string_lossy().to_string();
                }
            }
            ui.add(egui::TextEdit::singleline(&mut self.style.stems_out).hint_text("留空则不导出分轨"));
        });
        ui.add(
            egui::Slider::new(&mut self.style.max_tokens, 32..=512)
                .text("效果器 token 上限")
                .step_by(16.0),
        );
        ui.checkbox(&mut self.style.lufs_on, "渲染后逐轨响度归一");
        if self.style.lufs_on {
            ui.add(egui::Slider::new(&mut self.style.lufs, -30.0..=-10.0).text("目标 LUFS"));
        }
        ui.label(
            egui::RichText::new(
                "模型：StemFX BSFiLM 编码器 + 6 层 Transformer（W8A32 int8 量化，已内嵌，无需联网）。\
                 变调 / 变速类效果不做处理，未覆盖的效果器会在结果中标注跳过。",
            )
            .small()
            .weak(),
        );
        ui.separator();

        ui.horizontal(|ui| {
            let btn = egui::Button::new("▶ 迁移风格")
                .fill(egui::Color32::from_rgb(59, 130, 246));
            if ui.add_enabled(!self.running, btn).clicked() {
                self.start_style(false);
            }
            if ui
                .add_enabled(!self.running, egui::Button::new("仅看效果链"))
                .clicked()
            {
                self.start_style(true);
            }
            cancel_button(self, ui);
        });

        ui_status(ui, self.progress, &self.stage);
        if let Some(err) = &self.style.error {
            ui.colored_label(egui::Color32::RED, format!("错误：{}", err));
        }

        if let Some(chain) = &self.style.dry_chain {
            ui.separator();
            ui.heading("预测出的效果链（未渲染）");
            chain_block(ui, chain);
        } else if let Some(r) = &self.style.result {
            ui.separator();
            ui.heading("✓ 风格迁移完成");
            egui::Grid::new("style_grid").num_columns(2).striped(true).show(ui, |ui| {
                ui.label("输出文件");
                ui.label(if r.stems.is_empty() { "（未渲染）" } else { self.style.output.as_str() });
                ui.label("采样率"); ui.label(format!("{} Hz", r.sample_rate));
                ui.label("token 数"); ui.label(format!("{}", r.tokens.len()));
                ui.label("已应用效果"); ui.label(format!("{} 个", r.applied.len()));
                ui.label("跳过效果");
                ui.label(if r.skipped.is_empty() {
                    "无".to_string()
                } else {
                    r.skipped.join("、")
                });
                ui.label("耗时"); ui.label(format!("{} ms", r.elapsed_ms));
            });
            ui.add_space(4.0);
            ui.label("效果链：");
            chain_block(ui, &r.pretty);
            if !self.style.stems_out.trim().is_empty() {
                if ui.button("打开分轨目录").clicked() {
                    let _ = open::that(self.style.stems_out.trim());
                }
            }
        }
    }
}

fn chain_block(ui: &mut egui::Ui, text: &str) {
    egui::ScrollArea::vertical()
        .max_height(180.0)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            ui.add(
                egui::Label::new(egui::RichText::new(text).monospace().size(12.0)).wrap(),
            );
        });
    if ui.button("复制效果链").clicked() {
        ui.output_mut(|o| o.copied_text = text.to_string());
    }
}

/// 运行中的取消按钮（置标志位，引擎在阶段边界退出，不留半成品）。
fn cancel_button(app: &mut OneMixApp, ui: &mut egui::Ui) {
    if app.running && ui.button("取消").clicked() {
        if let Some(flag) = &app.cancel {
            flag.store(true, Ordering::Relaxed);
        }
        app.stage = "取消中…".into();
    }
}

fn ui_status(ui: &mut egui::Ui, progress: u32, stage: &str) {
    if progress > 0 {
        ui.add(
            egui::ProgressBar::new(progress as f32 / 100.0)
                .show_percentage()
                .text(stage),
        );
    } else if stage.contains('…') {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(stage);
        });
    }
}

fn path_row(ui: &mut egui::Ui, label: &str, value: &mut String, save: bool, hint: &str) {
    ui.horizontal(|ui| {
        ui.label(label);
        if save {
            if ui.button("保存为…").clicked() {
                if let Some(p) = rfd::FileDialog::new()
                    .add_filter("WAV", &["wav"])
                    .set_file_name("final_mix.wav")
                    .save_file()
                {
                    *value = p.to_string_lossy().to_string();
                }
            }
        } else if ui.button("浏览…").clicked() {
            if let Some(p) = rfd::FileDialog::new()
                .add_filter("WAV", &["wav"])
                .pick_file()
            {
                *value = p.to_string_lossy().to_string();
            }
        }
        ui.add(egui::TextEdit::singleline(value).hint_text(hint).desired_width(f32::INFINITY));
    });
}

impl eframe::App for OneMixApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll();
        if self.running {
            ctx.request_repaint();
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("◉ OneMix");
                ui.separator();
                ui.selectable_value(&mut self.tab, Tab::Mix, "一键混音");
                ui.selectable_value(&mut self.tab, Tab::Style, "风格迁移 StemFX");
            });
            ui.label(match self.tab {
                Tab::Mix => "人声 + 伴奏 → 成品母带  |  Rust 原生 · 与 SXSEditor 插件能力一致",
                Tab::Style => "参考曲风格 → 效果器链 → 渲染到自己的分轨  |  StemFX W8A32 内嵌模型",
            });
            ui.separator();

            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                match self.tab {
                    Tab::Mix => self.ui_mix(ui, ctx),
                    Tab::Style => self.ui_style(ui),
                }
                ui.separator();
                ui.label(format!("OneMix v{} · 开源 SOTA 模型 + 纯 Rust DSP · MIT", env!("CARGO_PKG_VERSION")));
            });
        });
    }
}

fn main() {
    // 主应用双入口：无参数或 --gui 启动原生 GUI；其余参数一律走 CLI
    //（与 mixer-cli 完全一致的逻辑，包括 --help 与参数错误）。
    let args: Vec<String> = std::env::args().collect();
    if mixer_cli::is_cli_invocation(&args) {
        let code = mixer_cli::run_with_args(args);
        std::process::exit(code);
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([820.0, 860.0])
            .with_min_inner_size([600.0, 620.0])
            .with_title("OneMix · 一键混音"),
        ..Default::default()
    };
    if let Err(e) = eframe::run_native(
        "OneMix",
        options,
        Box::new(|cc| Ok(Box::new(OneMixApp::new(&cc.egui_ctx)))),
    ) {
        // 无图形环境（服务器 / SSH）时给出可操作的提示，而不是裸错误。
        eprintln!("无法启动图形界面：{}", e);
        eprintln!("当前环境可能没有显示器。可改用命令行：");
        eprintln!("  mixer-app --help");
        eprintln!("  mixer-app mix --stems 人声.wav --stems 伴奏.wav -o 成品.wav");
        eprintln!("  mixer-app stemfx --source-dir 我的分轨 --reference-dir 参考分轨 -o 成品.wav");
        std::process::exit(1);
    }
}
