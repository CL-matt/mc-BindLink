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
use eframe::egui;

use crate::types::{
    CacheTransfer, ConflictChoice, DetectedInstance, MigrationRequest, MigrationResult,
    PackConflict, PackTransfer, PackVersion, ServerCache, SharedAsset, SyncMode,
};

const CJK_FONT_NAME: &str = "msyh";

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([760.0, 680.0])
            .with_min_inner_size([560.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        "MC 按键与配置迁移",
        options,
        Box::new(|cc| Ok(Box::new(MigratorApp::new(&cc.egui_ctx)))),
    )
}

enum WorkerMessage {
    Log(String),
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
    logs: Vec<String>,
    worker: Option<Receiver<WorkerMessage>>,
    conflict_dialog: Option<ConflictDialog>,
}

impl MigratorApp {
    fn new(ctx: &egui::Context) -> Self {
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
            worker: None,
            conflict_dialog: None,
        };
        if let Err(err) = install_cjk_font(ctx) {
            app.logs
                .push(format!("加载中文字体失败，界面文字可能无法显示：{err:#}"));
        }
        app.rescan_instances();
        app
    }

    fn rescan_instances(&mut self) {
        let mut messages = Vec::new();
        let instances =
            launcher::scan_all_instances(&self.selected_dirs, &mut |m| messages.push(m));
        self.logs.extend(messages);

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
                self.logs.push(message);
                return;
            }
        };
        let (sender, receiver) = mpsc::channel();
        let ctx = ctx.clone();
        // 复制 Voxy 或 Bobby 缓存可能耗时数分钟，放到后台线程以免阻塞界面
        thread::spawn(move || {
            // 发送失败只会发生在窗口关闭、接收端已释放之后，此时没有可写日志的地方
            let mut log = |message: String| {
                sender.send(WorkerMessage::Log(message)).ok();
                ctx.request_repaint();
            };
            let mut resolve = |conflicts: Vec<PackConflict>| -> Result<Vec<ConflictChoice>> {
                let (reply, answer) = mpsc::channel();
                sender
                    .send(WorkerMessage::Conflicts(conflicts, reply))
                    .map_err(|_| anyhow!("界面已关闭，无法询问材质包冲突的处理方式"))?;
                ctx.request_repaint();
                answer
                    .recv()
                    .map_err(|_| anyhow!("未收到材质包冲突的处理选择"))
            };
            let outcome = engine::run_migration(&request, &mut resolve, &mut log);
            sender.send(WorkerMessage::Finished(outcome)).ok();
            ctx.request_repaint();
        });
        self.worker = Some(receiver);
    }

    fn drain_worker_messages(&mut self) {
        let Some(receiver) = &self.worker else {
            return;
        };
        let mut finished = false;
        loop {
            match receiver.try_recv() {
                Ok(WorkerMessage::Log(message)) => self.logs.push(message),
                Ok(WorkerMessage::Conflicts(conflicts, reply)) => {
                    self.conflict_dialog = Some(ConflictDialog {
                        choices: vec![ConflictChoice::KeepExisting; conflicts.len()],
                        conflicts,
                        reply,
                    });
                }
                Ok(WorkerMessage::Finished(outcome)) => {
                    match outcome {
                        Ok(result) if result.warnings.is_empty() => {
                            self.logs.push("迁移完成".to_string());
                        }
                        Ok(result) => self.logs.push(format!(
                            "迁移完成，但有 {} 条警告，请查看上方日志",
                            result.warnings.len()
                        )),
                        Err(err) => self.logs.push(format!("迁移失败：{err:#}")),
                    }
                    finished = true;
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.logs.push("迁移线程意外退出".to_string());
                    finished = true;
                    break;
                }
            }
        }
        if finished {
            self.worker = None;
        }
    }

    fn show_instance_section(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button("重新扫描").clicked() {
                self.rescan_instances();
            }
            if ui.button("添加目录…").clicked()
                && let Some(dir) = rfd::FileDialog::new().pick_folder()
            {
                self.selected_dirs.push(dir);
                self.rescan_instances();
            }
        });
        for dir in &self.selected_dirs {
            ui.label(format!("已添加：{}", dir.display()));
        }

        egui::Grid::new("instance_grid")
            .num_columns(2)
            .show(ui, |ui| {
                ui.label("模板实例");
                show_instance_combo(
                    ui,
                    "template_combo",
                    &self.instances,
                    &mut self.template_index,
                );
                ui.end_row();
                ui.label("目标实例");
                show_instance_combo(ui, "target_combo", &self.instances, &mut self.target_index);
                ui.end_row();
            });
    }

    fn apply_preset(&mut self, preset: &Preset) {
        self.mode = preset.mode;
        self.sync_options = true;
        self.sync_mod_configs = true;
        self.sync_maps = true;
        self.voxy_choice = preset.cache_choice;
        self.bobby_choice = preset.cache_choice;
        self.blueprint_choice = PackChoice::Skip;
        self.logs.push(format!("已应用固定配置：{}", preset.name));
    }

    fn show_preset_section(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("固定配置");
            for preset in PRESETS {
                if ui.button(preset.name).clicked() {
                    self.apply_preset(preset);
                }
            }
        });
    }

    fn show_content_section(&mut self, ui: &mut egui::Ui) {
        ui.checkbox(&mut self.sync_options, "按键与选项（options.txt）");
        ui.checkbox(&mut self.sync_mod_configs, "Mod 独立配置（Axiom 等）");
        ui.horizontal(|ui| {
            ui.label("同步模式");
            ui.radio_value(&mut self.mode, SyncMode::Merge, "局部合并：只同步按键");
            ui.radio_value(
                &mut self.mode,
                SyncMode::FullOverwrite,
                "全量覆写：以模板为准",
            );
        });
        ui.checkbox(&mut self.sync_maps, "地图与路点（Xaero）");

        egui::Grid::new("cache_grid").num_columns(2).show(ui, |ui| {
            for (kind, choice) in [
                (ServerCache::Voxy, &mut self.voxy_choice),
                (ServerCache::Bobby, &mut self.bobby_choice),
            ] {
                ui.label(format!("{kind} 缓存"));
                show_cache_combo(ui, kind, choice);
                ui.end_row();
            }
            for (kind, choice) in [
                (SharedAsset::ResourcePacks, &mut self.pack_choice),
                (SharedAsset::AxiomBlueprints, &mut self.blueprint_choice),
            ] {
                ui.label(kind.to_string());
                show_pack_combo(ui, kind, choice);
                ui.end_row();
            }
        });
        let needs_shared_dir = [self.voxy_choice, self.bobby_choice]
            .contains(&CacheChoice::LinkToShared)
            || [self.pack_choice, self.blueprint_choice].contains(&PackChoice::LinkToShared);
        if needs_shared_dir {
            ui.horizontal(|ui| {
                if ui.button("选择共享目录…").clicked()
                    && let Some(dir) = rfd::FileDialog::new().pick_folder()
                {
                    self.shared_dir = Some(dir);
                }
                match &self.shared_dir {
                    Some(dir) => ui.label(dir.display().to_string()),
                    None => ui.label("尚未选择"),
                };
            });
        }
    }

    fn show_conflict_dialog(&mut self, ctx: &egui::Context) {
        let Some(dialog) = &mut self.conflict_dialog else {
            return;
        };
        let mut confirmed = false;
        egui::Modal::new(egui::Id::unique("pack_conflicts")).show(ctx, |ui| {
            let kind = dialog
                .conflicts
                .first()
                .map_or(SharedAsset::ResourcePacks, |conflict| conflict.kind);
            ui.heading(format!("{kind}同名冲突"));
            ui.label(format!(
                "以下{kind}名称相同但内容不同，请逐个决定保留现有版本还是使用新版本。被替换的版本会移入同级的 .bak 备份目录。"
            ));
            ui.separator();
            egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                for (conflict, choice) in dialog.conflicts.iter().zip(&mut dialog.choices) {
                    ui.group(|ui| {
                        ui.strong(&conflict.pack_name);
                        ui.label(format!("现有：{}", describe_version(&conflict.existing)));
                        ui.label(format!("新版本：{}", describe_version(&conflict.incoming)));
                        ui.horizontal(|ui| {
                            ui.radio_value(choice, ConflictChoice::KeepExisting, "保留现有");
                            ui.radio_value(choice, ConflictChoice::UseIncoming, "使用新版本");
                        });
                    });
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
                if ui.button("确定").clicked() {
                    confirmed = true;
                }
            });
        });
        if confirmed && let Some(dialog) = self.conflict_dialog.take() {
            // 后台线程已结束时发送会失败，迁移结果会通过 Finished 消息另行报告
            dialog.reply.send(dialog.choices).ok();
        }
    }

    fn show_log_section(&self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for line in &self.logs {
                    ui.label(line);
                }
            });
    }
}

