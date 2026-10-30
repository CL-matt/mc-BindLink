use std::fs::{self, File};
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use walkdir::WalkDir;

use crate::map_sync::{
    copy_dir_recursive, create_junction, ensure_cache_not_in_use, is_link, move_dir, rename_aside,
    resolve_real_path,
};
use crate::types::{
    ConflictChoice, DetectedInstance, MigrationResult, PackConflict, PackTransfer, PackVersion,
};

const RESOURCE_PACKS_DIR: &str = "resourcepacks";
/// 被替换下来的材质包放在材质包目录之外，避免游戏把备份当作可选材质包列出。
const PACK_BACKUP_DIR: &str = "resourcepacks.bak";
const SHARED_OWNER: &str = "共享目录";

/// 把一批同名冲突交给用户决定，返回与输入一一对应的选择。
pub type ConflictResolver<'a> = dyn FnMut(Vec<PackConflict>) -> Result<Vec<ConflictChoice>> + 'a;

/// 按 `method` 同步模板与目标实例的材质包，同名且内容不同的材质包由 `resolve` 交给用户决定。
pub fn sync_resource_packs(
    template: &DetectedInstance,
    target: &DetectedInstance,
    method: &PackTransfer,
    resolve: &mut ConflictResolver,
    log: &mut dyn FnMut(String),
) -> Result<MigrationResult> {
    match method {
        PackTransfer::CopyMissing => copy_missing_packs(template, target, resolve, log),
        PackTransfer::LinkToShared(shared_root) => {
            link_packs_to_shared(template, target, shared_root, resolve, log)
        }
    }
}

fn copy_missing_packs(
    template: &DetectedInstance,
    target: &DetectedInstance,
    resolve: &mut ConflictResolver,
    log: &mut dyn FnMut(String),
) -> Result<MigrationResult> {
    let mut result = MigrationResult::default();
    let source_dir = template.game_dir.join(RESOURCE_PACKS_DIR);
    if !source_dir.is_dir() {
        log("材质包：模板实例没有 resourcepacks 目录，跳过".to_string());
        return Ok(result);
    }
    let target_dir = target.game_dir.join(RESOURCE_PACKS_DIR);
    fs::create_dir_all(&target_dir)
        .with_context(|| format!("创建目录 {} 失败", target_dir.display()))?;

    let mut missing = Vec::new();
    let mut conflicts = Vec::new();
    for (name, source) in list_packs(&source_dir)? {
        let destination = target_dir.join(&name);
        if !destination.exists() {
            missing.push((source, destination));
        } else if !have_same_content(&source, &destination)? {
            conflicts.push(PlannedConflict {
                name,
                incoming: source,
                existing: destination,
            });
        }
    }
    let choices = ask_user(&conflicts, &target.name, &template.name, resolve)?;

    for (source, destination) in missing {
        copy_pack(&source, &destination)?;
        result.changed_paths.push(destination);
    }
    for (conflict, choice) in conflicts.into_iter().zip(choices) {
        if choice == ConflictChoice::KeepExisting {
            continue;
        }
        ensure_cache_not_in_use(&conflict.existing)?;
        let backup = move_to_backup(&conflict.existing, &target.game_dir)?;
        copy_pack(&conflict.incoming, &conflict.existing)?;
        log(format!(
            "材质包：{} 已替换为模板版本，原版本移至 {}",
            conflict.name,
            backup.display()
        ));
        result.changed_paths.push(conflict.existing);
    }
    log(format!(
        "材质包：复制或替换 {} 个",
        result.changed_paths.len()
    ));
    Ok(result)
}

fn link_packs_to_shared(
    template: &DetectedInstance,
    target: &DetectedInstance,
    shared_root: &Path,
    resolve: &mut ConflictResolver,
    log: &mut dyn FnMut(String),
) -> Result<MigrationResult> {
    let mut result = MigrationResult::default();
    let shared = shared_root.join(RESOURCE_PACKS_DIR);
    // 先处理模板，使共享目录第一次创建时以模板的材质包为准
    for instance in [template, target] {
        if link_instance_packs(instance, &shared, shared_root, resolve, log)? {
            result
                .changed_paths
                .push(instance.game_dir.join(RESOURCE_PACKS_DIR));
        }
    }
    Ok(result)
}

