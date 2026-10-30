use std::fs::{self, OpenOptions};
use std::io::ErrorKind;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use walkdir::WalkDir;

use crate::types::{CacheTransfer, MigrationResult, ServerCache};

/// 以世界键（单人存档名或 `Multiplayer_<服务器地址>`）为子目录的地图与路点根目录。
const MAP_ROOTS: &[&str] = &[
    "xaero/minimap",
    "xaero/world-map",
    "XaeroWaypoints",
    "XaeroWorldMap",
];
const ERROR_SHARING_VIOLATION: i32 = 32;
const ERROR_LOCK_VIOLATION: i32 = 33;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 把模板实例各世界键下的地图与路点文件同步到目标实例。
///
/// 目标缺少的文件直接复制，同名文件仅在模板修改时间更新时覆盖，目标独有的文件保持不变。
/// 地图文件数量大，按规则豁免逐文件 `.bak` 备份，改为执行前在日志中列出将被覆盖的文件。
pub fn sync_map_directories(
    template_dir: &Path,
    target_dir: &Path,
    log: &mut dyn FnMut(String),
) -> Result<MigrationResult> {
    let mut plan = Vec::new();
    for root in MAP_ROOTS {
        let source_root = template_dir.join(root);
        if !source_root.is_dir() {
            continue;
        }
        for entry in fs::read_dir(&source_root)
            .with_context(|| format!("读取目录 {} 失败", source_root.display()))?
        {
            let entry =
                entry.with_context(|| format!("读取目录 {} 失败", source_root.display()))?;
            if entry.path().is_dir() {
                collect_copy_plan(
                    &entry.path(),
                    &target_dir.join(root).join(entry.file_name()),
                    &mut plan,
                )?;
            }
        }
    }

    let mut result = MigrationResult::default();
    if plan.is_empty() {
        log("地图与路点：目标已是最新，无需同步".to_string());
        return Ok(result);
    }
    let overwrites: Vec<&CopyItem> = plan.iter().filter(|item| item.overwrites).collect();
    if !overwrites.is_empty() {
        log(format!(
            "地图与路点：以下 {} 个文件将被模板中更新的版本覆盖",
            overwrites.len()
        ));
        for item in &overwrites {
            log(format!("  {}", item.destination.display()));
        }
    }
    let overwrite_count = overwrites.len();

    for item in plan {
        if let Some(parent) = item.destination.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("创建目录 {} 失败", parent.display()))?;
        }
        fs::copy(&item.source, &item.destination).with_context(|| {
            format!(
                "复制 {} 到 {} 失败",
                item.source.display(),
                item.destination.display()
            )
        })?;
        result.changed_paths.push(item.destination);
    }
    log(format!(
        "地图与路点：复制 {} 个文件，其中覆盖 {} 个",
        result.changed_paths.len(),
        overwrite_count
    ));
    Ok(result)
}

struct CopyItem {
    source: PathBuf,
    destination: PathBuf,
    overwrites: bool,
}

fn collect_copy_plan(
    source_dir: &Path,
    destination_dir: &Path,
    plan: &mut Vec<CopyItem>,
) -> Result<()> {
    for entry in WalkDir::new(source_dir) {
        let entry = entry.with_context(|| format!("遍历 {} 失败", source_dir.display()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry.path().strip_prefix(source_dir)?;
        let destination = destination_dir.join(relative);
        let overwrites = destination.exists();
        if overwrites && modified_time(entry.path())? <= modified_time(&destination)? {
            continue;
        }
        plan.push(CopyItem {
            source: entry.path().to_path_buf(),
            destination,
            overwrites,
        });
    }
    Ok(())
}

fn modified_time(path: &Path) -> Result<SystemTime> {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .with_context(|| format!("读取 {} 的修改时间失败", path.display()))
}

/// 缓存在游戏目录中的相对位置，其下每个子目录对应一个世界或服务器地址。
fn cache_root(kind: ServerCache) -> &'static str {
    match kind {
        ServerCache::Voxy => ".voxy/saves",
        ServerCache::Bobby => ".bobby",
    }
}

/// 共享目录下按缓存种类分子目录，避免 Voxy 与 Bobby 的同名服务器目录互相冲突。
fn shared_subdir(kind: ServerCache) -> &'static str {
    match kind {
        ServerCache::Voxy => "voxy",
        ServerCache::Bobby => "bobby",
    }
}

