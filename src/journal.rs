//! 迁移记录与回退。
//!
//! 迁移过程中所有写入、复制、移动与建立链接都经由 [`Journal`] 执行并登记，覆盖文件前另存迁移前版本。
//! 回退时按登记的逆序撤销；迁移后又被改动的文件或目录先改名为 `<原名>.<unix秒>.rollback` 保留，
//! 不删除用户在迁移后产生的内容。

use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::fs_ops::{
    copy_dir_recursive, create_junction, is_link, move_dir, rename_aside, resolve_real_path,
    unix_seconds,
};
use crate::types::{HistoryEntry, LogSink, MigrationResult};

const HISTORY_DIR: [&str; 2] = ["BindLink", "history"];
const MANIFEST_FILE: &str = "manifest.json";
const PREIMAGE_DIR: &str = "files";
const ROLLBACK_SUFFIX: &str = "rollback";
/// 地图同步可能产生上千条记录，每次登记都重写清单代价过高，按批写入。
const SAVE_EVERY: usize = 200;

/// 文件或目录在迁移结束时的状态，回退时据此判断迁移后是否又被改动。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Fingerprint {
    files: u64,
    bytes: u64,
    /// serde_json 不支持 u128，纳秒数用 u64 表示，可覆盖到 2554 年。
    latest_modified_nanos: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op")]
enum Operation {
    /// 写入或覆盖单个文件；`preimage` 为记录目录中迁移前版本的文件名，原本不存在时为空。
    FileWritten {
        path: PathBuf,
        preimage: Option<String>,
        after: Option<Fingerprint>,
    },
    /// 新建并填充的目录。
    DirCreated {
        path: PathBuf,
        after: Option<Fingerprint>,
    },
    Moved {
        from: PathBuf,
        to: PathBuf,
    },
    JunctionCreated {
        link: PathBuf,
    },
    /// 删除的 Junction 及其原先指向的目录，回退时重建链接。
    JunctionRemoved {
        link: PathBuf,
        destination: PathBuf,
    },
    EmptyDirRemoved {
        path: PathBuf,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    created_at: u64,
    template: String,
    target: String,
    operations: Vec<Operation>,
}

/// `%APPDATA%\BindLink\history`，所有迁移记录按时间放在其下的子目录中。
pub fn history_root() -> Result<PathBuf> {
    let app_data = env::var_os("APPDATA").context("找不到 APPDATA 环境变量")?;
    Ok(HISTORY_DIR
        .iter()
        .fold(PathBuf::from(app_data), |path, part| path.join(part)))
}

/// 列出尚未回退的迁移记录，最新的在前。
pub fn list_history(history_root: &Path) -> Result<Vec<HistoryEntry>> {
    let mut entries = Vec::new();
    if !history_root.is_dir() {
        return Ok(entries);
    }
    for entry in fs::read_dir(history_root)
        .with_context(|| format!("读取目录 {} 失败", history_root.display()))?
    {
        let dir = entry
            .with_context(|| format!("读取目录 {} 失败", history_root.display()))?
            .path();
        let manifest = load_manifest(&dir)?;
        entries.push(HistoryEntry {
            created_at: manifest.created_at,
            template: manifest.template,
            target: manifest.target,
            operation_count: manifest.operations.len(),
            dir,
        });
    }
    entries.sort_by(|a, b| b.dir.cmp(&a.dir));
    Ok(entries)
}

fn load_manifest(dir: &Path) -> Result<Manifest> {
    let path = dir.join(MANIFEST_FILE);
    let text =
        fs::read_to_string(&path).with_context(|| format!("读取 {} 失败", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("{} 格式错误", path.display()))
}

/// 一次迁移的操作记录，迁移代码通过它执行所有会改动文件系统的操作。
pub struct Journal {
    dir: PathBuf,
    manifest: Manifest,
    unsaved: usize,
}

impl Journal {
    pub fn begin(history_root: &Path, template: &str, target: &str) -> Result<Self> {
        let created_at = unix_seconds()?;
        // 同一秒内多次迁移时追加序号，目录名按字典序即可排出先后
        let mut sequence = 0;
        let dir = loop {
            let candidate = history_root.join(format!("{created_at:012}-{sequence:03}"));
            if !candidate.exists() {
                break candidate;
            }
            sequence += 1;
        };
        fs::create_dir_all(dir.join(PREIMAGE_DIR))
            .with_context(|| format!("创建迁移记录目录 {} 失败", dir.display()))?;
        let mut journal = Self {
            dir,
            manifest: Manifest {
                created_at,
                template: template.to_string(),
                target: target.to_string(),
                operations: Vec::new(),
            },
            unsaved: 0,
        };
        journal.save()?;
        Ok(journal)
    }

    fn record(&mut self, operation: Operation) -> Result<()> {
        self.manifest.operations.push(operation);
        self.unsaved += 1;
        if self.unsaved >= SAVE_EVERY {
            self.save()?;
        }
        Ok(())
    }

    fn save(&mut self) -> Result<()> {
        let path = self.dir.join(MANIFEST_FILE);
        let text = serde_json::to_string_pretty(&self.manifest)?;
        fs::write(&path, text).with_context(|| format!("写入 {} 失败", path.display()))?;
        self.unsaved = 0;
        Ok(())
    }

    /// 先另存 `path` 的迁移前版本并登记，再写入新内容。
    pub fn write_file(&mut self, path: &Path, contents: impl AsRef<[u8]>) -> Result<()> {
        self.record_file_change(path)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("创建目录 {} 失败", parent.display()))?;
        }
        fs::write(path, contents).with_context(|| format!("写入 {} 失败", path.display()))
    }