/// 把实例的材质包并入共享目录后建立链接，实例已链接到共享目录时返回 `false`。
fn link_instance_packs(
    instance: &DetectedInstance,
    shared: &Path,
    shared_root: &Path,
    resolve: &mut ConflictResolver,
    log: &mut dyn FnMut(String),
) -> Result<bool> {
    let packs_dir = instance.game_dir.join(RESOURCE_PACKS_DIR);
    if packs_dir.exists()
        && shared.exists()
        && resolve_real_path(&packs_dir)? == resolve_real_path(shared)?
    {
        log(format!("材质包：{} 已链接到共享目录，跳过", instance.name));
        return Ok(false);
    }

    if packs_dir.exists() {
        // 实例的材质包目录若已链接到别处，合并会改动其他实例正在使用的目录
        if is_link(&packs_dir)? {
            bail!(
                "{} 的 resourcepacks 是指向 {} 的链接，无法合并到共享目录",
                instance.name,
                resolve_real_path(&packs_dir)?.display()
            );
        }
        ensure_cache_not_in_use(&packs_dir)?;
        if shared.exists() {
            merge_into_shared(instance, &packs_dir, shared, shared_root, resolve, log)?;
            set_aside_leftovers(&packs_dir, log)?;
        } else {
            fs::create_dir_all(shared_root)
                .with_context(|| format!("创建目录 {} 失败", shared_root.display()))?;
            move_dir(&packs_dir, shared)?;
            log(format!(
                "材质包：已将 {} 的材质包移动到 {}",
                instance.name,
                shared.display()
            ));
        }
    } else {
        fs::create_dir_all(shared)
            .with_context(|| format!("创建目录 {} 失败", shared.display()))?;
    }

    create_junction(&packs_dir, shared).with_context(|| {
        format!(
            "材质包已位于 {}，但 {} 的链接建立失败",
            shared.display(),
            instance.name
        )
    })?;
    log(format!(
        "材质包：{} → {}",
        packs_dir.display(),
        shared.display()
    ));
    Ok(true)
}

fn merge_into_shared(
    instance: &DetectedInstance,
    packs_dir: &Path,
    shared: &Path,
    shared_root: &Path,
    resolve: &mut ConflictResolver,
    log: &mut dyn FnMut(String),
) -> Result<()> {
    let mut moves = Vec::new();
    let mut conflicts = Vec::new();
    for (name, path) in list_packs(packs_dir)? {
        let destination = shared.join(&name);
        if !destination.exists() {
            moves.push((path, destination));
        } else if !have_same_content(&path, &destination)? {
            conflicts.push(PlannedConflict {
                name,
                incoming: path,
                existing: destination,
            });
        }
    }
    let choices = ask_user(&conflicts, SHARED_OWNER, &instance.name, resolve)?;

    let moved = moves.len();
    for (path, destination) in moves {
        move_dir(&path, &destination)?;
    }
    for (conflict, choice) in conflicts.into_iter().zip(choices) {
        if choice == ConflictChoice::KeepExisting {
            continue;
        }
        ensure_cache_not_in_use(&conflict.existing)?;
        let backup = move_to_backup(&conflict.existing, shared_root)?;
        move_dir(&conflict.incoming, &conflict.existing)?;
        log(format!(
            "材质包：共享目录中的 {} 已替换为 {} 的版本，原版本移至 {}",
            conflict.name,
            instance.name,
            backup.display()
        ));
    }
    log(format!(
        "材质包：已将 {} 的 {moved} 个材质包并入共享目录",
        instance.name
    ));
    Ok(())
}

