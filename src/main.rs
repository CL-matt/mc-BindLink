#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod engine;
mod launcher;
mod map_sync;
mod pack_sync;
#[cfg(test)]
mod test_support;
mod types;

use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::SystemTime;

use anyhow::{Context, Result, anyhow};
use eframe::egui::{self, Color32, RichText};

use crate::types::{
    CacheTransfer, ConflictChoice, DetectedInstance, LogEntry, LogLevel, LogSink, MigrationRequest,
    MigrationResult, PackConflict, PackTransfer, PackVersion, ServerCache, SharedAsset, SyncMode,
};

const CJK_FONT_NAME: &str = "msyh";
const ACCENT: Color32 = Color32::from_rgb(76, 145, 98);
const GITHUB_URL: &str = "https://github.com/CL-matt/mc-bindsync";
const LORIAN_TOOL_URL: &str = "https://tools.voxisle.art/";
/// LorianTool 图标需要授权，仓库不包含该文件；`build.rs` 检测到本地存在时才编译进程序，
/// 否则链接按钮只显示文字。
#[cfg(has_lorian_tool_logo)]
const LORIAN_TOOL_LOGO: Option<&[u8]> = Some(include_bytes!("../logos/logo-square-LorianTool.png"));
#[cfg(not(has_lorian_tool_logo))]
const LORIAN_TOOL_LOGO: Option<&[u8]> = None;
const APP_LOGO: &[u8] = include_bytes!("../logos/bindlink-256.png");
const APP_NAME: &str = "BindLink 键链";
/// 取自 Cargo.toml，版本号只需在那里维护。
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
const HEADER_ICON_SIZE: f32 = 18.0;
const HEADER_LOGO_SIZE: f32 = 28.0;

fn main() -> eframe::Result {
    // run_native 的应用名还用作 eframe 的设置存储标识，版本号只放在窗口标题中，避免升级后丢失窗口布局
    let mut viewport = egui::ViewportBuilder::default()
        .with_title(format!("{APP_NAME} v{APP_VERSION}"))
        .with_inner_size([1080.0, 720.0])
        .with_min_inner_size([820.0, 560.0]);
    // 窗口创建前还没有日志区，图标解码失败的原因留到界面启动后写入日志
    let icon_error = match decode_png(APP_LOGO) {
        Ok(image) => {
            viewport = viewport.with_icon(Arc::new(egui::IconData {
                width: image.width(),
                height: image.height(),
                rgba: image.into_raw(),
            }));
            None
        }
        Err(err) => Some(format!("加载窗口图标失败：{err:#}")),
    };
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        APP_NAME,
        options,
        Box::new(|cc| Ok(Box::new(MigratorApp::new(&cc.egui_ctx, icon_error)))),
    )
}

enum WorkerMessage {
    Log(LogEntry),
    /// 后台线程遇到材质包或蓝图同名冲突，阻塞等待界面通过附带的发送端回传用户选择。
    Conflicts(Vec<PackConflict>, Sender<Vec<ConflictChoice>>),
    Finished(Result<MigrationResult>),
}

struct ConflictDialog {
    conflicts: Vec<PackConflict>,
    choices: Vec<ConflictChoice>,
    reply: Sender<Vec<ConflictChoice>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PackChoice {
    Skip,
    CopyMissing,
    LinkToShared,
}

impl PackChoice {
    const ALL: [PackChoice; 3] = [
        PackChoice::Skip,
        PackChoice::CopyMissing,
        PackChoice::LinkToShared,
    ];

    fn label(self, kind: SharedAsset) -> &'static str {
        match (self, kind) {
            (PackChoice::Skip, _) => "不处理",
            (PackChoice::CopyMissing, SharedAsset::ResourcePacks) => "复制缺少的材质包",
            (PackChoice::CopyMissing, SharedAsset::AxiomBlueprints) => {
                "复制缺少的蓝图（跳过高于目标版本的蓝图）"
            }
            (PackChoice::LinkToShared, SharedAsset::ResourcePacks) => {
                "Junction 指向共享目录（同步启用列表）"
            }
            (PackChoice::LinkToShared, SharedAsset::AxiomBlueprints) => {
                "Junction 指向共享目录（仅限两个实例版本相同）"
            }
        }
    }

