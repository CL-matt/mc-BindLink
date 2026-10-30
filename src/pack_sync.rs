use std::fs::{self, File};
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;
use walkdir::WalkDir;

use crate::map_sync::{
    copy_dir_recursive, create_junction, ensure_cache_not_in_use, is_link, move_dir, rename_aside,
    resolve_real_path,
};
use crate::types::{
    ConflictChoice, DetectedInstance, MigrationResult, PackConflict, PackTransfer, PackVersion,
    SharedAsset,
};

const SHARED_OWNER: &str = "共享目录";
const BLUEPRINT_EXTENSION: &str = "bp";
const BLUEPRINT_MAGIC: [u8; 4] = [0x0a, 0xe5, 0xbb, 0x36];
/// NBT 中 `DataVersion` 整数标签的类型字节、名称长度与名称。
const DATA_VERSION_TAG: &[u8] = b"\x03\x00\x0bDataVersion";

/// 把一批同名冲突交给用户决定，返回与输入一一对应的选择。
pub type ConflictResolver<'a> = dyn FnMut(Vec<PackConflict>) -> Result<Vec<ConflictChoice>> + 'a;

/// 资源同步结果，`added` 记录每个目录新放入的条目名称，供调用方更新 Axiom 蓝图排序文件。
#[derive(Debug, Default)]
pub struct AssetSyncOutcome {
    pub result: MigrationResult,
    pub added: Vec<(PathBuf, Vec<String>)>,
}

/// 资源在游戏目录中的相对位置。
pub fn asset_dir(kind: SharedAsset) -> &'static str {
    match kind {
        SharedAsset::ResourcePacks => "resourcepacks",
        SharedAsset::AxiomBlueprints => "config/axiom/blueprints",
    }
}

fn shared_subdir(kind: SharedAsset) -> &'static str {
    match kind {
        SharedAsset::ResourcePacks => "resourcepacks",
        SharedAsset::AxiomBlueprints => "axiom-blueprints",
    }
}

/// 材质包目录中的每个文件或文件夹都是一个材质包；蓝图目录只处理 `.bp` 文件，排序文件由调用方单独合并。
fn is_asset_entry(kind: SharedAsset, path: &Path) -> bool {
    match kind {
        SharedAsset::ResourcePacks => true,
        SharedAsset::AxiomBlueprints => {
            path.is_file()
                && path
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case(BLUEPRINT_EXTENSION))
        }
    }
}

/// 按 `method` 同步模板与目标实例的 `kind` 资源，同名且内容不同的条目由 `resolve` 交给用户决定。
///
/// `max_data_version` 不为空时，复制模式只放入数据版本号不超过该值的蓝图，避免目标版本加载新版本蓝图时崩溃。
pub fn sync_shared_assets(
    kind: SharedAsset,
    template: &DetectedInstance,
    target: &DetectedInstance,
    method: &PackTransfer,
    max_data_version: Option<i64>,
    resolve: &mut ConflictResolver,
    log: &mut dyn FnMut(String),
) -> Result<AssetSyncOutcome> {
    match method {
        PackTransfer::CopyMissing => {
            copy_missing_assets(kind, template, target, max_data_version, resolve, log)
        }
        PackTransfer::LinkToShared(shared_root) => {
            link_assets_to_shared(kind, template, target, shared_root, resolve, log)
        }
    }
}

