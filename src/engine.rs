use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io::{Read, Seek};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use toml_edit::DocumentMut;
use zip::ZipArchive;
use zip::result::ZipError;

use crate::journal::Journal;
use crate::map_sync;
use crate::pack_sync::{self, AssetSyncRequest, ConflictResolver};
use crate::types::{
    LogSink, MigrationRequest, MigrationResult, ModDiff, ModEntry, ModVersionChange, PackTransfer,
    ServerCache, SharedAsset, SyncMode,
};

const OPTIONS_FILE_NAME: &str = "options.txt";
const KEY_PREFIX: &str = "key_";
const VERSION_KEY: &str = "version";
/// 1.13 正式版的数据版本号，更早版本的 `options.txt` 用数字键码记录按键。
const FIRST_NAMED_KEY_DATA_VERSION: i64 = 1519;
/// 全量覆写时保留目标原值的条目，它们与数据版本、本机硬件或目标实例的文件绑定。
const FULL_OVERWRITE_BLACKLIST: &[&str] = &[
    VERSION_KEY,
    "overrideWidth",
    "overrideHeight",
    "fullscreenResolution",
    "soundDevice",
    "resourcePacks",
    "incompatibleResourcePacks",
    "lastServer",
];
const MAX_TIMESTAMPED_BACKUPS: usize = 5;
/// `options.txt` 中记录已启用材质包的条目。
const ENABLED_PACK_KEYS: &[&str] = &["resourcePacks", "incompatibleResourcePacks"];
const AXIOM_MOD_ID: &str = "axiom";
/// Axiom 蓝图界面的排列顺序，每行一个蓝图文件名。
const BLUEPRINT_ORDERING_FILE: &str = ".axiom_blueprint_ordering";

/// 独立 Mod 配置文件的同步规则：目标实例安装了 `mod_id` 时才处理 `relative_path`。
struct ModConfigRule {
    mod_id: &'static str,
    relative_path: &'static str,
    /// 合并模式下从模板复制的顶层 JSON 字段。
    merge_fields: &'static [&'static str],
}

const MOD_CONFIG_RULES: &[ModConfigRule] = &[ModConfigRule {
    mod_id: AXIOM_MOD_ID,
    relative_path: "config/axiom/axiom.json",
    merge_fields: &["keybinds"],
}];

/// 按 `request` 依次执行各同步步骤。
///
/// 单个步骤失败时记录到日志与 `MigrationResult::warnings` 并继续后续步骤，
/// 使按键同步不会因为地图或缓存的问题而中断。所有改动登记在 `history_root` 下的迁移记录中，
/// 即使部分步骤失败也可以回退已完成的部分。
pub fn run_migration(
    request: &MigrationRequest,
    history_root: &Path,
    resolve: &mut ConflictResolver,
    log: &mut LogSink,
) -> Result<MigrationResult> {
    let template_dir = &request.template.game_dir;
    let target_dir = &request.target.game_dir;
    if template_dir == target_dir {
        bail!("模板实例与目标实例是同一个目录：{}", template_dir.display());
    }
    log.info(format!(
        "开始迁移：{} → {}",
        request.template, request.target
    ));
    let mut journal = Journal::begin(history_root, &request.template.name, &request.target.name)?;
    let journal = &mut journal;

    let mut result = MigrationResult::default();
    if request.sync_options {
        let outcome = sync_options_file(
            template_dir,
            target_dir,
            request.mode,
            journal,
            log,
            &mut result,
        );
        record_step_failure("options.txt 同步", outcome, log, &mut result);
    }
    if request.sync_mod_configs {
        let outcome = sync_mod_configs(
            template_dir,
            target_dir,
            request.mode,
            journal,
            log,
            &mut result,
        );
        record_step_failure("Mod 配置同步", outcome, log, &mut result);
    }
    if request.sync_maps {
        let outcome = map_sync::sync_map_directories(template_dir, target_dir, journal, log)
            .map(|step| merge_result(&mut result, step));
        record_step_failure("地图与路点同步", outcome, log, &mut result);
    }
    let caches = [
        (ServerCache::Voxy, &request.voxy),
        (ServerCache::Bobby, &request.bobby),
    ];
    for (kind, method) in caches {
        let Some(method) = method else {
            continue;
        };
        let outcome =
            map_sync::transfer_server_caches(kind, template_dir, target_dir, method, journal, log)
                .map(|step| merge_result(&mut result, step));
        record_step_failure(&format!("{kind} 缓存迁移"), outcome, log, &mut result);
    }
    if let Some(method) = &request.resource_packs {
        let outcome = pack_sync::sync_shared_assets(
            &AssetSyncRequest {
                kind: SharedAsset::ResourcePacks,
                template: &request.template,
                target: &request.target,
                method,
                max_data_version: None,
            },
            resolve,
            journal,
            log,
        )
        .map(|step| merge_result(&mut result, step.result));
        record_step_failure("材质包同步", outcome, log, &mut result);
        // 只有共享目录保证两个实例拥有相同的材质包文件，启用列表才能安全照搬
        if matches!(method, PackTransfer::LinkToShared(_)) {
            let outcome =
                sync_enabled_pack_list(template_dir, target_dir, journal, log, &mut result);
            record_step_failure("材质包启用列表同步", outcome, log, &mut result);
        }
    }
    if let Some(method) = &request.axiom_blueprints {
        let outcome = sync_axiom_blueprints(request, method, resolve, journal, log, &mut result);
        record_step_failure("Axiom 蓝图同步", outcome, log, &mut result);
    }
    if let Some(method) = &request.screenshots {
        let outcome = pack_sync::sync_shared_assets(
            &AssetSyncRequest {
                kind: SharedAsset::Screenshots,
                template: &request.template,
                target: &request.target,
                method,
                max_data_version: None,
            },
            resolve,
            journal,
            log,
        )
        .map(|step| merge_result(&mut result, step.result));
        record_step_failure("截图同步", outcome, log, &mut result);
    }

    log.info(format!(
        "迁移结束：修改 {} 处，警告 {} 条",
        result.changed_paths.len(),
        result.warnings.len()
    ));
    if request.report_mod_changes {
        match diff_installed_mods(template_dir, target_dir, log) {
            Ok(diff) => {
                log_mod_diff(&diff, log);
                result.mod_diff = Some(diff);
            }
            Err(err) => record_step_failure("Mod 变化对比", Err(err), log, &mut result),
        }
    }
    journal.finish()?;
    Ok(result)
}

