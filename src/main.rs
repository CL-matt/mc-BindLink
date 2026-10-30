#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod engine;
mod launcher;
mod map_sync;
#[cfg(test)]
mod test_support;
mod types;

use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;

use anyhow::{Context, Result};
use eframe::egui;

use crate::types::{DetectedInstance, MigrationRequest, MigrationResult, SyncMode, VoxyTransfer};

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
    Finished(Result<MigrationResult>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VoxyChoice {
    Skip,
    Copy,
    LinkToSource,
    LinkToShared,
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
    voxy_choice: VoxyChoice,
    shared_dir: Option<PathBuf>,
    logs: Vec<String>,
    worker: Option<Receiver<WorkerMessage>>,
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
            voxy_choice: VoxyChoice::Skip,
            shared_dir: None,
            logs: Vec::new(),
            worker: None,
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
        let voxy = match self.voxy_choice {
            VoxyChoice::Skip => None,
            VoxyChoice::Copy => Some(VoxyTransfer::Copy),
            VoxyChoice::LinkToSource => Some(VoxyTransfer::LinkToSource),
            VoxyChoice::LinkToShared => Some(VoxyTransfer::LinkToShared(
                self.shared_dir.clone().ok_or("请先选择 Voxy 共享目录")?,
            )),
        };
        if !self.sync_options && !self.sync_mod_configs && !self.sync_maps && voxy.is_none() {
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
        // 复制 Voxy 缓存可能耗时数分钟，放到后台线程以免阻塞界面
        thread::spawn(move || {
            // 发送失败只会发生在窗口关闭、接收端已释放之后，此时没有可写日志的地方
            let mut log = |message: String| {
                sender.send(WorkerMessage::Log(message)).ok();
                ctx.request_repaint();
            };
            let outcome = engine::run_migration(&request, &mut log);
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

        ui.label("Voxy 缓存");
        ui.radio_value(&mut self.voxy_choice, VoxyChoice::Skip, "不处理");
        ui.radio_value(
            &mut self.voxy_choice,
            VoxyChoice::Copy,
            "物理复制（迁移后将删除源缓存时选用）",
        );
        ui.radio_value(
            &mut self.voxy_choice,
            VoxyChoice::LinkToSource,
            "Junction 指向源实例（源实例继续使用时选用）",
        );
        ui.radio_value(
            &mut self.voxy_choice,
            VoxyChoice::LinkToShared,
            "Junction 指向共享目录（同一服务器反复更新整合包时选用）",
        );
        if self.voxy_choice == VoxyChoice::LinkToShared {
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