    fn to_transfer(self, shared_dir: &Option<PathBuf>) -> Result<Option<PackTransfer>, String> {
        Ok(match self {
            PackChoice::Skip => None,
            PackChoice::CopyMissing => Some(PackTransfer::CopyMissing),
            PackChoice::LinkToShared => Some(PackTransfer::LinkToShared(
                shared_dir.clone().ok_or("请先选择共享目录")?,
            )),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CacheChoice {
    Skip,
    Copy,
    LinkToSource,
    LinkToShared,
}

impl CacheChoice {
    const ALL: [CacheChoice; 4] = [
        CacheChoice::Skip,
        CacheChoice::Copy,
        CacheChoice::LinkToSource,
        CacheChoice::LinkToShared,
    ];

    fn label(self) -> &'static str {
        match self {
            CacheChoice::Skip => "不处理",
            CacheChoice::Copy => "物理复制（迁移后将删除源缓存时选用）",
            CacheChoice::LinkToSource => "Junction 指向源实例（源实例继续使用时选用）",
            CacheChoice::LinkToShared => "Junction 指向共享目录（同一服务器反复更新整合包时选用）",
        }
    }

    fn to_transfer(self, shared_dir: &Option<PathBuf>) -> Result<Option<CacheTransfer>, String> {
        Ok(match self {
            CacheChoice::Skip => None,
            CacheChoice::Copy => Some(CacheTransfer::Copy),
            CacheChoice::LinkToSource => Some(CacheTransfer::LinkToSource),
            CacheChoice::LinkToShared => Some(CacheTransfer::LinkToShared(
                shared_dir.clone().ok_or("请先选择共享目录")?,
            )),
        })
    }
}

/// 一键填充同步内容的固定配置，只修改界面选项，用户仍可在应用后逐项调整。
struct Preset {
    name: &'static str,
    mode: SyncMode,
    cache_choice: CacheChoice,
}

const PRESETS: &[Preset] = &[
    Preset {
        name: "洛林",
        mode: SyncMode::Merge,
        cache_choice: CacheChoice::Copy,
    },
    Preset {
        name: "neko喵喵喵",
        mode: SyncMode::FullOverwrite,
        cache_choice: CacheChoice::LinkToShared,
    },
];

/// 底部状态栏显示的最近一次迁移结果。
enum RunStatus {
    Idle,
    Running,
    Finished { warnings: usize },
    Failed,
}

struct MigratorApp {
    selected_dirs: Vec<PathBuf>,
    instances: Vec<DetectedInstance>,
    template_index: Option<usize>,
    target_index: Option<usize>,
    mode: SyncMode,
    sync_options: bool,
    sync_mod_configs: bool,
    sync_maps: bool,
    voxy_choice: CacheChoice,
    bobby_choice: CacheChoice,
    pack_choice: PackChoice,
    blueprint_choice: PackChoice,
    shared_dir: Option<PathBuf>,
    logs: Vec<LogEntry>,
    status: RunStatus,
    worker: Option<Receiver<WorkerMessage>>,
    conflict_dialog: Option<ConflictDialog>,
    /// 未编译进图标或解码失败时为 `None`，标题栏改为只显示文字链接。
    lorian_logo: Option<egui::TextureHandle>,
    /// 解码失败时为 `None`，标题栏只显示软件名称。
    app_logo: Option<egui::TextureHandle>,
}

impl MigratorApp {
    fn new(ctx: &egui::Context, icon_error: Option<String>) -> Self {
        let mut app = Self {
            selected_dirs: Vec::new(),
            instances: Vec::new(),
            template_index: None,
            target_index: None,
            mode: SyncMode::Merge,
            sync_options: true,
            sync_mod_configs: true,
            sync_maps: false,
            voxy_choice: CacheChoice::Skip,
            bobby_choice: CacheChoice::Skip,
            pack_choice: PackChoice::Skip,
            blueprint_choice: PackChoice::Skip,
            shared_dir: None,
            logs: Vec::new(),
            status: RunStatus::Idle,
            worker: None,
            conflict_dialog: None,
            lorian_logo: None,
            app_logo: None,
        };
        apply_style(ctx);
        if let Some(message) = icon_error {
            app.push_log(LogLevel::Warning, message);
        }
        if let Err(err) = install_cjk_font(ctx) {
            app.push_log(
                LogLevel::Warning,
                format!("加载中文字体失败，界面文字可能无法显示：{err:#}"),
            );
        }
        if let Some(bytes) = LORIAN_TOOL_LOGO {
            match load_logo_texture(ctx, "lorian_tool_logo", bytes) {
                Ok(texture) => app.lorian_logo = Some(texture),
                Err(err) => app.push_log(
                    LogLevel::Warning,
                    format!("加载 LorianTool 图标失败：{err:#}"),
                ),
            }
        }
        match load_logo_texture(ctx, "app_logo", APP_LOGO) {
            Ok(texture) => app.app_logo = Some(texture),
            Err(err) => app.push_log(LogLevel::Warning, format!("加载软件图标失败：{err:#}")),
        }
        app.rescan_instances();
        app
    }

    fn push_log(&mut self, level: LogLevel, message: impl Into<String>) {
        self.logs.push(LogEntry {
            level,
            message: message.into(),
        });
    }

    fn rescan_instances(&mut self) {
        let mut entries = Vec::new();
        let instances = launcher::scan_all_instances(
            &self.selected_dirs,
            &mut LogSink::new(&mut |entry| entries.push(entry)),
        );
        self.logs.extend(entries);

        let template = self.selected_instance(self.template_index).cloned();
        let target = self.selected_instance(self.target_index).cloned();
        self.instances = instances;
        self.template_index = template.and_then(|t| self.instances.iter().position(|i| *i == t));
        self.target_index = target.and_then(|t| self.instances.iter().position(|i| *i == t));
    }

    fn selected_instance(&self, index: Option<usize>) -> Option<&DetectedInstance> {
        index.and_then(|i| self.instances.get(i))
    }

    fn is_running(&self) -> bool {
        self.worker.is_some()
    }

    fn needs_shared_dir(&self) -> bool {
        [self.voxy_choice, self.bobby_choice].contains(&CacheChoice::LinkToShared)
            || [self.pack_choice, self.blueprint_choice].contains(&PackChoice::LinkToShared)
    }

    fn build_request(&self) -> Result<MigrationRequest, String> {
        let template = self
            .selected_instance(self.template_index)
            .ok_or("请选择模板实例")?;
        let target = self
            .selected_instance(self.target_index)
            .ok_or("请选择目标实例")?;
        if template.game_dir == target.game_dir {
            return Err("模板实例与目标实例不能相同".to_string());
        }
        let voxy = self.voxy_choice.to_transfer(&self.shared_dir)?;
        let bobby = self.bobby_choice.to_transfer(&self.shared_dir)?;
        let resource_packs = self.pack_choice.to_transfer(&self.shared_dir)?;
        let axiom_blueprints = self.blueprint_choice.to_transfer(&self.shared_dir)?;
        if !self.sync_options
            && !self.sync_mod_configs
            && !self.sync_maps
            && voxy.is_none()
            && bobby.is_none()
            && resource_packs.is_none()
            && axiom_blueprints.is_none()
        {
            return Err("请至少选择一项同步内容".to_string());
        }
        Ok(MigrationRequest {
            template: template.clone(),
            target: target.clone(),
            mode: self.mode,
            sync_options: self.sync_options,
            sync_mod_configs: self.sync_mod_configs,
            sync_maps: self.sync_maps,
            voxy,
            bobby,
            resource_packs,
            axiom_blueprints,
        })
    }

    fn start_migration(&mut self, ctx: &egui::Context) {
        let request = match self.build_request() {
            Ok(request) => request,
            Err(message) => {
                self.push_log(LogLevel::Error, message);
                return;
            }
        };
        let (sender, receiver) = mpsc::channel();
        let ctx = ctx.clone();
        // 复制 Voxy 或 Bobby 缓存可能耗时数分钟，放到后台线程以免阻塞界面
        thread::spawn(move || {
            // 发送失败只会发生在窗口关闭、接收端已释放之后，此时没有可写日志的地方
            let mut emit = |entry: LogEntry| {
                sender.send(WorkerMessage::Log(entry)).ok();
                ctx.request_repaint();
            };
            let mut resolve = |conflicts: Vec<PackConflict>| -> Result<Vec<ConflictChoice>> {
                let (reply, answer) = mpsc::channel();
                sender
                    .send(WorkerMessage::Conflicts(conflicts, reply))
                    .map_err(|_| anyhow!("界面已关闭，无法询问同名冲突的处理方式"))?;
                ctx.request_repaint();
                answer
                    .recv()
                    .map_err(|_| anyhow!("未收到同名冲突的处理选择"))
            };
            let outcome =
                engine::run_migration(&request, &mut resolve, &mut LogSink::new(&mut emit));
            sender.send(WorkerMessage::Finished(outcome)).ok();
            ctx.request_repaint();
        });
        self.worker = Some(receiver);
        self.status = RunStatus::Running;
    }

    fn drain_worker_messages(&mut self) {
        let Some(receiver) = &self.worker else {
            return;
        };
        let mut received = Vec::new();
        let mut finished = None;
        loop {
            match receiver.try_recv() {
                Ok(WorkerMessage::Log(entry)) => received.push(entry),
                Ok(WorkerMessage::Conflicts(conflicts, reply)) => {
                    self.conflict_dialog = Some(ConflictDialog {
                        choices: vec![ConflictChoice::KeepExisting; conflicts.len()],
                        conflicts,
                        reply,
                    });
                }
                Ok(WorkerMessage::Finished(outcome)) => {
                    finished = Some(outcome.map_err(|err| format!("迁移失败：{err:#}")));
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    finished = Some(Err("迁移线程意外退出".to_string()));
                    break;
                }
            }
        }
        self.logs.extend(received);
        let Some(outcome) = finished else {
            return;
        };
        self.worker = None;
        match outcome {
            Ok(result) if result.warnings.is_empty() => {
                self.push_log(LogLevel::Success, "迁移完成");
                self.status = RunStatus::Finished { warnings: 0 };
            }
            Ok(result) => {
                let warnings = result.warnings.len();
                self.push_log(
                    LogLevel::Warning,
                    format!("迁移完成，但有 {warnings} 条警告，请查看上方日志"),
                );
                self.status = RunStatus::Finished { warnings };
            }
            Err(message) => {
                self.push_log(LogLevel::Error, message);
                self.status = RunStatus::Failed;
            }
        }
    }

    fn apply_preset(&mut self, preset: &Preset) {
        self.mode = preset.mode;
        self.sync_options = true;
        self.sync_mod_configs = true;
        self.sync_maps = true;
        self.voxy_choice = preset.cache_choice;
        self.bobby_choice = preset.cache_choice;
        self.blueprint_choice = PackChoice::Skip;
        self.push_log(LogLevel::Info, format!("已应用固定配置：{}", preset.name));
    }

    fn show_header(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if let Some(texture) = &self.app_logo {
                ui.add(
                    egui::Image::new(egui::load::SizedTexture::from_handle(texture))
                        .fit_to_exact_size(egui::vec2(HEADER_LOGO_SIZE, HEADER_LOGO_SIZE)),
                );
            }
            ui.label(RichText::new(APP_NAME).size(20.0).strong());
            ui.label(RichText::new("把按键、配置、地图与缓存迁移到新实例").weak());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                egui::widgets::global_theme_preference_buttons(ui);
                ui.separator();
                let lorian_button = match &self.lorian_logo {
                    Some(texture) => egui::Button::image_and_text(
                        egui::Image::new(egui::load::SizedTexture::from_handle(texture))
                            .fit_to_exact_size(egui::vec2(HEADER_ICON_SIZE, HEADER_ICON_SIZE)),
                        "LorianTool",
                    ),
                    None => egui::Button::new("LorianTool"),
                };
                show_link_button(ui, lorian_button, LORIAN_TOOL_URL);
                let github_label = format!("{} GitHub", egui::special_emojis::GITHUB);
                show_link_button(ui, egui::Button::new(github_label), GITHUB_URL);
            });
        });
    }

    fn show_settings(&mut self, ui: &mut egui::Ui) {
        section(ui, "实例", |ui| {
            ui.horizontal(|ui| {
                if ui.button("🔄 重新扫描").clicked() {
                    self.rescan_instances();
                }
                if ui.button("📂 添加目录…").clicked()
                    && let Some(dir) = rfd::FileDialog::new().pick_folder()
                {
                    self.selected_dirs.push(dir);
                    self.rescan_instances();
                }
                ui.label(RichText::new(format!("共 {} 个实例", self.instances.len())).weak());
            });
            for dir in &self.selected_dirs {
                ui.label(
                    RichText::new(format!("已添加：{}", dir.display()))
                        .weak()
                        .small(),
                );
            }
            ui.add_space(4.0);
            field_label(ui, "模板实例（从这里读取）");
            show_instance_combo(
                ui,
                "template_combo",
                &self.instances,
                &mut self.template_index,
            );
            field_label(ui, "目标实例（写入到这里）");
            show_instance_combo(ui, "target_combo", &self.instances, &mut self.target_index);
        });

        section(ui, "固定配置", |ui| {
            ui.horizontal_wrapped(|ui| {
                for preset in PRESETS {
                    if ui.button(preset.name).clicked() {
                        self.apply_preset(preset);
                    }
                }
            });
            ui.label(
                RichText::new("点击后填好下方选项，仍可逐项修改")
                    .weak()
                    .small(),
            );
        });

        section(ui, "按键与配置", |ui| {
            ui.checkbox(&mut self.sync_options, "按键与选项（options.txt）");
            ui.checkbox(&mut self.sync_mod_configs, "Mod 独立配置（Axiom 等）");
            field_label(ui, "同步模式");
            ui.radio_value(&mut self.mode, SyncMode::Merge, "局部合并：只同步按键");
            ui.radio_value(
                &mut self.mode,
                SyncMode::FullOverwrite,
                "全量覆写：以模板为准",
            );
        });

        section(ui, "地图与缓存", |ui| {
            ui.checkbox(&mut self.sync_maps, "地图与路点（Xaero）");
            for (kind, choice) in [
                (ServerCache::Voxy, &mut self.voxy_choice),
                (ServerCache::Bobby, &mut self.bobby_choice),
            ] {
                field_label(ui, &format!("{kind} 缓存"));
                show_cache_combo(ui, kind, choice);
            }
        });

        section(ui, "材质包与蓝图", |ui| {
            for (kind, choice) in [
                (SharedAsset::ResourcePacks, &mut self.pack_choice),
                (SharedAsset::AxiomBlueprints, &mut self.blueprint_choice),
            ] {
                field_label(ui, &kind.to_string());
                show_pack_combo(ui, kind, choice);
            }
        });

        if self.needs_shared_dir() {
            section(ui, "共享目录", |ui| {
                match &self.shared_dir {
                    Some(dir) => ui.label(dir.display().to_string()),
                    None => ui.label(
                        RichText::new("尚未选择，使用 Junction 指向共享目录前必须先选择")
                            .color(ui.visuals().warn_fg_color),
                    ),
                };
                if ui.button("📂 选择共享目录…").clicked()
                    && let Some(dir) = rfd::FileDialog::new().pick_folder()
                {
                    self.shared_dir = Some(dir);
                }
            });
        }
    }

    fn show_action_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            match self.status {
                RunStatus::Idle => {
                    ui.label(RichText::new("就绪").weak());
                }
                RunStatus::Running => {
                    ui.spinner();
                    ui.label("正在迁移…");
                }
                RunStatus::Finished { warnings: 0 } => {
                    ui.label(RichText::new("✔ 迁移完成").color(log_color(ui, LogLevel::Success)));
                }
                RunStatus::Finished { warnings } => {
                    ui.label(
                        RichText::new(format!("⚠ 迁移完成，{warnings} 条警告"))
                            .color(log_color(ui, LogLevel::Warning)),
                    );
                }
                RunStatus::Failed => {
                    ui.label(RichText::new("✖ 迁移失败").color(log_color(ui, LogLevel::Error)));
                }
            }
            if let (Some(template), Some(target)) = (
                self.selected_instance(self.template_index),
                self.selected_instance(self.target_index),
            ) {
                ui.separator();
                ui.label(RichText::new(format!("{} → {}", template.name, target.name)).weak());
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let button = egui::Button::new(
                    RichText::new("开始迁移")
                        .size(16.0)
                        .strong()
                        .color(Color32::WHITE),
                )
                .fill(ACCENT)
                .min_size(egui::vec2(140.0, 34.0));
                if ui.add_enabled(!self.is_running(), button).clicked() {
                    self.start_migration(ctx);
                }
            });
        });
        ui.add_space(6.0);
    }

    fn show_log_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("日志").size(16.0).strong());
            let count = |level| {
                self.logs
                    .iter()
                    .filter(|entry| entry.level == level)
                    .count()
            };
            let (warnings, errors) = (count(LogLevel::Warning), count(LogLevel::Error));
            if warnings > 0 {
                ui.label(
                    RichText::new(format!("⚠ {warnings}")).color(log_color(ui, LogLevel::Warning)),
                );
            }
            if errors > 0 {
                ui.label(
                    RichText::new(format!("✖ {errors}")).color(log_color(ui, LogLevel::Error)),
                );
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add_enabled(!self.is_running(), egui::Button::new("清空"))
                    .clicked()
                {
                    self.logs.clear();
                    self.status = RunStatus::Idle;
                }
            });
        });
        ui.separator();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                if self.logs.is_empty() {
                    ui.label(RichText::new("还没有日志").weak());
                }
                for entry in &self.logs {
                    let color = log_color(ui, entry.level);
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            RichText::new(log_icon(entry.level))
                                .color(color)
                                .monospace(),
                        );
                        ui.label(RichText::new(&entry.message).color(color));
                    });
                }
            });
    }

    fn show_conflict_dialog(&mut self, ctx: &egui::Context) {
        let Some(dialog) = &mut self.conflict_dialog else {
            return;
        };
        let mut confirmed = false;
        egui::Modal::new(egui::Id::unique("pack_conflicts")).show(ctx, |ui| {
            ui.set_max_width(560.0);
            let kind = dialog
                .conflicts
                .first()
                .map_or(SharedAsset::ResourcePacks, |conflict| conflict.kind);
            ui.label(RichText::new(format!("{kind}同名冲突")).size(18.0).strong());
            ui.label(format!(
                "以下{kind}名称相同但内容不同，请逐个决定保留现有版本还是使用新版本。被替换的版本会移入同级的 .bak 备份目录。"
            ));
            ui.separator();
            egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                for (conflict, choice) in dialog.conflicts.iter().zip(&mut dialog.choices) {
                    card_frame(ui).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(RichText::new(&conflict.pack_name).strong());
                        ui.label(format!("现有：{}", describe_version(&conflict.existing)));
                        ui.label(format!("新版本：{}", describe_version(&conflict.incoming)));
                        ui.horizontal(|ui| {
                            ui.radio_value(choice, ConflictChoice::KeepExisting, "保留现有");
                            ui.radio_value(choice, ConflictChoice::UseIncoming, "使用新版本");
                        });
                    });
                    ui.add_space(4.0);
                }
            });
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("全部保留现有").clicked() {
                    dialog.choices.fill(ConflictChoice::KeepExisting);
                }
                if ui.button("全部使用新版本").clicked() {
                    dialog.choices.fill(ConflictChoice::UseIncoming);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let confirm = egui::Button::new(RichText::new("确定").color(Color32::WHITE))
                        .fill(ACCENT);
                    if ui.add(confirm).clicked() {
                        confirmed = true;
                    }
                });
            });
        });
        if confirmed && let Some(dialog) = self.conflict_dialog.take() {
            // 后台线程已结束时发送会失败，迁移结果会通过 Finished 消息另行报告
            dialog.reply.send(dialog.choices).ok();
        }
    }
}