/// 合并后实例目录中剩下的是与共享目录相同或用户选择保留共享版本的材质包，整体改名保留。
fn set_aside_leftovers(packs_dir: &Path, log: &mut dyn FnMut(String)) -> Result<()> {
    let is_empty = fs::read_dir(packs_dir)
        .with_context(|| format!("读取目录 {} 失败", packs_dir.display()))?
        .next()
        .is_none();
    if is_empty {
        fs::remove_dir(packs_dir)
            .with_context(|| format!("删除空目录 {} 失败", packs_dir.display()))?;
    } else {
        let backup = rename_aside(packs_dir)?;
        log(format!(
            "材质包：未并入的原有材质包保留在 {}",
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
            pack_name: conflict.name.clone(),
            existing: describe_pack(&conflict.existing, existing_owner)?,
            incoming: describe_pack(&conflict.incoming, incoming_owner)?,
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

fn describe_pack(path: &Path, owner: &str) -> Result<PackVersion> {
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
    Ok(PackVersion {
        owner: owner.to_string(),
        size,
        modified: metadata.modified().ok(),
    })
}

fn list_packs(dir: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut packs = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("读取目录 {} 失败", dir.display()))?
    {
        let entry = entry.with_context(|| format!("读取目录 {} 失败", dir.display()))?;
        packs.push((
            entry.file_name().to_string_lossy().into_owned(),
            entry.path(),
        ));
    }
    packs.sort();
    Ok(packs)
}

fn copy_pack(source: &Path, destination: &Path) -> Result<()> {
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

fn move_to_backup(path: &Path, backup_parent: &Path) -> Result<PathBuf> {
    let backup_dir = backup_parent.join(PACK_BACKUP_DIR);
    fs::create_dir_all(&backup_dir)
        .with_context(|| format!("创建目录 {} 失败", backup_dir.display()))?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("系统时间早于 1970 年")?
        .as_secs();
    let backup = backup_dir.join(format!("{name}.{seconds}"));
    if backup.exists() {
        bail!("备份 {} 已存在", backup.display());
    }
    move_dir(path, &backup)?;
    Ok(backup)
}

/// 比较两个材质包（zip 文件或文件夹）的内容是否完全相同。
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
    use anyhow::anyhow;

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

    fn write_pack(game_dir: &Path, name: &str, content: &str) -> Result<PathBuf> {
        let dir = game_dir.join(RESOURCE_PACKS_DIR);
        fs::create_dir_all(&dir)?;
        let path = dir.join(name);
        fs::write(&path, content)?;
        Ok(path)
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
        let template_dir = TempDir::new("packs-copy-template")?;
        let target_dir = TempDir::new("packs-copy-target")?;
        write_pack(template_dir.path(), "new.zip", "new")?;
        write_pack(template_dir.path(), "same.zip", "same")?;
        write_pack(template_dir.path(), "changed.zip", "template-version")?;
        write_pack(target_dir.path(), "same.zip", "same")?;
        let target_changed = write_pack(target_dir.path(), "changed.zip", "target")?;
        let (template, target) = (
            instance(&template_dir, "旧包"),
            instance(&target_dir, "新包"),
        );

        let mut seen = Vec::new();
        let result = sync_resource_packs(
            &template,
            &target,
            &PackTransfer::CopyMissing,
            &mut answer_all(ConflictChoice::UseIncoming, &mut seen),
            &mut no_log(),
        )?;

        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].pack_name, "changed.zip");
        assert_eq!(seen[0].existing.owner, "新包");
        assert_eq!(seen[0].incoming.owner, "旧包");
        assert_eq!(seen[0].incoming.size, "template-version".len() as u64);
        assert_eq!(result.changed_paths.len(), 2);
        assert_eq!(fs::read_to_string(&target_changed)?, "template-version");
        let backups: Vec<_> = fs::read_dir(target_dir.path().join(PACK_BACKUP_DIR))?
            .filter_map(|entry| entry.ok())
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(fs::read_to_string(backups[0].path())?, "target");
        Ok(())
    }

    #[test]
    fn copy_missing_keeps_existing_when_user_chooses_so() -> Result<()> {
        let template_dir = TempDir::new("packs-keep-template")?;
        let target_dir = TempDir::new("packs-keep-target")?;
        write_pack(template_dir.path(), "changed.zip", "template")?;
        let target_changed = write_pack(target_dir.path(), "changed.zip", "target")?;

        let mut seen = Vec::new();
        sync_resource_packs(
            &instance(&template_dir, "旧包"),
            &instance(&target_dir, "新包"),
            &PackTransfer::CopyMissing,
            &mut answer_all(ConflictChoice::KeepExisting, &mut seen),
            &mut no_log(),
        )?;

        assert_eq!(fs::read_to_string(&target_changed)?, "target");
        assert!(!target_dir.path().join(PACK_BACKUP_DIR).exists());
        Ok(())
    }

    #[test]
    fn link_to_shared_merges_both_instances_and_links_them() -> Result<()> {
        let template_dir = TempDir::new("packs-shared-template")?;
        let target_dir = TempDir::new("packs-shared-target")?;
        let shared_root = TempDir::new("packs-shared-store")?;
        write_pack(template_dir.path(), "common.zip", "template")?;
        write_pack(template_dir.path(), "same.zip", "same")?;
        write_pack(target_dir.path(), "common.zip", "target")?;
        write_pack(target_dir.path(), "same.zip", "same")?;
        let folder_pack = target_dir.path().join(RESOURCE_PACKS_DIR).join("bundled");
        fs::create_dir_all(&folder_pack)?;
        fs::write(folder_pack.join("pack.mcmeta"), "{}")?;

        let mut seen = Vec::new();
        let result = sync_resource_packs(
            &instance(&template_dir, "旧包"),
            &instance(&target_dir, "新包"),
            &PackTransfer::LinkToShared(shared_root.path().to_path_buf()),
            &mut answer_all(ConflictChoice::KeepExisting, &mut seen),
            &mut no_log(),
        )?;

        assert_eq!(result.changed_paths.len(), 2);
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].existing.owner, SHARED_OWNER);
        assert_eq!(seen[0].incoming.owner, "新包");
        let shared = resolve_real_path(&shared_root.path().join(RESOURCE_PACKS_DIR))?;
        for game_dir in [template_dir.path(), target_dir.path()] {
            let packs_dir = game_dir.join(RESOURCE_PACKS_DIR);
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
}