fn copy_missing_assets(
    kind: SharedAsset,
    template: &DetectedInstance,
    target: &DetectedInstance,
    max_data_version: Option<i64>,
    resolve: &mut ConflictResolver,
    log: &mut dyn FnMut(String),
) -> Result<AssetSyncOutcome> {
    let mut outcome = AssetSyncOutcome::default();
    let source_dir = template.game_dir.join(asset_dir(kind));
    if !source_dir.is_dir() {
        log(format!("{kind}：模板实例没有 {}，跳过", asset_dir(kind)));
        return Ok(outcome);
    }
    let target_dir = target.game_dir.join(asset_dir(kind));
    fs::create_dir_all(&target_dir)
        .with_context(|| format!("创建目录 {} 失败", target_dir.display()))?;

    let mut missing = Vec::new();
    let mut conflicts = Vec::new();
    for (name, source) in list_assets(kind, &source_dir)? {
        let destination = target_dir.join(&name);
        let differs = destination.exists() && !have_same_content(&source, &destination)?;
        if destination.exists() && !differs {
            continue;
        }
        if let Some(max) = max_data_version
            && let Some(reason) = check_blueprint_version(&source, max)
        {
            log(format!("{kind}：跳过 {name}，{reason}"));
            continue;
        }
        if differs {
            conflicts.push(PlannedConflict {
                name,
                incoming: source,
                existing: destination,
            });
        } else {
            missing.push((name, source, destination));
        }
    }
    let choices = ask_user(kind, &conflicts, &target.name, &template.name, resolve)?;

    let mut added = Vec::new();
    for (name, source, destination) in missing {
        copy_entry(&source, &destination)?;
        outcome.result.changed_paths.push(destination);
        added.push(name);
    }
    let backup_dir = target_dir.with_file_name(format!("{}.bak", file_name_of(&target_dir)));
    for (conflict, choice) in conflicts.into_iter().zip(choices) {
        if choice == ConflictChoice::KeepExisting {
            continue;
        }
        ensure_cache_not_in_use(&conflict.existing)?;
        let backup = move_to_backup(&conflict.existing, &backup_dir)?;
        copy_entry(&conflict.incoming, &conflict.existing)?;
        log(format!(
            "{kind}：{} 已替换为模板版本，原版本移至 {}",
            conflict.name,
            backup.display()
        ));
        outcome.result.changed_paths.push(conflict.existing);
    }
    log(format!(
        "{kind}：复制或替换 {} 个",
        outcome.result.changed_paths.len()
    ));
    outcome.added.push((target_dir, added));
    Ok(outcome)
}

/// 蓝图比目标版本新或无法读取版本时返回跳过原因。
fn check_blueprint_version(path: &Path, max: i64) -> Option<String> {
    match read_blueprint_data_version(path) {
        Ok(version) if version <= max => None,
        Ok(version) => Some(format!(
            "数据版本 {version} 高于目标实例的 {max}，放入后游戏会无法加载"
        )),
        Err(err) => Some(format!("无法读取数据版本：{err:#}")),
    }
}

fn link_assets_to_shared(
    kind: SharedAsset,
    template: &DetectedInstance,
    target: &DetectedInstance,
    shared_root: &Path,
    resolve: &mut ConflictResolver,
    log: &mut dyn FnMut(String),
) -> Result<AssetSyncOutcome> {
    let mut outcome = AssetSyncOutcome::default();
    let shared = shared_root.join(shared_subdir(kind));
    // 先处理模板，使共享目录第一次创建时以模板的内容为准
    for instance in [template, target] {
        let linked = link_instance_assets(kind, instance, &shared, shared_root, resolve, log)?;
        if let Some(added) = linked {
            outcome
                .result
                .changed_paths
                .push(instance.game_dir.join(asset_dir(kind)));
            outcome.added.push((shared.clone(), added));
        }
    }
    Ok(outcome)
}

/// 把实例的资源并入共享目录后建立链接，返回新并入共享目录的条目名称；实例已链接到共享目录时返回 `None`。
fn link_instance_assets(
    kind: SharedAsset,
    instance: &DetectedInstance,
    shared: &Path,
    shared_root: &Path,
    resolve: &mut ConflictResolver,
    log: &mut dyn FnMut(String),
) -> Result<Option<Vec<String>>> {
    let instance_dir = instance.game_dir.join(asset_dir(kind));
    if instance_dir.exists()
        && shared.exists()
        && resolve_real_path(&instance_dir)? == resolve_real_path(shared)?
    {
        log(format!("{kind}：{} 已链接到共享目录，跳过", instance.name));
        return Ok(None);
    }

    let mut added = Vec::new();
    if instance_dir.exists() {
        // 实例目录若已链接到别处，合并会改动其他实例正在使用的目录
        if is_link(&instance_dir)? {
            bail!(
                "{} 的 {} 是指向 {} 的链接，无法合并到共享目录",
                instance.name,
                asset_dir(kind),
                resolve_real_path(&instance_dir)?.display()
            );
        }
        ensure_cache_not_in_use(&instance_dir)?;
        if shared.exists() {
            added = merge_into_shared(
                kind,
                instance,
                &instance_dir,
                shared,
                shared_root,
                resolve,
                log,
            )?;
            set_aside_leftovers(kind, &instance_dir, log)?;
        } else {
            fs::create_dir_all(shared_root)
                .with_context(|| format!("创建目录 {} 失败", shared_root.display()))?;
            move_dir(&instance_dir, shared)?;
            log(format!(
                "{kind}：已将 {} 的内容移动到 {}",
                instance.name,
                shared.display()
            ));
        }
    } else {
        fs::create_dir_all(shared)
            .with_context(|| format!("创建目录 {} 失败", shared.display()))?;
        if let Some(parent) = instance_dir.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("创建目录 {} 失败", parent.display()))?;
        }
    }

    create_junction(&instance_dir, shared).with_context(|| {
        format!(
            "{kind}已位于 {}，但 {} 的链接建立失败",
            shared.display(),
            instance.name
        )
    })?;
    log(format!(
        "{kind}：{} → {}",
        instance_dir.display(),
        shared.display()
    ));
    Ok(Some(added))
}