/// 按 Mod ID 对比模板（旧）与目标（新）实例 `mods/` 中的 Mod。
pub fn diff_installed_mods(
    template_dir: &Path,
    target_dir: &Path,
    log: &mut LogSink,
) -> Result<ModDiff> {
    let old = scan_installed_mods(&template_dir.join("mods"), log)?;
    let new = scan_installed_mods(&target_dir.join("mods"), log)?;
    let mut diff = ModDiff::default();
    for (id, from) in &old {
        match new.get(id) {
            Some(to) if to != from => diff.changed.push(ModVersionChange {
                id: id.clone(),
                from: from.clone(),
                to: to.clone(),
            }),
            Some(_) => {}
            None => diff.removed.push(ModEntry {
                id: id.clone(),
                version: from.clone(),
            }),
        }
    }
    for (id, version) in &new {
        if !old.contains_key(id) {
            diff.added.push(ModEntry {
                id: id.clone(),
                version: version.clone(),
            });
        }
    }
    Ok(diff)
}

fn log_mod_diff(diff: &ModDiff, log: &mut LogSink) {
    log.info(format!(
        "Mod 变化：版本变化 {} 个，新增 {} 个，移除 {} 个",
        diff.changed.len(),
        diff.added.len(),
        diff.removed.len()
    ));
    for change in &diff.changed {
        log.info(format!(
            "  版本变化 {}：{} → {}",
            change.id, change.from, change.to
        ));
    }
    for entry in &diff.added {
        log.info(format!("  新增 {} {}", entry.id, entry.version));
    }
    for entry in &diff.removed {
        log.info(format!("  移除 {} {}", entry.id, entry.version));
    }
}

fn record_step_failure(
    step: &str,
    outcome: Result<()>,
    log: &mut LogSink,
    result: &mut MigrationResult,
) {
    if let Err(err) = outcome {
        let message = format!("{step}失败：{err:#}");
        log.error(message.clone());
        result.warnings.push(message);
    }
}

fn merge_result(result: &mut MigrationResult, step: MigrationResult) {
    result.changed_paths.extend(step.changed_paths);
    result.warnings.extend(step.warnings);
}

/// 写入前备份 `path`，返回本次使用的备份路径；`path` 不存在时返回 `None`。
///
/// 第一次备份生成 `<文件>.bak` 并永久保留最早的原始内容，之后生成
/// `<文件>.<unix秒>.bak`，同一文件只保留最近若干份带时间戳的备份。
pub fn backup_file_safely(path: &Path) -> Result<Option<PathBuf>> {
    if !path.is_file() {
        return Ok(None);
    }
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow!("无法取得文件名：{}", path.display()))?
        .to_string_lossy()
        .into_owned();

    let original_backup = path.with_file_name(format!("{file_name}.bak"));
    if !original_backup.exists() {
        fs::copy(path, &original_backup)
            .with_context(|| format!("备份 {} 失败", path.display()))?;
        return Ok(Some(original_backup));
    }

    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("系统时间早于 1970 年")?
        .as_secs();
    let stamped_backup = path.with_file_name(format!("{file_name}.{seconds}.bak"));
    // 同一秒内再次写入时，文件当前内容来自本程序刚完成的写入，沿用已有备份即可保留更早的状态
    if !stamped_backup.exists() {
        fs::copy(path, &stamped_backup).with_context(|| format!("备份 {} 失败", path.display()))?;
    }
    prune_timestamped_backups(path, &file_name)?;
    Ok(Some(stamped_backup))
}