impl eframe::App for MigratorApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain_worker_messages();
        let ctx = ui.ctx().clone();

        egui::Panel::top("header").show(ui, |ui| {
            ui.add_space(6.0);
            self.show_header(ui);
            ui.add_space(6.0);
        });
        egui::Panel::bottom("action_bar").show(ui, |ui| {
            self.show_action_bar(ui, &ctx);
        });
        egui::Panel::left("settings")
            .resizable(true)
            .default_size(620.0)
            .size_range(400.0..=760.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.add_space(6.0);
                        ui.add_enabled_ui(!self.is_running(), |ui| self.show_settings(ui));
                    });
            });
        egui::CentralPanel::default().show(ui, |ui| {
            self.show_log_panel(ui);
        });
        self.show_conflict_dialog(&ctx);
    }
}

/// 统一圆角、间距与强调色；`all_styles_mut` 同时作用于浅色与深色主题。
fn apply_style(ctx: &egui::Context) {
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(10.0, 4.0);
        style.spacing.interact_size.y = 26.0;
        let radius = egui::CornerRadius::same(6);
        style.visuals.widgets.noninteractive.corner_radius = radius;
        style.visuals.widgets.inactive.corner_radius = radius;
        style.visuals.widgets.hovered.corner_radius = radius;
        style.visuals.widgets.active.corner_radius = radius;
        style.visuals.widgets.open.corner_radius = radius;
        style.visuals.window_corner_radius = egui::CornerRadius::same(10);
        style.visuals.selection.bg_fill = ACCENT;
        style.visuals.hyperlink_color = ACCENT;
    });
}