fn merge_into_shared(
    kind: SharedAsset,
    instance: &DetectedInstance,
    instance_dir: &Path,
    shared: &Path,
    shared_root: &Path,
    resolve: &mut ConflictResolver,
    log: &mut dyn FnMut(String),
) -> Result<Vec<String>> {
    let mut moves = Vec::new();
    let mut conflicts = Vec::new();
    for (name, path) in list_assets(kind, instance_dir)? {
        let destination = shared.join(&name);
        if !destination.exists() {
            moves.push((name, path, destination));
        } else if !have_same_content(&path, &destination)? {
            conflicts.push(PlannedConflict {
                name,
                incoming: path,
                existing: destination,
            });
        }
    }
    let choices = ask_user(kind, &conflicts, SHARED_OWNER, &instance.name, resolve)?;

    let mut added = Vec::new();
    for (name, path, destination) in moves {
        move_dir(&path, &destination)?;
        added.push(name);
    }
    let backup_dir = shared_root.join(format!("{}.bak", shared_subdir(kind)));
    for (conflict, choice) in conflicts.into_iter().zip(choices) {
        if choice == ConflictChoice::KeepExisting {
            continue;
        }
        ensure_cache_not_in_use(&conflict.existing)?;
        let backup = move_to_backup(&conflict.existing, &backup_dir)?;
        move_dir(&conflict.incoming, &conflict.existing)?;
        log(format!(
            "{kind}：共享目录中的 {} 已替换为 {} 的版本，原版本移至 {}",
            conflict.name,
            instance.name,
            backup.display()
        ));
    }
    log(format!(
        "{kind}：已将 {} 的 {} 个条目并入共享目录",
        instance.name,
        added.len()
    ));
    Ok(added)
}

/// 合并后实例目录中剩下的是与共享目录相同、用户选择保留共享版本或不属于资源条目的文件，整体改名保留。
fn set_aside_leftovers(
    kind: SharedAsset,
    instance_dir: &Path,
    log: &mut dyn FnMut(String),
) -> Result<()> {
    let is_empty = fs::read_dir(instance_dir)
        .with_context(|| format!("读取目录 {} 失败", instance_dir.display()))?
        .next()
        .is_none();
    if is_empty {
        fs::remove_dir(instance_dir)
            .with_context(|| format!("删除空目录 {} 失败", instance_dir.display()))?;
    } else {
        let backup = rename_aside(instance_dir)?;
        log(format!(
            "{kind}：未并入的原有文件保留在 {}",
            backup.display()
        ));
    }
    Ok(())
}

struct PlannedConflict {
    name: String,
    incoming: PathBuf,
    existing: PathBuf,
}

fn ask_user(
    kind: SharedAsset,
    conflicts: &[PlannedConflict],
    existing_owner: &str,
    incoming_owner: &str,
    resolve: &mut ConflictResolver,
) -> Result<Vec<ConflictChoice>> {
    if conflicts.is_empty() {
        return Ok(Vec::new());
    }
    let mut questions = Vec::new();
    for conflict in conflicts {
        questions.push(PackConflict {
            kind,
            pack_name: conflict.name.clone(),
            existing: describe_entry(kind, &conflict.existing, existing_owner)?,
            incoming: describe_entry(kind, &conflict.incoming, incoming_owner)?,
        });
    }
    let choices = resolve(questions)?;
    if choices.len() != conflicts.len() {
        bail!(
            "冲突选择数量不匹配：需要 {} 个，收到 {} 个",
            conflicts.len(),
            choices.len()
        );
    }
    Ok(choices)
}