fn prune_timestamped_backups(path: &Path, file_name: &str) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    let prefix = format!("{file_name}.");
    let mut backups = Vec::new();
    for entry in
        fs::read_dir(parent).with_context(|| format!("读取目录 {} 失败", parent.display()))?
    {
        let entry = entry.with_context(|| format!("读取目录 {} 失败", parent.display()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let seconds = name
            .strip_prefix(&prefix)
            .and_then(|rest| rest.strip_suffix(".bak"))
            .and_then(|stamp| stamp.parse::<u64>().ok());
        if let Some(seconds) = seconds {
            backups.push((seconds, entry.path()));
        }
    }
    backups.sort_by_key(|(seconds, _)| std::cmp::Reverse(*seconds));
    for (_, stale) in backups.into_iter().skip(MAX_TIMESTAMPED_BACKUPS) {
        fs::remove_file(&stale).with_context(|| format!("删除旧备份 {} 失败", stale.display()))?;
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum OptionLine {
    Entry { key: String, value: String },
    Raw(String),
}

/// 按行保存的 `options.txt`，保留原有条目顺序、无法解析的行与换行符。
#[derive(Debug, Clone)]
struct OptionsFile {
    lines: Vec<OptionLine>,
    line_ending: &'static str,
}

impl OptionsFile {
    fn parse(text: &str) -> Self {
        let line_ending = if text.contains("\r\n") { "\r\n" } else { "\n" };
        let lines = text
            .lines()
            .map(|line| match line.split_once(':') {
                Some((key, value)) if !key.is_empty() => OptionLine::Entry {
                    key: key.to_string(),
                    value: value.to_string(),
                },
                _ => OptionLine::Raw(line.to_string()),
            })
            .collect();
        Self { lines, line_ending }
    }

    fn load(path: &Path) -> Result<Self> {
        let text =
            fs::read_to_string(path).with_context(|| format!("读取 {} 失败", path.display()))?;
        Ok(Self::parse(&text))
    }

    fn to_text(&self) -> String {
        let mut text = String::new();
        for line in &self.lines {
            match line {
                OptionLine::Entry { key, value } => {
                    text.push_str(key);
                    text.push(':');
                    text.push_str(value);
                }
                OptionLine::Raw(raw) => text.push_str(raw),
            }
            text.push_str(self.line_ending);
        }
        text
    }

    fn entries(&self) -> impl Iterator<Item = (&str, &str)> {
        self.lines.iter().filter_map(|line| match line {
            OptionLine::Entry { key, value } => Some((key.as_str(), value.as_str())),
            OptionLine::Raw(_) => None,
        })
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.entries().find(|(k, _)| *k == key).map(|(_, v)| v)
    }

    /// 覆盖已有条目或追加到末尾，返回内容是否发生变化。
    fn set(&mut self, key: &str, value: &str) -> bool {
        for line in &mut self.lines {
            if let OptionLine::Entry { key: k, value: v } = line
                && k == key
            {
                if v == value {
                    return false;
                }
                *v = value.to_string();
                return true;
            }
        }
        self.lines.push(OptionLine::Entry {
            key: key.to_string(),
            value: value.to_string(),
        });
        true
    }

    /// 判断按键是否使用 1.13 之前的数字键码；既无数据版本也无按键条目时无法判断。
    fn uses_legacy_key_codes(&self) -> Option<bool> {
        if let Some(version) = self
            .get(VERSION_KEY)
            .and_then(|v| v.trim().parse::<i64>().ok())
        {
            return Some(version < FIRST_NAMED_KEY_DATA_VERSION);
        }
        self.entries()
            .find(|(key, _)| key.starts_with(KEY_PREFIX))
            .map(|(_, value)| value.trim().parse::<i64>().is_ok())
    }
}

/// 按 `mode` 把模板条目写入 `target`，返回发生变化的条目数。
fn apply_options_mapping(
    template: &OptionsFile,
    target: &mut OptionsFile,
    mode: SyncMode,
    log: &mut LogSink,
) -> usize {
    let key_format_mismatch = matches!(
        (template.uses_legacy_key_codes(), target.uses_legacy_key_codes()),
        (Some(a), Some(b)) if a != b
    );
    if key_format_mismatch {
        log.warn("模板与目标分处 1.13 两侧，按键格式不兼容，已跳过全部 key_ 条目".to_string());
    }

    let mut changed = 0;
    for (key, value) in template.entries() {
        let is_key_binding = key.starts_with(KEY_PREFIX);
        let wanted = match mode {
            SyncMode::Merge => is_key_binding,
            SyncMode::FullOverwrite => !FULL_OVERWRITE_BLACKLIST.contains(&key),
        };
        if !wanted || (is_key_binding && key_format_mismatch) {
            continue;
        }
        if target.set(key, value) {
            changed += 1;
        }
    }
    changed
}

fn sync_options_file(
    template_dir: &Path,
    target_dir: &Path,
    mode: SyncMode,
    journal: &mut Journal,
    log: &mut LogSink,
    result: &mut MigrationResult,
) -> Result<()> {
    let template_path = template_dir.join(OPTIONS_FILE_NAME);
    let target_path = target_dir.join(OPTIONS_FILE_NAME);
    if !template_path.is_file() {
        bail!("模板实例没有 {}", template_path.display());
    }
    // 新文件缺少 version 条目会让游戏按最旧格式升级配置，而 version 又禁止从模板复制
    if !target_path.is_file() {
        let message = format!(
            "目标实例尚未生成 {}，请先启动一次游戏再迁移",
            target_path.display()
        );
        log.warn(message.clone());
        result.warnings.push(message);
        return Ok(());
    }

    let template = OptionsFile::load(&template_path)?;
    let mut target = OptionsFile::load(&target_path)?;
    let changed = apply_options_mapping(&template, &mut target, mode, log);
    if changed == 0 {
        log.skip("options.txt：与模板一致，无需修改".to_string());
        return Ok(());
    }

    if let Some(backup) = backup_file_safely(&target_path)? {
        log.info(format!("已备份到 {}", backup.display()));
    }
    journal.write_file(&target_path, target.to_text())?;
    log.success(format!("options.txt：更新 {changed} 个条目"));
    result.changed_paths.push(target_path);
    Ok(())
}

/// 把模板 `options.txt` 中的材质包启用列表写入目标，目标没有 `options.txt` 时跳过。
fn sync_enabled_pack_list(
    template_dir: &Path,
    target_dir: &Path,
    journal: &mut Journal,
    log: &mut LogSink,
    result: &mut MigrationResult,
) -> Result<()> {
    let template_path = template_dir.join(OPTIONS_FILE_NAME);
    let target_path = target_dir.join(OPTIONS_FILE_NAME);
    if !template_path.is_file() || !target_path.is_file() {
        log.skip("材质包启用列表：模板或目标缺少 options.txt，跳过".to_string());
        return Ok(());
    }
    let template = OptionsFile::load(&template_path)?;
    let mut target = OptionsFile::load(&target_path)?;
    let mut changed = 0;
    for key in ENABLED_PACK_KEYS {
        if let Some(value) = template.get(key)
            && target.set(key, value)
        {
            changed += 1;
        }
    }
    if changed == 0 {
        log.skip("材质包启用列表：与模板一致，无需修改".to_string());
        return Ok(());
    }
    if let Some(backup) = backup_file_safely(&target_path)? {
        log.info(format!("已备份到 {}", backup.display()));
    }
    journal.write_file(&target_path, target.to_text())?;
    log.success("材质包启用列表：已按模板更新".to_string());
    result.changed_paths.push(target_path);
    Ok(())
}

/// 同步 Axiom 蓝图并把新放入的蓝图追加到排序文件。
///
/// 复制模式只放入数据版本号不超过目标实例的蓝图；共享模式要求两个实例的数据版本号相同，
/// 因为共享目录中的蓝图对所有链接的实例可见，旧版本实例加载新版本蓝图会崩溃。
fn sync_axiom_blueprints(
    request: &MigrationRequest,
    method: &PackTransfer,
    resolve: &mut ConflictResolver,
    journal: &mut Journal,
    log: &mut LogSink,
    result: &mut MigrationResult,
) -> Result<()> {
    let template_dir = &request.template.game_dir;
    let target_dir = &request.target.game_dir;
    let installed = detect_installed_mod_ids(&target_dir.join("mods"), log)?;
    if !installed.contains(AXIOM_MOD_ID) {
        log.skip("Axiom 蓝图：目标实例未安装 Axiom，跳过".to_string());
        return Ok(());
    }
    let target_version = read_data_version(target_dir)?;
    let max_data_version = match method {
        PackTransfer::CopyMissing => Some(target_version.ok_or_else(|| {
            anyhow!("目标实例的 options.txt 缺少数据版本号，无法判断蓝图能否加载，请先启动一次游戏")
        })?),
        PackTransfer::LinkToShared(_) => {
            let template_version = read_data_version(template_dir)?;
            if template_version.is_none() || template_version != target_version {
                bail!(
                    "共享蓝图要求两个实例的游戏数据版本相同（模板 {}，目标 {}），请改用复制模式",
                    describe_data_version(template_version),
                    describe_data_version(target_version)
                );
            }
            None
        }
    };

    let blueprint_dir = pack_sync::asset_dir(SharedAsset::AxiomBlueprints);
    let template_order = read_blueprint_ordering(&template_dir.join(blueprint_dir))?;
    let outcome = pack_sync::sync_shared_assets(
        &AssetSyncRequest {
            kind: SharedAsset::AxiomBlueprints,
            template: &request.template,
            target: &request.target,
            method,
            max_data_version,
        },
        resolve,
        journal,
        log,
    )?;
    merge_result(result, outcome.result);
    for (dir, names) in outcome.added {
        append_blueprint_ordering(&dir, &names, &template_order, journal, log, result)?;
    }
    Ok(())
}

fn describe_data_version(version: Option<i64>) -> String {
    version.map_or_else(|| "未知".to_string(), |v| v.to_string())
}

/// 读取实例 `options.txt` 记录的游戏数据版本号，文件或条目不存在时返回 `None`。
fn read_data_version(game_dir: &Path) -> Result<Option<i64>> {
    let path = game_dir.join(OPTIONS_FILE_NAME);
    if !path.is_file() {
        return Ok(None);
    }
    Ok(OptionsFile::load(&path)?
        .get(VERSION_KEY)
        .and_then(|value| value.trim().parse().ok()))
}

fn read_blueprint_ordering(dir: &Path) -> Result<Vec<String>> {
    let path = dir.join(BLUEPRINT_ORDERING_FILE);
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let text =
        fs::read_to_string(&path).with_context(|| format!("读取 {} 失败", path.display()))?;
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

/// 把 `names` 中排序文件尚未记录的蓝图追加到末尾，顺序跟随模板的排序文件。
fn append_blueprint_ordering(
    dir: &Path,
    names: &[String],
    template_order: &[String],
    journal: &mut Journal,
    log: &mut LogSink,
    result: &mut MigrationResult,
) -> Result<()> {
    let mut ordering = read_blueprint_ordering(dir)?;
    let mut missing: Vec<&String> = names
        .iter()
        .filter(|name| !ordering.contains(name))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    missing.sort_by_key(|name| {
        (
            template_order
                .iter()
                .position(|ordered| ordered == *name)
                .unwrap_or(usize::MAX),
            (*name).clone(),
        )
    });
    ordering.extend(missing.into_iter().cloned());

    let path = dir.join(BLUEPRINT_ORDERING_FILE);
    if let Some(backup) = backup_file_safely(&path)? {
        log.info(format!("已备份到 {}", backup.display()));
    }
    let text: String = ordering.iter().map(|name| format!("{name}\n")).collect();
    journal.write_file(&path, text)?;
    log.success(format!("Axiom 蓝图：已更新排序文件 {}", path.display()));
    result.changed_paths.push(path);
    Ok(())
}

/// 读取 `mods_dir` 下所有 `.jar` 的元数据并返回 Mod ID 集合。
pub fn detect_installed_mod_ids(mods_dir: &Path, log: &mut LogSink) -> Result<HashSet<String>> {
    Ok(scan_installed_mods(mods_dir, log)?.into_keys().collect())
}

/// 读取 `mods_dir` 下所有 `.jar` 的元数据，返回按 ID 排序的 Mod 与版本号。
///
/// 损坏的压缩包与不规范的元数据只记录日志并跳过，不会中断扫描。
pub fn scan_installed_mods(mods_dir: &Path, log: &mut LogSink) -> Result<BTreeMap<String, String>> {
    let mut mods = BTreeMap::new();
    if !mods_dir.is_dir() {
        return Ok(mods);
    }
    for entry in
        fs::read_dir(mods_dir).with_context(|| format!("读取目录 {} 失败", mods_dir.display()))?
    {
        let path = match entry {
            Ok(entry) => entry.path(),
            Err(err) => {
                log.warn(format!("跳过无法读取的目录项：{err}"));
                continue;
            }
        };
        let is_jar = path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("jar"));
        if !is_jar || !path.is_file() {
            continue;
        }
        match read_mod_ids_from_jar(&path) {
            Ok(metadata) if metadata.mods.is_empty() && metadata.problems.is_empty() => {
                log.skip(format!("未在 {} 中找到 Mod 元数据", path.display()));
            }
            Ok(metadata) if metadata.mods.is_empty() => log.warn(format!(
                "跳过无法解析的 Mod 文件 {}：{}",
                path.display(),
                metadata.problems.join("；")
            )),
            Ok(metadata) => {
                if !metadata.problems.is_empty() {
                    let ids: Vec<&str> = metadata.mods.iter().map(|m| m.id.as_str()).collect();
                    log.skip(format!(
                        "{}：已识别 {}，忽略无法解析的元数据（{}）",
                        path.display(),
                        ids.join("、"),
                        metadata.problems.join("；")
                    ));
                }
                for found in metadata.mods {
                    let version = found.version.unwrap_or_else(|| UNKNOWN_VERSION.to_string());
                    mods.entry(found.id).or_insert(version);
                }
            }
            Err(err) => log.warn(format!(
                "跳过无法解析的 Mod 文件 {}：{err:#}",
                path.display()
            )),
        }
    }
    Ok(mods)
}

const UNKNOWN_VERSION: &str = "未知";

/// 从一份元数据中读到的 Mod。
#[derive(Debug, Clone, PartialEq, Eq)]
struct ModMetadata {
    id: String,
    /// 缺失或仍是 `${...}` 占位符时为 `None`。
    version: Option<String>,
}

impl ModMetadata {
    fn new(id: &str, version: Option<&str>) -> Self {
        Self {
            id: id.to_string(),
            version: version
                .filter(|v| !v.is_empty() && !v.contains('$'))
                .map(str::to_string),
        }
    }
}

/// 单个 jar 的元数据读取结果。
#[derive(Debug, Default)]
struct JarMetadata {
    mods: Vec<ModMetadata>,
    /// 无法读取或解析的元数据文件及原因。
    problems: Vec<String>,
}

type MetadataParser = fn(&str) -> Result<Vec<ModMetadata>>;

/// 各加载器的元数据文件及其解析函数。
const METADATA_PARSERS: &[(&str, MetadataParser)] = &[
    ("fabric.mod.json", parse_fabric_mod_ids),
    ("quilt.mod.json", parse_quilt_mod_ids),
    ("META-INF/mods.toml", parse_forge_mod_ids),
    ("META-INF/neoforge.mods.toml", parse_forge_mod_ids),
];

/// 逐个解析 jar 中的元数据文件；一份损坏只记为问题，不影响从其他元数据读到的 ID。
///
/// 同一个 jar 常同时带有多个加载器的元数据，作者只维护实际使用的那份，其余可能残留未替换的占位符。
fn read_mod_ids_from_jar(path: &Path) -> Result<JarMetadata> {
    let file = File::open(path).context("打开文件失败")?;
    let mut archive = ZipArchive::new(file).context("不是有效的 jar 压缩包")?;
    let mut metadata = JarMetadata::default();
    for (name, parse) in METADATA_PARSERS {
        let parsed = read_zip_text(&mut archive, name)
            .and_then(|text| text.map(|text| parse(&text)).transpose());
        match parsed {
            Ok(Some(mods)) => {
                for found in mods {
                    if !metadata.mods.iter().any(|m| m.id == found.id) {
                        metadata.mods.push(found);
                    }
                }
            }
            Ok(None) => {}
            Err(err) => metadata.problems.push(format!("{name} 格式错误：{err:#}")),
        }
    }
    // Forge 的版本号常写成 `${file.jarVersion}`，实际值由构建工具写在 jar 清单中
    if metadata.mods.iter().any(|m| m.version.is_none())
        && let Ok(Some(manifest)) = read_zip_text(&mut archive, "META-INF/MANIFEST.MF")
        && let Some(version) = manifest_implementation_version(&manifest)
    {
        for found in metadata.mods.iter_mut().filter(|m| m.version.is_none()) {
            found.version = Some(version.clone());
        }
    }
    Ok(metadata)
}

fn manifest_implementation_version(manifest: &str) -> Option<String> {
    manifest
        .lines()
        .find_map(|line| line.strip_prefix("Implementation-Version:"))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty() && !value.contains('$'))
}

fn parse_fabric_mod_ids(text: &str) -> Result<Vec<ModMetadata>> {
    let json = parse_mod_metadata_json(text)?;
    Ok(json
        .get("id")
        .and_then(Value::as_str)
        .map(|id| ModMetadata::new(id, json.get("version").and_then(Value::as_str)))
        .into_iter()
        .collect())
}

fn parse_quilt_mod_ids(text: &str) -> Result<Vec<ModMetadata>> {
    let json = parse_mod_metadata_json(text)?;
    Ok(json
        .pointer("/quilt_loader/id")
        .and_then(Value::as_str)
        .map(|id| {
            ModMetadata::new(
                id,
                json.pointer("/quilt_loader/version")
                    .and_then(Value::as_str),
            )
        })
        .into_iter()
        .collect())
}

/// Fabric 加载器能读取字符串中直接换行的元数据，严格的 JSON 解析会拒绝；
/// 先把字符串内的控制字符转义，再按标准 JSON 解析。
fn parse_mod_metadata_json(text: &str) -> Result<Value> {
    parse_json_text(&escape_control_chars_in_strings(text))
}

fn escape_control_chars_in_strings(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    let (mut in_string, mut after_backslash) = (false, false);
    for ch in text.chars() {
        if in_string && !after_backslash && ch < ' ' {
            match ch {
                '\n' => escaped.push_str("\\n"),
                '\r' => escaped.push_str("\\r"),
                '\t' => escaped.push_str("\\t"),
                other => escaped.push_str(&format!("\\u{:04x}", u32::from(other))),
            }
            continue;
        }
        escaped.push(ch);
        if in_string && after_backslash {
            after_backslash = false;
        } else if in_string && ch == '\\' {
            after_backslash = true;
        } else if ch == '"' {
            in_string = !in_string;
        }
    }
    escaped
}

fn read_zip_text<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    name: &str,
) -> Result<Option<String>> {
    let mut entry = match archive.by_name(name) {
        Ok(entry) => entry,
        Err(ZipError::FileNotFound) => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("读取 {name} 失败")),
    };
    let mut text = String::new();
    entry
        .read_to_string(&mut text)
        .with_context(|| format!("读取 {name} 失败"))?;
    Ok(Some(text))
}