fn show_link_button(ui: &mut egui::Ui, button: egui::Button, url: &str) {
    if ui.add(button.frame(false)).on_hover_text(url).clicked() {
        ui.ctx().open_url(egui::OpenUrl::new_tab(url));
    }
}

/// 解码内嵌的 PNG 图标；egui 本身不解码 PNG，借助 `image` 完成。
fn decode_png(bytes: &[u8]) -> Result<image::RgbaImage> {
    Ok(
        image::load_from_memory_with_format(bytes, image::ImageFormat::Png)
            .context("PNG 解码失败")?
            .to_rgba8(),
    )
}

fn load_logo_texture(ctx: &egui::Context, name: &str, bytes: &[u8]) -> Result<egui::TextureHandle> {
    let rgba = decode_png(bytes)?;
    let size = [rgba.width() as usize, rgba.height() as usize];
    let pixels = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
    Ok(ctx.load_texture(name, pixels, egui::TextureOptions::LINEAR))
}

fn card_frame(ui: &egui::Ui) -> egui::Frame {
    egui::Frame::new()
        .fill(ui.visuals().faint_bg_color)
        .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
        .corner_radius(egui::CornerRadius::same(8))
        .inner_margin(egui::Margin::same(12))
}

/// 带标题的卡片分组，卡片宽度撑满所在面板。
fn section(ui: &mut egui::Ui, title: &str, add_contents: impl FnOnce(&mut egui::Ui)) {
    card_frame(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new(title).size(15.0).strong().color(ACCENT));
        ui.add_space(2.0);
        add_contents(ui);
    });
}