fn describe_entry(kind: SharedAsset, path: &Path, owner: &str) -> Result<PackVersion> {
    let metadata =
        fs::metadata(path).with_context(|| format!("读取 {} 的属性失败", path.display()))?;
    let size = if metadata.is_dir() {
        let mut total = 0;
        for entry in WalkDir::new(path) {
            let entry = entry.with_context(|| format!("遍历 {} 失败", path.display()))?;
            if entry.file_type().is_file() {
                total += entry
                    .metadata()
                    .with_context(|| format!("读取 {} 的属性失败", entry.path().display()))?
                    .len();
            }
        }
        total
    } else {
        metadata.len()
    };
    // 版本号只用于帮助用户比较，读取失败时显示为未知
    let data_version = match kind {
        SharedAsset::ResourcePacks => None,
        SharedAsset::AxiomBlueprints => read_blueprint_data_version(path).ok(),
    };
    Ok(PackVersion {
        owner: owner.to_string(),
        size,
        modified: metadata.modified().ok(),
        data_version,
    })
}

/// 读取 Axiom 蓝图中方块数据记录的游戏数据版本号。
///
/// 蓝图文件依次为：4 字节魔数、长度前缀的头部 NBT、长度前缀的缩略图、长度前缀的 gzip 方块数据 NBT；
/// 头部的 `Version` 是 Axiom 自身的格式版本，游戏数据版本只记录在方块数据中。
pub fn read_blueprint_data_version(path: &Path) -> Result<i64> {
    let bytes = fs::read(path).with_context(|| format!("读取 {} 失败", path.display()))?;
    if !bytes.starts_with(&BLUEPRINT_MAGIC) {
        bail!("不是 Axiom 蓝图文件");
    }
    let mut offset = BLUEPRINT_MAGIC.len();
    for section in ["头部", "缩略图"] {
        let length = read_section_length(&bytes, offset, section)?;
        offset += 4 + length;
    }
    let length = read_section_length(&bytes, offset, "方块数据")?;
    let block_data = bytes
        .get(offset + 4..offset + 4 + length)
        .context("方块数据长度超出文件范围")?;
    let mut nbt = Vec::new();
    GzDecoder::new(block_data)
        .read_to_end(&mut nbt)
        .context("方块数据解压失败")?;
    let position = nbt
        .windows(DATA_VERSION_TAG.len())
        .position(|window| window == DATA_VERSION_TAG)
        .context("方块数据中没有 DataVersion")?;
    let value_start = position + DATA_VERSION_TAG.len();
    let value: [u8; 4] = nbt
        .get(value_start..value_start + 4)
        .and_then(|slice| slice.try_into().ok())
        .context("DataVersion 数据不完整")?;
    Ok(i64::from(i32::from_be_bytes(value)))
}

fn read_section_length(bytes: &[u8], offset: usize, section: &str) -> Result<usize> {
    let raw: [u8; 4] = bytes
        .get(offset..offset + 4)
        .and_then(|slice| slice.try_into().ok())
        .with_context(|| format!("{section}长度字段不完整"))?;
    Ok(u32::from_be_bytes(raw) as usize)
}

fn list_assets(kind: SharedAsset, dir: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("读取目录 {} 失败", dir.display()))?
    {
        let entry = entry.with_context(|| format!("读取目录 {} 失败", dir.display()))?;
        if is_asset_entry(kind, &entry.path()) {
            entries.push((
                entry.file_name().to_string_lossy().into_owned(),
                entry.path(),
            ));
        }
    }
    entries.sort();
    Ok(entries)
}

fn copy_entry(source: &Path, destination: &Path) -> Result<()> {
    if source.is_dir() {
        copy_dir_recursive(source, destination)
    } else {
        fs::copy(source, destination).map(|_| ()).with_context(|| {
            format!(
                "复制 {} 到 {} 失败",
                source.display(),
                destination.display()
            )
        })
    }
}