fn parse_json_text(text: &str) -> Result<Value> {
    Ok(serde_json::from_str(text.trim_start_matches('\u{feff}'))?)
}

/// 解析 `mods.toml`；整份文件因残留占位符等原因无法解析时，退回逐行查找 `[[mods]]` 下的 `modId`。
fn parse_forge_mod_ids(text: &str) -> Result<Vec<ModMetadata>> {
    let document: DocumentMut = match text.parse() {
        Ok(document) => document,
        Err(err) => {
            let ids = scan_forge_mod_ids(text);
            if ids.is_empty() {
                return Err(err.into());
            }
            return Ok(ids);
        }
    };
    let Some(mods) = document
        .get("mods")
        .and_then(|item| item.as_array_of_tables())
    else {
        return Ok(Vec::new());
    };
    // 未经构建替换的 `${mod_id}` 占位符不是真实 ID
    Ok(mods
        .iter()
        .filter_map(|table| {
            let id = table.get("modId").and_then(|item| item.as_str())?;
            let version = table.get("version").and_then(|item| item.as_str());
            (!id.contains('$')).then(|| ModMetadata::new(id, version))
        })
        .collect())
}

fn scan_forge_mod_ids(text: &str) -> Vec<ModMetadata> {
    let mut mods = Vec::new();
    let mut current: Option<(Option<String>, Option<String>)> = None;
    let mut finish = |entry: Option<(Option<String>, Option<String>)>| {
        if let Some((Some(id), version)) = entry
            && !id.contains('$')
        {
            mods.push(ModMetadata::new(&id, version.as_deref()));
        }
    };
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            finish(current.take());
            current = (line == "[[mods]]").then_some((None, None));
            continue;
        }
        let Some((id, version)) = &mut current else {
            continue;
        };
        if let Some(value) = toml_line_value(line, "modId") {
            *id = Some(value);
        } else if let Some(value) = toml_line_value(line, "version") {
            *version = Some(value);
        }
    }
    finish(current.take());
    mods
}