    pub fn copy_file(&mut self, source: &Path, destination: &Path) -> Result<()> {
        self.record_file_change(destination)?;
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("创建目录 {} 失败", parent.display()))?;
        }
        fs::copy(source, destination).with_context(|| {
            format!(
                "复制 {} 到 {} 失败",
                source.display(),
                destination.display()
            )
        })?;
        Ok(())
    }

    /// 复制整个目录到尚不存在的 `destination`。
    pub fn copy_dir(&mut self, source: &Path, destination: &Path) -> Result<()> {
        if destination.exists() {
            bail!("{} 已存在，无法复制", destination.display());
        }
        self.record(Operation::DirCreated {
            path: destination.to_path_buf(),
            after: None,
        })?;
        copy_dir_recursive(source, destination)
    }

    /// 复制单个文件或整个文件夹。
    pub fn copy_entry(&mut self, source: &Path, destination: &Path) -> Result<()> {
        if source.is_dir() {
            self.copy_dir(source, destination)
        } else {
            self.copy_file(source, destination)
        }
    }

    pub fn move_path(&mut self, from: &Path, to: &Path) -> Result<()> {
        move_dir(from, to)?;
        self.record(Operation::Moved {
            from: from.to_path_buf(),
            to: to.to_path_buf(),
        })
    }

    /// 改名为 `<原名>.<unix秒>.bak` 并登记，返回新路径。
    pub fn rename_aside(&mut self, path: &Path) -> Result<PathBuf> {
        let aside = rename_aside(path, "bak")?;
        self.record(Operation::Moved {
            from: path.to_path_buf(),
            to: aside.clone(),
        })?;
        Ok(aside)
    }

    pub fn create_junction(&mut self, link: &Path, destination: &Path) -> Result<()> {
        create_junction(link, destination)?;
        self.record(Operation::JunctionCreated {
            link: link.to_path_buf(),
        })
    }

    /// 只删除 Junction 本身并登记其指向，不影响其指向的目录。
    pub fn remove_junction(&mut self, link: &Path) -> Result<()> {
        if !is_link(link)? {
            bail!("{} 不是链接，无法按链接删除", link.display());
        }
        let destination = resolve_real_path(link)?;
        fs::remove_dir(link).with_context(|| format!("删除链接 {} 失败", link.display()))?;
        self.record(Operation::JunctionRemoved {
            link: link.to_path_buf(),
            destination,
        })
    }

    pub fn remove_empty_dir(&mut self, path: &Path) -> Result<()> {
        fs::remove_dir(path).with_context(|| format!("删除空目录 {} 失败", path.display()))?;
        self.record(Operation::EmptyDirRemoved {
            path: path.to_path_buf(),
        })
    }

    fn record_file_change(&mut self, path: &Path) -> Result<()> {
        let preimage = if path.is_file() {
            let name = format!("{:06}", self.manifest.operations.len());
            fs::copy(path, self.dir.join(PREIMAGE_DIR).join(&name))
                .with_context(|| format!("另存 {} 的迁移前版本失败", path.display()))?;
            Some(name)
        } else {
            None
        };
        self.record(Operation::FileWritten {
            path: path.to_path_buf(),
            preimage,
            after: None,
        })
    }

    /// 记录迁移结束时各文件与目录的状态并保存；没有任何改动时删除本次记录。
    pub fn finish(&mut self) -> Result<()> {
        if self.manifest.operations.is_empty() {
            return fs::remove_dir_all(&self.dir)
                .with_context(|| format!("删除空的迁移记录 {} 失败", self.dir.display()));
        }
        // 同一文件多次写入时只有最后一次的状态有比较意义
        let mut last_write: HashMap<PathBuf, usize> = HashMap::new();
        for (index, operation) in self.manifest.operations.iter().enumerate() {
            if let Operation::FileWritten { path, .. } = operation {
                last_write.insert(path.clone(), index);
            }
        }
        for (index, operation) in self.manifest.operations.iter_mut().enumerate() {
            match operation {
                Operation::FileWritten { path, after, .. }
                    if last_write.get(path) == Some(&index) =>
                {
                    *after = fingerprint(path)?;
                }
                Operation::DirCreated { path, after } => *after = fingerprint(path)?,
                _ => {}
            }
        }
        self.save()
    }
}