/// 字段标题使用主题的强调文字颜色（浅色主题为黑色），深色主题下仍保持可读。
fn field_label(ui: &mut egui::Ui, text: &str) {
    ui.add_space(2.0);
    ui.label(
        RichText::new(text)
            .size(14.0)
            .color(ui.visuals().strong_text_color()),
    );
}

fn log_icon(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Info => "•",
        LogLevel::Skip => "–",
        LogLevel::Success => "✔",
        LogLevel::Warning => "⚠",
        LogLevel::Error => "✖",
    }
}

/// 按当前主题选择日志颜色，深色主题使用更亮的色值以保证对比度。
fn log_color(ui: &egui::Ui, level: LogLevel) -> Color32 {
    let dark = ui.visuals().dark_mode;
    match level {
        LogLevel::Info => ui.visuals().text_color(),
        LogLevel::Skip => ui.visuals().weak_text_color(),
        LogLevel::Success if dark => Color32::from_rgb(120, 200, 140),
        LogLevel::Success => Color32::from_rgb(30, 120, 60),
        LogLevel::Warning if dark => Color32::from_rgb(235, 190, 90),
        LogLevel::Warning => Color32::from_rgb(170, 110, 0),
        LogLevel::Error if dark => Color32::from_rgb(240, 110, 110),
        LogLevel::Error => Color32::from_rgb(190, 40, 40),
    }
}