impl eframe::App for MigratorApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain_worker_messages();
        let ctx = ui.ctx().clone();

        egui::Panel::top("controls").show(ui, |ui| {
            ui.add_enabled_ui(!self.is_running(), |ui| {
                ui.heading("实例");
                self.show_instance_section(ui);
                ui.separator();
                self.show_preset_section(ui);
                ui.heading("同步内容");
                self.show_content_section(ui);
                ui.separator();
                if ui.button("开始迁移").clicked() {
                    self.start_migration(&ctx);
                }
            });
            if self.is_running() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("正在迁移…");
                });
            }
            ui.add_space(4.0);
        });
        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("日志");
            self.show_log_section(ui);
        });
        self.show_conflict_dialog(&ctx);
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

fn show_pack_combo(ui: &mut egui::Ui, kind: SharedAsset, choice: &mut PackChoice) {
    egui::ComboBox::from_id_salt(format!("pack_combo_{kind}"))
        .width(520.0)
        .selected_text(choice.label(kind))
        .show_ui(ui, |ui| {
            for option in PackChoice::ALL {
                ui.selectable_value(choice, option, option.label(kind));
            }
        });
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
        .width(520.0)
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
        .width(520.0)
        .selected_text(choice.label())
        .show_ui(ui, |ui| {
            for option in CacheChoice::ALL {
                ui.selectable_value(choice, option, option.label());
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