/// 把模板实例中 `kind` 缓存的每个世界或服务器目录按 `method` 迁移到目标实例。
///
/// 单个目录失败时记录警告并继续处理其余目录。
pub fn transfer_server_caches(
    kind: ServerCache,
    template_dir: &Path,
    target_dir: &Path,
    method: &CacheTransfer,
    log: &mut dyn FnMut(String),
) -> Result<MigrationResult> {
    let mut result = MigrationResult::default();
    let source_root = template_dir.join(cache_root(kind));
    if !source_root.is_dir() {
        log(format!("{kind}：模板实例没有缓存目录，跳过"));
        return Ok(result);
    }
    let target_root = target_dir.join(cache_root(kind));
    fs::create_dir_all(&target_root)
        .with_context(|| format!("创建目录 {} 失败", target_root.display()))?;

    for entry in fs::read_dir(&source_root)
        .with_context(|| format!("读取目录 {} 失败", source_root.display()))?
    {
        let entry = entry.with_context(|| format!("读取目录 {} 失败", source_root.display()))?;
        let name = entry.file_name();
        // 跳过本程序此前重命名留下的备份目录
        if !entry.path().is_dir() || name.to_string_lossy().ends_with(".bak") {
            continue;
        }
        let target = target_root.join(&name);
        match transfer_one_cache(kind, &entry.path(), &target, method, log) {
            Ok(true) => result.changed_paths.push(target),
            Ok(false) => {}
            Err(err) => {
                let message = format!("{kind}：迁移 {} 失败：{err:#}", name.to_string_lossy());
                log(message.clone());
                result.warnings.push(message);
            }
        }
    }
    Ok(result)
}

/// 迁移单个世界或服务器的缓存，目标已与源共用同一份缓存时返回 `false`。
fn transfer_one_cache(
    kind: ServerCache,
    source: &Path,
    target: &Path,
    method: &CacheTransfer,
    log: &mut dyn FnMut(String),
) -> Result<bool> {
    let real_source = resolve_real_path(source)?;
    if target.exists() && resolve_real_path(target)? == real_source {
        log(format!("{kind}：{} 已与源共用缓存，跳过", target.display()));
        return Ok(false);
    }

    let link_destination = match method {
        CacheTransfer::Copy => {
            ensure_cache_not_in_use(&real_source)?;
            None
        }
        CacheTransfer::LinkToSource => Some(real_source.clone()),
        CacheTransfer::LinkToShared(shared_root) => Some(prepare_shared_cache(
            kind,
            source,
            &real_source,
            shared_root,
            log,
        )?),
    };

    if target.exists() {
        ensure_cache_not_in_use(target)?;
        let backup = rename_aside(target)?;
        log(format!(
            "{kind}：已将目标原有缓存重命名为 {}",
            backup.display()
        ));
    }

    match link_destination {
        Some(destination) => {
            create_junction(target, &destination)?;
            log(format!(
                "{kind}：{} → {}",
                target.display(),
                destination.display()
            ));
        }
        None => {
            copy_dir_recursive(&real_source, target)?;
            log(format!("{kind}：已复制 {}", target.display()));
        }
    }
    Ok(true)
}

/// 确保源缓存位于共享目录并由源实例链接过去，返回共享目录中的缓存路径。
fn prepare_shared_cache(
    kind: ServerCache,
    source: &Path,
    real_source: &Path,
    shared_root: &Path,
    log: &mut dyn FnMut(String),
) -> Result<PathBuf> {
    let name = source
        .file_name()
        .ok_or_else(|| anyhow!("无法取得目录名：{}", source.display()))?;
    let shared_root = shared_root.join(shared_subdir(kind));
    let shared = shared_root.join(name);
    if shared.exists() && resolve_real_path(&shared)? == real_source {
        return Ok(shared);
    }
    // 源缓存若已链接到别处，移动其真实目录会让其他实例的链接失效
    if is_link(source)? {
        bail!(
            "源缓存是指向 {} 的链接，无法移动到共享目录，请改用“Junction 指向源实例”",
            real_source.display()
        );
    }
    if shared.exists() {
        bail!("共享目录中已存在 {}，为避免覆盖已中止", shared.display());
    }

    ensure_cache_not_in_use(real_source)?;
    fs::create_dir_all(&shared_root)
        .with_context(|| format!("创建目录 {} 失败", shared_root.display()))?;
    move_dir(real_source, &shared)?;
    // 源实例的链接建立失败时把缓存移回原处，避免源实例丢失缓存
    if let Err(err) = create_junction(source, &shared) {
        move_dir(&shared, real_source).with_context(|| {
            format!(
                "建立链接失败且无法移回缓存，缓存现位于 {}",
                shared.display()
            )
        })?;
        return Err(err);
    }
    log(format!(
        "{kind}：已将源缓存移动到 {} 并在源实例建立链接",
        shared.display()
    ));
    Ok(shared)
}