fn describe_version(version: &PackVersion) -> String {
    let mut text = format!(
        "{} · {} · {}",
        version.owner,
        format_size(version.size),
        format_age(version.modified)
    );
    if let Some(data_version) = version.data_version {
        text.push_str(&format!(" · 数据版本 {data_version}"));
    }
    text
}

fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let bytes = bytes as f64;
    if bytes >= KB * KB {
        format!("{:.1} MB", bytes / (KB * KB))
    } else {
        format!("{:.1} KB", bytes / KB)
    }
}

/// 只显示相对天数，避免为本地时区换算引入新依赖。
fn format_age(modified: Option<SystemTime>) -> String {
    let Some(elapsed) = modified.and_then(|time| SystemTime::now().duration_since(time).ok())
    else {
        return "修改时间未知".to_string();
    };
    match elapsed.as_secs() / 86_400 {
        0 => "今天修改".to_string(),
        days => format!("{days} 天前修改"),
    }
}

fn show_instance_combo(
    ui: &mut egui::Ui,
    id: &str,
    instances: &[DetectedInstance],
    selected: &mut Option<usize>,
) {
    let selected_text = selected
        .and_then(|i| instances.get(i))
        .map(ToString::to_string)
        .unwrap_or_else(|| "请选择".to_string());
    egui::ComboBox::from_id_salt(id)
        .width(ui.available_width())
        .selected_text(selected_text)
        .show_ui(ui, |ui| {
            for (index, instance) in instances.iter().enumerate() {
                ui.selectable_value(selected, Some(index), instance.to_string())
                    .on_hover_text(instance.game_dir.display().to_string());
            }
        });
}