/// 读取形如 `key = "value"` 的单行字符串值。
fn toml_line_value(line: &str, key: &str) -> Option<String> {
    let value = line
        .strip_prefix(key)?
        .trim_start()
        .strip_prefix('=')?
        .trim()
        .trim_matches(['"', '\'']);
    (!value.is_empty()).then(|| value.to_string())
}

fn sync_mod_configs(
    template_dir: &Path,
    target_dir: &Path,
    mode: SyncMode,
    journal: &mut Journal,
    log: &mut LogSink,
    result: &mut MigrationResult,
) -> Result<()> {
    let installed = detect_installed_mod_ids(&target_dir.join("mods"), log)?;
    log.info(format!("目标实例识别到 {} 个 Mod", installed.len()));

    for rule in MOD_CONFIG_RULES {
        if !installed.contains(rule.mod_id) {
            continue;
        }
        let source = template_dir.join(rule.relative_path);
        if !source.is_file() {
            log.skip(format!(
                "{}：模板实例没有 {}，跳过",
                rule.mod_id, rule.relative_path
            ));
            continue;
        }
        let destination = target_dir.join(rule.relative_path);
        match apply_mod_config_rule(rule, &source, &destination, mode, journal) {
            Ok(true) => {
                log.success(format!("{}：已更新 {}", rule.mod_id, rule.relative_path));
                result.changed_paths.push(destination);
            }
            Ok(false) => log.skip(format!("{}：与模板一致，无需修改", rule.mod_id)),
            Err(err) => {
                let message = format!("{}：同步 {} 失败：{err:#}", rule.mod_id, rule.relative_path);
                log.warn(message.clone());
                result.warnings.push(message);
            }
        }
    }
    Ok(())
}