/// 解析链接后的真实路径，并去掉 `canonicalize` 附加的 `\\?\` 前缀，使其可直接传给 `mklink`。
pub(crate) fn resolve_real_path(path: &Path) -> Result<PathBuf> {
    let canonical =
        fs::canonicalize(path).with_context(|| format!("解析路径 {} 失败", path.display()))?;
    let text = canonical.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(stripped) if !stripped.starts_with("UNC\\") => Ok(PathBuf::from(stripped)),
        _ => Ok(canonical),
    }
}

pub(crate) fn is_link(path: &Path) -> Result<bool> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("读取 {} 的属性失败", path.display()))?;
    Ok(metadata.file_type().is_symlink())
}

/// 逐个以不共享模式打开缓存中的文件，任何文件仍被其他进程打开时都会失败。
///
/// 游戏运行时会一直打开正在读写的缓存文件（Voxy 的 RocksDB、Bobby 的区块文件），
/// 据此判断对应实例是否正在运行。
pub(crate) fn ensure_cache_not_in_use(cache_dir: &Path) -> Result<()> {
    for entry in WalkDir::new(cache_dir) {
        let entry = entry.with_context(|| format!("遍历 {} 失败", cache_dir.display()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let opened = OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(entry.path());
        match opened {
            Ok(_) => {}
            Err(err)
                if matches!(
                    err.raw_os_error(),
                    Some(ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION)
                ) =>
            {
                bail!("{} 正在被占用，请先关闭对应的游戏实例", cache_dir.display());
            }
            Err(err) => {
                return Err(err).with_context(|| format!("检查 {} 失败", entry.path().display()));
            }
        }
    }
    Ok(())
}

pub(crate) fn rename_aside(path: &Path) -> Result<PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("无法取得目录名：{}", path.display()))?
        .to_string_lossy()
        .into_owned();
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("系统时间早于 1970 年")?
        .as_secs();
    let backup = path.with_file_name(format!("{name}.{seconds}.bak"));
    if backup.exists() {
        bail!("备份目录 {} 已存在", backup.display());
    }
    fs::rename(path, &backup).with_context(|| format!("重命名 {} 失败", path.display()))?;
    Ok(backup)
}

pub(crate) fn move_dir(from: &Path, to: &Path) -> Result<()> {
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        // 共享目录位于其他磁盘时无法直接重命名，改为复制后删除源目录
        Err(err) if err.kind() == ErrorKind::CrossesDevices => {
            copy_dir_recursive(from, to)?;
            fs::remove_dir_all(from).with_context(|| format!("删除 {} 失败", from.display()))
        }
        Err(err) => {
            Err(err).with_context(|| format!("移动 {} 到 {} 失败", from.display(), to.display()))
        }
    }
}

pub(crate) fn copy_dir_recursive(source: &Path, destination: &Path) -> Result<()> {
    for entry in WalkDir::new(source) {
        let entry = entry.with_context(|| format!("遍历 {} 失败", source.display()))?;
        let target = destination.join(entry.path().strip_prefix(source)?);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target)
                .with_context(|| format!("创建目录 {} 失败", target.display()))?;
        } else {
            fs::copy(entry.path(), &target).with_context(|| {
                format!(
                    "复制 {} 到 {} 失败",
                    entry.path().display(),
                    target.display()
                )
            })?;
        }
    }
    Ok(())
}