fn show_cache_combo(ui: &mut egui::Ui, kind: ServerCache, choice: &mut CacheChoice) {
    egui::ComboBox::from_id_salt(format!("cache_combo_{kind}"))
        .width(ui.available_width())
        .selected_text(choice.label())
        .show_ui(ui, |ui| {
            for option in CacheChoice::ALL {
                ui.selectable_value(choice, option, option.label());
            }
        });
}

fn show_pack_combo(ui: &mut egui::Ui, kind: SharedAsset, choice: &mut PackChoice) {
    egui::ComboBox::from_id_salt(format!("pack_combo_{kind}"))
        .width(ui.available_width())
        .selected_text(choice.label(kind))
        .show_ui(ui, |ui| {
            for option in PackChoice::ALL {
                ui.selectable_value(choice, option, option.label(kind));
            }
        });
}

/// egui 自带字体不含中文字形，从系统字体目录加载微软雅黑。
fn install_cjk_font(ctx: &egui::Context) -> Result<()> {
    let windows_dir = env::var_os("WINDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    let font_path = windows_dir.join("Fonts").join("msyh.ttc");
    let bytes =
        fs::read(&font_path).with_context(|| format!("读取 {} 失败", font_path.display()))?;

    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        CJK_FONT_NAME.to_string(),
        Arc::new(egui::FontData::from_owned(bytes)),
    );
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .push(CJK_FONT_NAME.to_string());
    }
    ctx.set_fonts(fonts);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_logos_decode() -> Result<()> {
        let ctx = egui::Context::default();
        let logos = [("lorian", LORIAN_TOOL_LOGO), ("app", Some(APP_LOGO))];
        for (name, bytes) in logos.into_iter().filter_map(|(n, b)| b.map(|b| (n, b))) {
            let [width, height] = load_logo_texture(&ctx, name, bytes)?.size();
            assert!(width > 0 && height > 0, "{name}");
        }
        Ok(())
    }
}