fn file_name_of(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn move_to_backup(path: &Path, backup_dir: &Path) -> Result<PathBuf> {
    fs::create_dir_all(backup_dir)
        .with_context(|| format!("创建目录 {} 失败", backup_dir.display()))?;
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("系统时间早于 1970 年")?
        .as_secs();
    let backup = backup_dir.join(format!("{}.{seconds}", file_name_of(path)));
    if backup.exists() {
        bail!("备份 {} 已存在", backup.display());
    }
    move_dir(path, &backup)?;
    Ok(backup)
}

/// 比较两个条目（文件或文件夹）的内容是否完全相同。
fn have_same_content(a: &Path, b: &Path) -> Result<bool> {
    let (meta_a, meta_b) = (
        fs::metadata(a).with_context(|| format!("读取 {} 的属性失败", a.display()))?,
        fs::metadata(b).with_context(|| format!("读取 {} 的属性失败", b.display()))?,
    );
    if meta_a.is_dir() != meta_b.is_dir() {
        return Ok(false);
    }
    if !meta_a.is_dir() {
        return have_same_file_content(a, b);
    }
    let files_a = list_relative_files(a)?;
    if files_a != list_relative_files(b)? {
        return Ok(false);
    }
    for relative in &files_a {
        if !have_same_file_content(&a.join(relative), &b.join(relative))? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn list_relative_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in WalkDir::new(dir) {
        let entry = entry.with_context(|| format!("遍历 {} 失败", dir.display()))?;
        if entry.file_type().is_file() {
            files.push(entry.path().strip_prefix(dir)?.to_path_buf());
        }
    }
    files.sort();
    Ok(files)
}

fn have_same_file_content(a: &Path, b: &Path) -> Result<bool> {
    let open = |path: &Path| {
        File::open(path)
            .map(BufReader::new)
            .with_context(|| format!("打开 {} 失败", path.display()))
    };
    if fs::metadata(a)?.len() != fs::metadata(b)?.len() {
        return Ok(false);
    }
    let (mut reader_a, mut reader_b) = (open(a)?, open(b)?);
    let (mut buffer_a, mut buffer_b) = (vec![0u8; 64 * 1024], vec![0u8; 64 * 1024]);
    loop {
        let read = reader_a.read(&mut buffer_a)?;
        if read == 0 {
            return Ok(true);
        }
        reader_b.read_exact(&mut buffer_b[..read])?;
        if buffer_a[..read] != buffer_b[..read] {
            return Ok(false);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use anyhow::anyhow;
    use flate2::Compression;
    use flate2::write::GzEncoder;

    use super::*;
    use crate::test_support::TempDir;
    use crate::types::LauncherKind;

    fn no_log() -> impl FnMut(String) {
        |_| {}
    }

    fn instance(dir: &TempDir, name: &str) -> DetectedInstance {
        DetectedInstance {
            name: name.to_string(),
            game_dir: dir.path().to_path_buf(),
            launcher: LauncherKind::Pcl2,
            is_isolated: true,
        }
    }

    fn write_asset(
        kind: SharedAsset,
        game_dir: &Path,
        name: &str,
        content: &[u8],
    ) -> Result<PathBuf> {
        let dir = game_dir.join(asset_dir(kind));
        fs::create_dir_all(&dir)?;
        let path = dir.join(name);
        fs::write(&path, content)?;
        Ok(path)
    }

    /// 按 Axiom 蓝图的分段格式构造一个只含 `DataVersion` 的最小蓝图。
    fn build_blueprint(data_version: i32, marker: &str) -> Result<Vec<u8>> {
        let mut nbt = vec![0x0a, 0x00, 0x00];
        nbt.extend_from_slice(DATA_VERSION_TAG);
        nbt.extend_from_slice(&data_version.to_be_bytes());
        nbt.extend_from_slice(marker.as_bytes());
        nbt.push(0x00);
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&nbt)?;
        let block_data = encoder.finish()?;

        let mut bytes = BLUEPRINT_MAGIC.to_vec();
        for section in [
            b"header".as_slice(),
            b"png".as_slice(),
            block_data.as_slice(),
        ] {
            bytes.extend_from_slice(&(section.len() as u32).to_be_bytes());
            bytes.extend_from_slice(section);
        }
        Ok(bytes)
    }

    /// 记录收到的冲突并对每个冲突返回同一选择。
    fn answer_all(
        choice: ConflictChoice,
        seen: &mut Vec<PackConflict>,
    ) -> impl FnMut(Vec<PackConflict>) -> Result<Vec<ConflictChoice>> + '_ {
        move |conflicts| {
            let answers = vec![choice; conflicts.len()];
            seen.extend(conflicts);
            Ok(answers)
        }
    }

    #[test]
    fn copy_missing_copies_new_packs_and_asks_only_for_different_ones() -> Result<()> {
        let kind = SharedAsset::ResourcePacks;
        let template_dir = TempDir::new("packs-copy-template")?;
        let target_dir = TempDir::new("packs-copy-target")?;
        write_asset(kind, template_dir.path(), "new.zip", b"new")?;
        write_asset(kind, template_dir.path(), "same.zip", b"same")?;
        write_asset(
            kind,
            template_dir.path(),
            "changed.zip",
            b"template-version",
        )?;
        write_asset(kind, target_dir.path(), "same.zip", b"same")?;
        let target_changed = write_asset(kind, target_dir.path(), "changed.zip", b"target")?;
        let (template, target) = (
            instance(&template_dir, "旧包"),
            instance(&target_dir, "新包"),
        );

        let mut seen = Vec::new();
        let outcome = sync_shared_assets(
            kind,
            &template,
            &target,
            &PackTransfer::CopyMissing,
            None,
            &mut answer_all(ConflictChoice::UseIncoming, &mut seen),
            &mut no_log(),
        )?;

        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].pack_name, "changed.zip");
        assert_eq!(seen[0].existing.owner, "新包");
        assert_eq!(seen[0].incoming.owner, "旧包");
        assert_eq!(seen[0].incoming.size, "template-version".len() as u64);
        assert_eq!(outcome.result.changed_paths.len(), 2);
        assert_eq!(fs::read_to_string(&target_changed)?, "template-version");
        let backups: Vec<_> = fs::read_dir(target_dir.path().join("resourcepacks.bak"))?
            .filter_map(|entry| entry.ok())
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(fs::read_to_string(backups[0].path())?, "target");
        Ok(())
    }

    #[test]
    fn copy_missing_keeps_existing_when_user_chooses_so() -> Result<()> {
        let kind = SharedAsset::ResourcePacks;
        let template_dir = TempDir::new("packs-keep-template")?;
        let target_dir = TempDir::new("packs-keep-target")?;
        write_asset(kind, template_dir.path(), "changed.zip", b"template")?;
        let target_changed = write_asset(kind, target_dir.path(), "changed.zip", b"target")?;

        let mut seen = Vec::new();
        sync_shared_assets(
            kind,
            &instance(&template_dir, "旧包"),
            &instance(&target_dir, "新包"),
            &PackTransfer::CopyMissing,
            None,
            &mut answer_all(ConflictChoice::KeepExisting, &mut seen),
            &mut no_log(),
        )?;

        assert_eq!(fs::read_to_string(&target_changed)?, "target");
        assert!(!target_dir.path().join("resourcepacks.bak").exists());
        Ok(())
    }

    #[test]
    fn link_to_shared_merges_both_instances_and_links_them() -> Result<()> {
        let kind = SharedAsset::ResourcePacks;
        let template_dir = TempDir::new("packs-shared-template")?;
        let target_dir = TempDir::new("packs-shared-target")?;
        let shared_root = TempDir::new("packs-shared-store")?;
        write_asset(kind, template_dir.path(), "common.zip", b"template")?;
        write_asset(kind, template_dir.path(), "same.zip", b"same")?;
        write_asset(kind, target_dir.path(), "common.zip", b"target")?;
        write_asset(kind, target_dir.path(), "same.zip", b"same")?;
        let folder_pack = target_dir.path().join(asset_dir(kind)).join("bundled");
        fs::create_dir_all(&folder_pack)?;
        fs::write(folder_pack.join("pack.mcmeta"), "{}")?;

        let mut seen = Vec::new();
        let outcome = sync_shared_assets(
            kind,
            &instance(&template_dir, "旧包"),
            &instance(&target_dir, "新包"),
            &PackTransfer::LinkToShared(shared_root.path().to_path_buf()),
            None,
            &mut answer_all(ConflictChoice::KeepExisting, &mut seen),
            &mut no_log(),
        )?;

        assert_eq!(outcome.result.changed_paths.len(), 2);
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].existing.owner, SHARED_OWNER);
        assert_eq!(seen[0].incoming.owner, "新包");
        let shared = resolve_real_path(&shared_root.path().join(shared_subdir(kind)))?;
        for game_dir in [template_dir.path(), target_dir.path()] {
            let packs_dir = game_dir.join(asset_dir(kind));
            assert!(is_link(&packs_dir)?);
            assert_eq!(resolve_real_path(&packs_dir)?, shared);
        }
        assert_eq!(fs::read_to_string(shared.join("common.zip"))?, "template");
        assert!(shared.join("bundled/pack.mcmeta").is_file());
        let leftovers = fs::read_dir(target_dir.path())?
            .filter_map(|entry| entry.ok())
            .find(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                name.starts_with("resourcepacks.") && name.ends_with(".bak")
            })
            .ok_or_else(|| anyhow!("未找到保留的原有材质包目录"))?;
        assert_eq!(
            fs::read_to_string(leftovers.path().join("common.zip"))?,
            "target"
        );
        Ok(())
    }

    #[test]
    fn folder_packs_compare_by_content() -> Result<()> {
        let a = TempDir::new("packs-compare-a")?;
        let b = TempDir::new("packs-compare-b")?;
        for dir in [a.path(), b.path()] {
            fs::create_dir_all(dir.join("assets"))?;
            fs::write(dir.join("pack.mcmeta"), "{}")?;
        }
        fs::write(a.path().join("assets/x.png"), "one")?;
        fs::write(b.path().join("assets/x.png"), "one")?;
        assert!(have_same_content(a.path(), b.path())?);

        fs::write(b.path().join("assets/x.png"), "two")?;
        assert!(!have_same_content(a.path(), b.path())?);
        Ok(())
    }

    #[test]
    fn reads_data_version_from_blueprint() -> Result<()> {
        let dir = TempDir::new("blueprint-version")?;
        let path = dir.path().join("tower.bp");
        fs::write(&path, build_blueprint(5023, "tower")?)?;
        assert_eq!(read_blueprint_data_version(&path)?, 5023);

        fs::write(&path, b"not a blueprint")?;
        assert!(read_blueprint_data_version(&path).is_err());
        Ok(())
    }

    #[test]
    fn blueprint_copy_skips_newer_versions_and_ignores_ordering_file() -> Result<()> {
        let kind = SharedAsset::AxiomBlueprints;
        let template_dir = TempDir::new("bp-copy-template")?;
        let target_dir = TempDir::new("bp-copy-target")?;
        write_asset(
            kind,
            template_dir.path(),
            "old.bp",
            &build_blueprint(4903, "old")?,
        )?;
        write_asset(
            kind,
            template_dir.path(),
            "new.bp",
            &build_blueprint(5023, "new")?,
        )?;
        write_asset(kind, template_dir.path(), "broken.bp", b"broken")?;
        write_asset(
            kind,
            template_dir.path(),
            ".axiom_blueprint_ordering",
            b"old.bp\n",
        )?;

        let mut seen = Vec::new();
        let outcome = sync_shared_assets(
            kind,
            &instance(&template_dir, "26.3"),
            &instance(&target_dir, "26.2"),
            &PackTransfer::CopyMissing,
            Some(4903),
            &mut answer_all(ConflictChoice::UseIncoming, &mut seen),
            &mut no_log(),
        )?;

        let target_blueprints = target_dir.path().join(asset_dir(kind));
        assert!(target_blueprints.join("old.bp").is_file());
        assert!(!target_blueprints.join("new.bp").exists());
        assert!(!target_blueprints.join("broken.bp").exists());
        assert!(!target_blueprints.join(".axiom_blueprint_ordering").exists());
        assert_eq!(
            outcome.added,
            vec![(target_blueprints, vec!["old.bp".to_string()])]
        );
        Ok(())
    }

    #[test]
    fn blueprint_conflict_reports_data_versions() -> Result<()> {
        let kind = SharedAsset::AxiomBlueprints;
        let template_dir = TempDir::new("bp-conflict-template")?;
        let target_dir = TempDir::new("bp-conflict-target")?;
        write_asset(
            kind,
            template_dir.path(),
            "tower.bp",
            &build_blueprint(4903, "a")?,
        )?;
        write_asset(
            kind,
            target_dir.path(),
            "tower.bp",
            &build_blueprint(4440, "b")?,
        )?;

        let mut seen = Vec::new();
        sync_shared_assets(
            kind,
            &instance(&template_dir, "旧包"),
            &instance(&target_dir, "新包"),
            &PackTransfer::CopyMissing,
            Some(4903),
            &mut answer_all(ConflictChoice::KeepExisting, &mut seen),
            &mut no_log(),
        )?;

        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].existing.data_version, Some(4440));
        assert_eq!(seen[0].incoming.data_version, Some(4903));
        Ok(())
    }
}