/// 通过 `mklink /J` 创建目录 Junction，无需管理员权限或开发者模式。
pub(crate) fn create_junction(link: &Path, destination: &Path) -> Result<()> {
    // mklink 不接受 `/` 分隔符，而 Path::join 拼接相对路径时会保留它
    let to_native = |path: &Path| path.to_string_lossy().replace('/', "\\");
    let output = Command::new("cmd")
        .arg("/C")
        .arg("mklink")
        .arg("/J")
        .arg(to_native(link))
        .arg(to_native(destination))
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .context("无法执行 mklink")?;
    if !output.status.success() {
        bail!(
            "创建 Junction {} 失败：{}",
            link.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs::File;

    use super::*;
    use crate::test_support::TempDir;

    fn no_log() -> impl FnMut(String) {
        |_| {}
    }

    fn set_modified(path: &Path, time: SystemTime) -> Result<()> {
        File::options().write(true).open(path)?.set_modified(time)?;
        Ok(())
    }

    #[test]
    fn map_sync_copies_missing_and_newer_files_only() -> Result<()> {
        let template = TempDir::new("map-template")?;
        let target = TempDir::new("map-target")?;
        let source_world = template
            .path()
            .join("xaero/minimap/Multiplayer_example.com");
        let target_world = target.path().join("xaero/minimap/Multiplayer_example.com");
        fs::create_dir_all(source_world.join("dim%0"))?;
        fs::create_dir_all(target_world.join("dim%0"))?;

        let base = SystemTime::now() - std::time::Duration::from_secs(3600);
        let later = base + std::time::Duration::from_secs(60);
        fs::write(source_world.join("dim%0/waypoints.txt"), "template-new")?;
        fs::write(target_world.join("dim%0/waypoints.txt"), "target-old")?;
        set_modified(&source_world.join("dim%0/waypoints.txt"), later)?;
        set_modified(&target_world.join("dim%0/waypoints.txt"), base)?;
        fs::write(source_world.join("config.txt"), "template-old")?;
        fs::write(target_world.join("config.txt"), "target-new")?;
        set_modified(&source_world.join("config.txt"), base)?;
        set_modified(&target_world.join("config.txt"), later)?;
        fs::write(source_world.join("extra.txt"), "added")?;
        fs::write(target_world.join("mine.txt"), "kept")?;

        let result = sync_map_directories(template.path(), target.path(), &mut no_log())?;

        assert_eq!(result.changed_paths.len(), 2);
        assert_eq!(
            fs::read_to_string(target_world.join("dim%0/waypoints.txt"))?,
            "template-new"
        );
        assert_eq!(
            fs::read_to_string(target_world.join("config.txt"))?,
            "target-new"
        );
        assert_eq!(fs::read_to_string(target_world.join("extra.txt"))?, "added");
        assert_eq!(fs::read_to_string(target_world.join("mine.txt"))?, "kept");
        Ok(())
    }

    fn create_voxy_world(game_dir: &Path, world: &str, content: &str) -> Result<PathBuf> {
        let dir = game_dir.join(cache_root(ServerCache::Voxy)).join(world);
        fs::create_dir_all(dir.join("hash/storage"))?;
        fs::write(dir.join("config.json"), content)?;
        fs::write(dir.join("hash/storage/LOCK"), "")?;
        Ok(dir)
    }

    fn create_bobby_server(game_dir: &Path, server: &str) -> Result<PathBuf> {
        let dir = game_dir.join(cache_root(ServerCache::Bobby)).join(server);
        let dimension = dir.join("123/minecraft/overworld");
        fs::create_dir_all(&dimension)?;
        fs::write(dimension.join("r.0.0.mca"), "region")?;
        fs::write(dimension.join("last_access"), "")?;
        Ok(dir)
    }

    fn voxy_dir(game_dir: &Path, world: &str) -> PathBuf {
        game_dir.join(cache_root(ServerCache::Voxy)).join(world)
    }

    #[test]
    fn voxy_copy_backs_up_existing_target_cache() -> Result<()> {
        let template = TempDir::new("voxy-copy-template")?;
        let target = TempDir::new("voxy-copy-target")?;
        create_voxy_world(template.path(), "example.com", "template")?;
        let target_world = create_voxy_world(target.path(), "example.com", "target")?;

        let result = transfer_server_caches(
            ServerCache::Voxy,
            template.path(),
            target.path(),
            &CacheTransfer::Copy,
            &mut no_log(),
        )?;

        assert_eq!(result.changed_paths, vec![target_world.clone()]);
        assert_eq!(
            fs::read_to_string(target_world.join("config.json"))?,
            "template"
        );
        assert!(!is_link(&target_world)?);
        let backups = fs::read_dir(target.path().join(cache_root(ServerCache::Voxy)))?
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".bak"))
            .count();
        assert_eq!(backups, 1);
        Ok(())
    }

    #[test]
    fn voxy_link_to_source_replaces_existing_target_with_junction() -> Result<()> {
        let template = TempDir::new("voxy-link-template")?;
        let target = TempDir::new("voxy-link-target")?;
        let source_world = create_voxy_world(template.path(), "example.com", "template")?;
        let target_world = create_voxy_world(target.path(), "example.com", "target")?;

        let result = transfer_server_caches(
            ServerCache::Voxy,
            template.path(),
            target.path(),
            &CacheTransfer::LinkToSource,
            &mut no_log(),
        )?;

        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        assert!(is_link(&target_world)?);
        assert!(!is_link(&source_world)?);
        assert_eq!(
            resolve_real_path(&target_world)?,
            resolve_real_path(&source_world)?
        );
        Ok(())
    }

    #[test]
    fn voxy_link_to_shared_moves_source_and_links_both() -> Result<()> {
        let template = TempDir::new("voxy-shared-template")?;
        let target = TempDir::new("voxy-shared-target")?;
        let next_target = TempDir::new("voxy-shared-next")?;
        let shared = TempDir::new("voxy-shared-store")?;
        let source_world = create_voxy_world(template.path(), "example.com", "template")?;
        let method = CacheTransfer::LinkToShared(shared.path().to_path_buf());
        let transfer = |target_dir: &Path| {
            transfer_server_caches(
                ServerCache::Voxy,
                template.path(),
                target_dir,
                &method,
                &mut no_log(),
            )
        };

        let first = transfer(target.path())?;
        assert!(first.warnings.is_empty(), "{:?}", first.warnings);
        let shared_world = resolve_real_path(&shared.path().join("voxy/example.com"))?;
        let target_world = voxy_dir(target.path(), "example.com");

        assert!(is_link(&source_world)?);
        assert!(is_link(&target_world)?);
        assert_eq!(resolve_real_path(&source_world)?, shared_world);
        assert_eq!(resolve_real_path(&target_world)?, shared_world);
        assert_eq!(
            fs::read_to_string(target_world.join("config.json"))?,
            "template"
        );

        transfer(next_target.path())?;
        let next_world = voxy_dir(next_target.path(), "example.com");
        assert_eq!(resolve_real_path(&next_world)?, shared_world);

        let repeated = transfer(target.path())?;
        assert!(repeated.changed_paths.is_empty());
        Ok(())
    }

    #[test]
    fn voxy_and_bobby_share_one_root_without_conflict() -> Result<()> {
        let template = TempDir::new("both-shared-template")?;
        let target = TempDir::new("both-shared-target")?;
        let shared = TempDir::new("both-shared-store")?;
        create_voxy_world(template.path(), "example.com", "template")?;
        create_bobby_server(template.path(), "example.com")?;
        let method = CacheTransfer::LinkToShared(shared.path().to_path_buf());

        for kind in [ServerCache::Voxy, ServerCache::Bobby] {
            let result = transfer_server_caches(
                kind,
                template.path(),
                target.path(),
                &method,
                &mut no_log(),
            )?;
            assert!(result.warnings.is_empty(), "{kind}: {:?}", result.warnings);
        }

        let bobby_server = target
            .path()
            .join(cache_root(ServerCache::Bobby))
            .join("example.com");
        assert_eq!(
            resolve_real_path(&bobby_server)?,
            resolve_real_path(&shared.path().join("bobby/example.com"))?
        );
        assert!(
            bobby_server
                .join("123/minecraft/overworld/r.0.0.mca")
                .is_file()
        );
        assert!(shared.path().join("voxy/example.com/config.json").is_file());
        Ok(())
    }

    #[test]
    fn voxy_copy_refuses_cache_held_by_running_game() -> Result<()> {
        let template = TempDir::new("voxy-lock-template")?;
        let target = TempDir::new("voxy-lock-target")?;
        let source_world = create_voxy_world(template.path(), "example.com", "template")?;
        let _held = OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0)
            .open(source_world.join("hash/storage/LOCK"))?;

        let result = transfer_server_caches(
            ServerCache::Voxy,
            template.path(),
            target.path(),
            &CacheTransfer::Copy,
            &mut no_log(),
        )?;

        assert!(result.changed_paths.is_empty());
        assert_eq!(result.warnings.len(), 1);
        assert!(!voxy_dir(target.path(), "example.com").exists());
        Ok(())
    }

    #[test]
    fn bobby_copy_refuses_region_file_opened_with_shared_access() -> Result<()> {
        let template = TempDir::new("bobby-lock-template")?;
        let target = TempDir::new("bobby-lock-target")?;
        let server = create_bobby_server(template.path(), "example.com")?;
        // 默认共享模式与游戏打开区块文件的方式一致，允许其他进程同时读写
        let _held = OpenOptions::new()
            .read(true)
            .write(true)
            .open(server.join("123/minecraft/overworld/r.0.0.mca"))?;

        let result = transfer_server_caches(
            ServerCache::Bobby,
            template.path(),
            target.path(),
            &CacheTransfer::Copy,
            &mut no_log(),
        )?;

        assert!(result.changed_paths.is_empty());
        assert_eq!(result.warnings.len(), 1);
        Ok(())
    }
}