/// 统计文件或目录的文件数、总字节数与最新修改时间；路径不存在时返回 `None`。
fn fingerprint(path: &Path) -> Result<Option<Fingerprint>> {
    if !path.exists() {
        return Ok(None);
    }
    let mut print = Fingerprint {
        files: 0,
        bytes: 0,
        latest_modified_nanos: 0,
    };
    for entry in WalkDir::new(path) {
        let entry = entry.with_context(|| format!("遍历 {} 失败", path.display()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let metadata = entry
            .metadata()
            .with_context(|| format!("读取 {} 的属性失败", entry.path().display()))?;
        let modified = metadata
            .modified()
            .with_context(|| format!("读取 {} 的修改时间失败", entry.path().display()))?
            .duration_since(UNIX_EPOCH)
            .map(|duration| u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX))
            .unwrap_or_default();
        print.files += 1;
        print.bytes += metadata.len();
        print.latest_modified_nanos = print.latest_modified_nanos.max(modified);
    }
    Ok(Some(print))
}

/// 按逆序撤销 `record_dir` 记录的迁移，全部撤销后删除该记录。
///
/// 中途失败时保存尚未撤销的部分，处理掉失败原因（例如关闭游戏）后可以再次回退。
pub fn rollback(record_dir: &Path, log: &mut LogSink) -> Result<MigrationResult> {
    let mut manifest = load_manifest(record_dir)?;
    log.info(format!(
        "开始回退：{} → {}",
        manifest.template, manifest.target
    ));
    let mut result = MigrationResult::default();
    while let Some(operation) = manifest.operations.pop() {
        if let Err(err) = undo(record_dir, &operation, log, &mut result) {
            manifest.operations.push(operation);
            let text = serde_json::to_string_pretty(&manifest)?;
            fs::write(record_dir.join(MANIFEST_FILE), text)
                .with_context(|| format!("保存未完成的回退记录 {} 失败", record_dir.display()))?;
            return Err(err);
        }
    }
    fs::remove_dir_all(record_dir)
        .with_context(|| format!("删除已回退的记录 {} 失败", record_dir.display()))?;
    log.success(format!("回退完成，恢复 {} 处", result.changed_paths.len()));
    Ok(result)
}

fn undo(
    record_dir: &Path,
    operation: &Operation,
    log: &mut LogSink,
    result: &mut MigrationResult,
) -> Result<()> {
    match operation {
        Operation::FileWritten {
            path,
            preimage,
            after,
        } => {
            // 同一文件多次写入时只有最后一次记录了结束状态，其余写入直接按迁移前版本恢复
            if path.exists() {
                if after.is_some() && fingerprint(path)? != *after {
                    keep_modified(path, log, result)?;
                } else if preimage.is_none() {
                    fs::remove_file(path)
                        .with_context(|| format!("删除 {} 失败", path.display()))?;
                }
            }
            if let Some(name) = preimage {
                fs::copy(record_dir.join(PREIMAGE_DIR).join(name), path)
                    .with_context(|| format!("恢复 {} 失败", path.display()))?;
            }
            result.changed_paths.push(path.clone());
        }
        Operation::DirCreated { path, after } => {
            if path.exists() {
                if fingerprint(path)? != *after {
                    keep_modified(path, log, result)?;
                } else {
                    fs::remove_dir_all(path)
                        .with_context(|| format!("删除 {} 失败", path.display()))?;
                }
                result.changed_paths.push(path.clone());
            }
        }
        Operation::Moved { from, to } => {
            if fs::symlink_metadata(to).is_err() {
                let message = format!("{} 已不存在，无法移回 {}", to.display(), from.display());
                log.warn(message.clone());
                result.warnings.push(message);
                return Ok(());
            }
            if from.exists() {
                let is_empty_dir = from.is_dir()
                    && fs::read_dir(from)
                        .with_context(|| format!("读取目录 {} 失败", from.display()))?
                        .next()
                        .is_none();
                if is_empty_dir {
                    fs::remove_dir(from)
                        .with_context(|| format!("删除空目录 {} 失败", from.display()))?;
                } else {
                    keep_modified(from, log, result)?;
                }
            }
            move_dir(to, from)?;
            result.changed_paths.push(from.clone());
        }
        Operation::JunctionCreated { link } => {
            if fs::symlink_metadata(link).is_err() {
                return Ok(());
            }
            if !is_link(link)? {
                let message = format!("{} 已不是链接，保持不变", link.display());
                log.warn(message.clone());
                result.warnings.push(message);
                return Ok(());
            }
            // 对 Junction 调用 remove_dir 只删除链接本身，不影响其指向的目录
            fs::remove_dir(link).with_context(|| format!("删除链接 {} 失败", link.display()))?;
            result.changed_paths.push(link.clone());
        }
        Operation::JunctionRemoved { link, destination } => {
            if fs::symlink_metadata(link).is_ok() {
                let message = format!(
                    "{} 已存在，无法恢复指向 {} 的链接",
                    link.display(),
                    destination.display()
                );
                log.warn(message.clone());
                result.warnings.push(message);
                return Ok(());
            }
            create_junction(link, destination)?;
            result.changed_paths.push(link.clone());
        }
        Operation::EmptyDirRemoved { path } => {
            fs::create_dir_all(path)
                .with_context(|| format!("重建目录 {} 失败", path.display()))?;
        }
    }
    Ok(())
}

fn keep_modified(path: &Path, log: &mut LogSink, result: &mut MigrationResult) -> Result<()> {
    let aside = rename_aside(path, ROLLBACK_SUFFIX)?;
    let message = format!(
        "{} 在迁移后被改动过，当前版本已另存为 {}",
        path.display(),
        aside.display()
    );
    log.warn(message.clone());
    result.warnings.push(message);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::run_migration;
    use crate::test_support::TempDir;
    use crate::types::{
        CacheTransfer, ConflictChoice, DetectedInstance, LauncherKind, MigrationRequest,
        PackConflict, PackTransfer, SyncMode,
    };

    fn instance(dir: &TempDir, name: &str) -> DetectedInstance {
        DetectedInstance {
            name: name.to_string(),
            game_dir: dir.path().to_path_buf(),
            minecraft_root: dir.path().to_path_buf(),
            launcher: LauncherKind::Pcl2,
            is_isolated: true,
        }
    }

    fn write(path: &Path, content: &str) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, content)?;
        Ok(())
    }

    #[test]
    fn rollback_restores_every_change_and_keeps_later_edits() -> Result<()> {
        let template = TempDir::new("rb-template")?;
        let target = TempDir::new("rb-target")?;
        let shared = TempDir::new("rb-shared")?;
        let history = TempDir::new("rb-history")?;
        let (t, g) = (template.path(), target.path());

        write(
            &t.join("options.txt"),
            "version:4903\nkey_key.jump:key.keyboard.space\n",
        )?;
        write(
            &g.join("options.txt"),
            "version:4903\nkey_key.jump:key.keyboard.j\n",
        )?;
        let map_file = "xaero/minimap/Multiplayer_example.com/config.txt";
        write(&t.join(map_file), "template map")?;
        write(&t.join(".voxy/saves/example.com/config.json"), "voxy")?;
        write(&t.join("resourcepacks/a.zip"), "pack a")?;
        write(&g.join("resourcepacks/b.zip"), "pack b")?;
        write(&t.join("screenshots/1.png"), "shot")?;

        let request = MigrationRequest {
            template: instance(&template, "旧包"),
            target: instance(&target, "新包"),
            mode: SyncMode::Merge,
            sync_options: true,
            sync_mod_configs: false,
            sync_maps: true,
            voxy: Some(CacheTransfer::LinkToShared(shared.path().to_path_buf())),
            bobby: None,
            resource_packs: Some(PackTransfer::LinkToShared(shared.path().to_path_buf())),
            shader_packs: None,
            axiom_blueprints: None,
            screenshots: Some(PackTransfer::CopyMissing),
            report_mod_changes: false,
        };
        let mut keep = |conflicts: Vec<PackConflict>| -> Result<Vec<ConflictChoice>> {
            Ok(vec![ConflictChoice::KeepExisting; conflicts.len()])
        };
        let result = run_migration(
            &request,
            history.path(),
            &mut keep,
            &mut LogSink::new(&mut |_| {}),
        )?;
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        assert!(is_link(&t.join(".voxy/saves/example.com"))?);
        assert!(is_link(&g.join("resourcepacks"))?);

        // 模拟迁移后启动游戏又改了按键
        write(
            &g.join("options.txt"),
            "version:4903\nkey_key.jump:key.keyboard.k\n",
        )?;

        let entries = list_history(history.path())?;
        assert_eq!(entries.len(), 1);
        let rolled = rollback(&entries[0].dir, &mut LogSink::new(&mut |_| {}))?;
        assert_eq!(rolled.warnings.len(), 1);

        assert_eq!(
            fs::read_to_string(g.join("options.txt"))?,
            "version:4903\nkey_key.jump:key.keyboard.j\n"
        );
        let kept_edit = fs::read_dir(g)?
            .filter_map(|entry| entry.ok())
            .find(|entry| entry.file_name().to_string_lossy().ends_with(".rollback"))
            .context("迁移后改动的 options.txt 应另存为 .rollback")?;
        assert_eq!(
            fs::read_to_string(kept_edit.path())?,
            "version:4903\nkey_key.jump:key.keyboard.k\n"
        );
        assert!(!g.join(map_file).exists());
        assert!(!g.join("screenshots/1.png").exists());
        assert!(!g.join(".voxy/saves/example.com").exists());

        let template_voxy = t.join(".voxy/saves/example.com");
        assert!(!is_link(&template_voxy)?);
        assert_eq!(
            fs::read_to_string(template_voxy.join("config.json"))?,
            "voxy"
        );
        for (dir, pack, content) in [(t, "a.zip", "pack a"), (g, "b.zip", "pack b")] {
            let packs = dir.join("resourcepacks");
            assert!(!is_link(&packs)?);
            assert_eq!(fs::read_to_string(packs.join(pack))?, content);
        }
        assert!(list_history(history.path())?.is_empty());
        Ok(())
    }

    #[test]
    fn migration_without_changes_leaves_no_record() -> Result<()> {
        let history = TempDir::new("rb-empty-history")?;
        let mut journal = Journal::begin(history.path(), "旧包", "新包")?;
        journal.finish()?;
        assert!(list_history(history.path())?.is_empty());
        Ok(())
    }
}