/// 按规则写入一个 Mod 配置文件，返回是否发生了写入。
fn apply_mod_config_rule(
    rule: &ModConfigRule,
    source: &Path,
    destination: &Path,
    mode: SyncMode,
    journal: &mut Journal,
) -> Result<bool> {
    let template_text =
        fs::read_to_string(source).with_context(|| format!("读取 {} 失败", source.display()))?;
    let new_text = if mode == SyncMode::Merge && destination.is_file() {
        let template = parse_json_text(&template_text)
            .with_context(|| format!("{} 不是有效的 JSON", source.display()))?;
        let current_text = fs::read_to_string(destination)
            .with_context(|| format!("读取 {} 失败", destination.display()))?;
        let current = parse_json_text(&current_text)
            .with_context(|| format!("{} 不是有效的 JSON", destination.display()))?;
        let mut merged = current.clone();
        let fields = merged
            .as_object_mut()
            .ok_or_else(|| anyhow!("{} 顶层不是 JSON 对象", destination.display()))?;
        for field in rule.merge_fields {
            if let Some(value) = template.get(*field) {
                fields.insert((*field).to_string(), value.clone());
            }
        }
        if merged == current {
            return Ok(false);
        }
        serde_json::to_string_pretty(&merged)?
    } else {
        if destination.is_file() && fs::read_to_string(destination)? == template_text {
            return Ok(false);
        }
        template_text
    };

    backup_file_safely(destination)?;
    journal.write_file(destination, new_text)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use zip::write::SimpleFileOptions;

    use super::*;
    use crate::test_support::{TempDir, test_journal};

    #[test]
    fn merge_overwrites_and_appends_key_bindings_only() {
        let template = OptionsFile::parse(
            "version:4903\nfov:0.5\nkey_key.jump:key.keyboard.space\nkey_key.axiom.tool:key.keyboard.r\n",
        );
        let mut target = OptionsFile::parse(
            "version:3955\r\nfov:0.0\r\nkey_key.jump:key.keyboard.j\r\nkey_key.only.target:key.keyboard.x\r\n",
        );

        let changed = apply_options_mapping(
            &template,
            &mut target,
            SyncMode::Merge,
            &mut LogSink::new(&mut |_| {}),
        );

        assert_eq!(changed, 2);
        assert_eq!(
            target.to_text(),
            "version:3955\r\nfov:0.0\r\nkey_key.jump:key.keyboard.space\r\n\
             key_key.only.target:key.keyboard.x\r\nkey_key.axiom.tool:key.keyboard.r\r\n"
        );
    }

    #[test]
    fn full_overwrite_keeps_blacklisted_entries() {
        let template = OptionsFile::parse(
            "version:4903\nrenderDistance:32\noverrideWidth:2560\nsoundDevice:\"USB\"\nkey_key.jump:key.keyboard.space\n",
        );
        let mut target = OptionsFile::parse(
            "version:4900\nrenderDistance:8\noverrideWidth:0\nsoundDevice:\"\"\n",
        );

        apply_options_mapping(
            &template,
            &mut target,
            SyncMode::FullOverwrite,
            &mut LogSink::new(&mut |_| {}),
        );

        assert_eq!(target.get("renderDistance"), Some("32"));
        assert_eq!(target.get("key_key.jump"), Some("key.keyboard.space"));
        assert_eq!(target.get("version"), Some("4900"));
        assert_eq!(target.get("overrideWidth"), Some("0"));
        assert_eq!(target.get("soundDevice"), Some("\"\""));
    }

    #[test]
    fn key_bindings_are_skipped_across_the_1_13_boundary() {
        let template = OptionsFile::parse("version:1343\nkey_key.jump:57\nrenderDistance:12\n");
        let mut target =
            OptionsFile::parse("version:4903\nkey_key.jump:key.keyboard.space\nrenderDistance:8\n");

        let changed = apply_options_mapping(
            &template,
            &mut target,
            SyncMode::FullOverwrite,
            &mut LogSink::new(&mut |_| {}),
        );

        assert_eq!(changed, 1);
        assert_eq!(target.get("key_key.jump"), Some("key.keyboard.space"));
        assert_eq!(target.get("renderDistance"), Some("12"));
    }

    #[test]
    fn backup_keeps_original_and_rotates_timestamped_copies() -> Result<()> {
        let dir = TempDir::new("backup")?;
        let file = dir.path().join("options.txt");
        fs::write(&file, "original")?;

        let first = backup_file_safely(&file)?;
        assert_eq!(first, Some(dir.path().join("options.txt.bak")));

        for seconds in 1..=6u64 {
            fs::write(dir.path().join(format!("options.txt.{seconds}.bak")), "old")?;
        }
        fs::write(&file, "changed")?;
        let stamped = backup_file_safely(&file)?.ok_or_else(|| anyhow!("未生成备份"))?;

        assert_eq!(
            fs::read_to_string(dir.path().join("options.txt.bak"))?,
            "original"
        );
        assert_eq!(fs::read_to_string(&stamped)?, "changed");
        let remaining = fs::read_dir(dir.path())?
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                name.starts_with("options.txt.") && name != "options.txt.bak"
            })
            .count();
        assert_eq!(remaining, MAX_TIMESTAMPED_BACKUPS);
        assert!(!dir.path().join("options.txt.1.bak").exists());
        Ok(())
    }

    #[test]
    fn sync_options_backs_up_before_writing() -> Result<()> {
        let (_history, mut journal) = test_journal()?;
        let template = TempDir::new("opt-template")?;
        let target = TempDir::new("opt-target")?;
        fs::write(
            template.path().join("options.txt"),
            "key_key.jump:key.keyboard.space\n",
        )?;
        fs::write(
            target.path().join("options.txt"),
            "key_key.jump:key.keyboard.j\n",
        )?;

        let mut result = MigrationResult::default();
        sync_options_file(
            template.path(),
            target.path(),
            SyncMode::Merge,
            &mut journal,
            &mut LogSink::new(&mut |_| {}),
            &mut result,
        )?;

        assert_eq!(
            fs::read_to_string(target.path().join("options.txt.bak"))?,
            "key_key.jump:key.keyboard.j\n"
        );
        assert_eq!(
            fs::read_to_string(target.path().join("options.txt"))?,
            "key_key.jump:key.keyboard.space\n"
        );
        assert_eq!(result.changed_paths.len(), 1);
        Ok(())
    }

    #[test]
    fn sync_options_skips_target_without_options_file() -> Result<()> {
        let (_history, mut journal) = test_journal()?;
        let template = TempDir::new("opt-template-missing")?;
        let target = TempDir::new("opt-target-missing")?;
        fs::write(
            template.path().join("options.txt"),
            "key_key.jump:key.keyboard.space\n",
        )?;

        let mut result = MigrationResult::default();
        sync_options_file(
            template.path(),
            target.path(),
            SyncMode::Merge,
            &mut journal,
            &mut LogSink::new(&mut |_| {}),
            &mut result,
        )?;

        assert!(!target.path().join("options.txt").exists());
        assert_eq!(result.warnings.len(), 1);
        Ok(())
    }

    fn write_jar(path: &Path, entries: &[(&str, &str)]) -> Result<()> {
        let mut writer = zip::ZipWriter::new(File::create(path)?);
        for (name, content) in entries {
            writer.start_file(*name, SimpleFileOptions::default())?;
            writer.write_all(content.as_bytes())?;
        }
        writer.finish()?;
        Ok(())
    }

    #[test]
    fn detects_mod_ids_and_skips_broken_jars() -> Result<()> {
        let dir = TempDir::new("mods")?;
        write_jar(
            &dir.path().join("axiom.jar"),
            &[("fabric.mod.json", r#"{"id":"axiom"}"#)],
        )?;
        write_jar(
            &dir.path().join("neo.jar"),
            &[(
                "META-INF/neoforge.mods.toml",
                "modLoader=\"javafml\"\n[[mods]]\nmodId=\"create\"\n[[mods]]\nmodId=\"${mod_id}\"\n",
            )],
        )?;
        write_jar(
            &dir.path().join("bad-meta.jar"),
            &[("fabric.mod.json", "{not json")],
        )?;
        fs::write(dir.path().join("broken.jar"), "not a zip")?;
        write_jar(
            &dir.path().join("off.jar.disabled"),
            &[("fabric.mod.json", r#"{"id":"off"}"#)],
        )?;

        let mut messages = Vec::new();
        let ids = detect_installed_mod_ids(
            dir.path(),
            &mut LogSink::new(&mut |entry| messages.push(entry)),
        )?;

        assert_eq!(
            ids,
            HashSet::from(["axiom".to_string(), "create".to_string()])
        );
        assert_eq!(messages.len(), 2);
        Ok(())
    }

    #[test]
    fn tolerates_lenient_metadata_and_keeps_ids_from_valid_files() -> Result<()> {
        let dir = TempDir::new("mods-lenient")?;
        write_jar(
            &dir.path().join("multiline.jar"),
            &[(
                "fabric.mod.json",
                "{\"id\": \"emf\", \"description\": \"first line\n\tsecond line\"}",
            )],
        )?;
        write_jar(
            &dir.path().join("universal.jar"),
            &[
                ("fabric.mod.json", r#"{"id":"mixintrace"}"#),
                (
                    "META-INF/mods.toml",
                    "[[mods]]\nmodId = \"${mod_id}\"\n[[dependencies.${mod_id}]]\nmodId = \"forge\"\n",
                ),
            ],
        )?;
        write_jar(
            &dir.path().join("forge-placeholder.jar"),
            &[(
                "META-INF/mods.toml",
                "[[mods]]\nmodId = \"realmod\"\n[[dependencies.${mod_id}]]\nmodId = \"forge\"\n",
            )],
        )?;

        let mut messages = Vec::new();
        let ids = detect_installed_mod_ids(
            dir.path(),
            &mut LogSink::new(&mut |entry| messages.push(entry)),
        )?;

        assert_eq!(
            ids,
            HashSet::from(["emf", "mixintrace", "realmod"].map(String::from))
        );
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].level, crate::types::LogLevel::Skip);
        Ok(())
    }

    fn fabric_jar(dir: &Path, id: &str, version: &str) -> Result<()> {
        let mods = dir.join("mods");
        fs::create_dir_all(&mods)?;
        write_jar(
            &mods.join(format!("{id}-{version}.jar")),
            &[(
                "fabric.mod.json",
                &format!(r#"{{"id":"{id}","version":"{version}"}}"#),
            )],
        )
    }

    #[test]
    fn diff_lists_version_changes_additions_and_removals() -> Result<()> {
        let template = TempDir::new("diff-template")?;
        let target = TempDir::new("diff-target")?;
        for (id, version) in [("same", "1.0"), ("upgraded", "1.0"), ("dropped", "2.1")] {
            fabric_jar(template.path(), id, version)?;
        }
        for (id, version) in [("same", "1.0"), ("upgraded", "1.1"), ("fresh", "0.3")] {
            fabric_jar(target.path(), id, version)?;
        }
        let forge_dir = target.path().join("mods");
        write_jar(
            &forge_dir.join("forge.jar"),
            &[
                (
                    "META-INF/mods.toml",
                    "[[mods]]\nmodId = \"forgemod\"\nversion = \"${file.jarVersion}\"\n",
                ),
                (
                    "META-INF/MANIFEST.MF",
                    "Manifest-Version: 1.0\r\nImplementation-Version: 4.5.6\r\n",
                ),
            ],
        )?;

        let diff = diff_installed_mods(
            template.path(),
            target.path(),
            &mut LogSink::new(&mut |_| {}),
        )?;

        let entry = |id: &str, version: &str| ModEntry {
            id: id.to_string(),
            version: version.to_string(),
        };
        assert_eq!(
            diff.changed,
            vec![ModVersionChange {
                id: "upgraded".to_string(),
                from: "1.0".to_string(),
                to: "1.1".to_string(),
            }]
        );
        assert_eq!(
            diff.added,
            vec![entry("forgemod", "4.5.6"), entry("fresh", "0.3")]
        );
        assert_eq!(diff.removed, vec![entry("dropped", "2.1")]);
        Ok(())
    }

    #[test]
    fn axiom_merge_replaces_only_keybinds() -> Result<()> {
        let (_history, mut journal) = test_journal()?;
        let template = TempDir::new("axiom-template")?;
        let target = TempDir::new("axiom-target")?;
        let rule = &MOD_CONFIG_RULES[0];
        let source = template.path().join(rule.relative_path);
        let destination = target.path().join(rule.relative_path);
        fs::create_dir_all(source.parent().ok_or_else(|| anyhow!("无父目录"))?)?;
        fs::create_dir_all(destination.parent().ok_or_else(|| anyhow!("无父目录"))?)?;
        fs::write(
            &source,
            r#"{"keybinds":{"undo":"ctrl+z"},"visuals":{"theme":"dark"}}"#,
        )?;
        fs::write(
            &destination,
            r#"{"keybinds":{"undo":"none"},"visuals":{"theme":"light"}}"#,
        )?;

        assert!(apply_mod_config_rule(
            rule,
            &source,
            &destination,
            SyncMode::Merge,
            &mut journal
        )?);
        let merged: Value = serde_json::from_str(&fs::read_to_string(&destination)?)?;
        assert_eq!(
            merged.pointer("/keybinds/undo"),
            Some(&Value::from("ctrl+z"))
        );
        assert_eq!(
            merged.pointer("/visuals/theme"),
            Some(&Value::from("light"))
        );
        assert!(destination.with_file_name("axiom.json.bak").is_file());

        assert!(!apply_mod_config_rule(
            rule,
            &source,
            &destination,
            SyncMode::Merge,
            &mut journal
        )?);
        Ok(())
    }

    #[test]
    fn blueprint_ordering_appends_new_names_in_template_order() -> Result<()> {
        let (_history, mut journal) = test_journal()?;
        let dir = TempDir::new("bp-ordering")?;
        fs::write(dir.path().join(BLUEPRINT_ORDERING_FILE), "mine.bp\n")?;
        let names = ["b.bp", "a.bp", "mine.bp", "z.bp"].map(String::from);
        let template_order = ["a.bp", "b.bp"].map(String::from);

        let mut result = MigrationResult::default();
        append_blueprint_ordering(
            dir.path(),
            &names,
            &template_order,
            &mut journal,
            &mut LogSink::new(&mut |_| {}),
            &mut result,
        )?;

        assert_eq!(
            fs::read_to_string(dir.path().join(BLUEPRINT_ORDERING_FILE))?,
            "mine.bp\na.bp\nb.bp\nz.bp\n"
        );
        assert!(dir.path().join(".axiom_blueprint_ordering.bak").is_file());
        Ok(())
    }

    #[test]
    fn enabled_pack_list_follows_template() -> Result<()> {
        let (_history, mut journal) = test_journal()?;
        let template = TempDir::new("packlist-template")?;
        let target = TempDir::new("packlist-target")?;
        fs::write(
            template.path().join("options.txt"),
            "resourcePacks:[\"vanilla\",\"file/a.zip\"]\nincompatibleResourcePacks:[]\nfov:1.0\n",
        )?;
        fs::write(
            target.path().join("options.txt"),
            "fov:0.0\nresourcePacks:[\"vanilla\"]\n",
        )?;

        let mut result = MigrationResult::default();
        sync_enabled_pack_list(
            template.path(),
            target.path(),
            &mut journal,
            &mut LogSink::new(&mut |_| {}),
            &mut result,
        )?;

        let updated = OptionsFile::load(&target.path().join("options.txt"))?;
        assert_eq!(
            updated.get("resourcePacks"),
            Some("[\"vanilla\",\"file/a.zip\"]")
        );
        assert_eq!(updated.get("incompatibleResourcePacks"), Some("[]"));
        assert_eq!(updated.get("fov"), Some("0.0"));
        assert!(target.path().join("options.txt.bak").is_file());
        Ok(())
    }
}
