use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::types::{DetectedInstance, LauncherKind};

const MINECRAFT_DIR_NAME: &str = ".minecraft";
const VERSIONS_DIR_NAME: &str = "versions";

/// 从用户选择的目录与程序工作目录及其父级的 `.minecraft` 中发现全部实例。
///
/// 单个 `.minecraft` 读取失败时只记录日志，不影响其他目录的扫描结果。
pub fn scan_all_instances(
    selected_dirs: &[PathBuf],
    log: &mut dyn FnMut(String),
) -> Vec<DetectedInstance> {
    let mut instances = Vec::new();
    for root in collect_minecraft_roots(selected_dirs, log) {
        match scan_minecraft_root(&root) {
            Ok(found) => instances.extend(found),
            Err(err) => log(format!("扫描 {} 失败：{err:#}", root.display())),
        }
    }
    log(format!("共发现 {} 个实例", instances.len()));
    instances
}

fn collect_minecraft_roots(selected_dirs: &[PathBuf], log: &mut dyn FnMut(String)) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    for dir in selected_dirs {
        match resolve_minecraft_root(dir) {
            Some(root) => candidates.push(root),
            None => log(format!("所选目录 {} 中未找到 .minecraft", dir.display())),
        }
    }
    match env::current_dir() {
        Ok(cwd) => candidates.extend(
            cwd.ancestors()
                .map(|dir| dir.join(MINECRAFT_DIR_NAME))
                .filter(|dir| dir.join(VERSIONS_DIR_NAME).is_dir()),
        ),
        Err(err) => log(format!("无法读取程序工作目录：{err}")),
    }

    let mut roots: Vec<PathBuf> = Vec::new();
    for candidate in candidates {
        // 同一目录可能经由不同写法或链接被重复发现
        let normalized = fs::canonicalize(&candidate).unwrap_or_else(|_| candidate.clone());
        let already_listed = roots
            .iter()
            .any(|root| fs::canonicalize(root).unwrap_or_else(|_| root.clone()) == normalized);
        if !already_listed {
            roots.push(candidate);
        }
    }
    roots
}

/// 接受 `.minecraft` 本身、其父目录或某个版本目录，返回对应的 `.minecraft` 根目录。
fn resolve_minecraft_root(dir: &Path) -> Option<PathBuf> {
    if dir.join(VERSIONS_DIR_NAME).is_dir() {
        return Some(dir.to_path_buf());
    }
    let nested = dir.join(MINECRAFT_DIR_NAME);
    if nested.join(VERSIONS_DIR_NAME).is_dir() {
        return Some(nested);
    }
    let versions_dir = dir.parent()?;
    if versions_dir.file_name()? == VERSIONS_DIR_NAME {
        return versions_dir.parent().map(Path::to_path_buf);
    }
    None
}

fn scan_minecraft_root(root: &Path) -> Result<Vec<DetectedInstance>> {
    let owner = root
        .parent()
        .and_then(Path::file_name)
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut instances = vec![DetectedInstance {
        name: format!("{owner}/{MINECRAFT_DIR_NAME}"),
        game_dir: root.to_path_buf(),
        launcher: LauncherKind::Unknown,
        is_isolated: false,
    }];

    let versions_dir = root.join(VERSIONS_DIR_NAME);
    let mut isolated = Vec::new();
    for entry in fs::read_dir(&versions_dir)
        .with_context(|| format!("读取目录 {} 失败", versions_dir.display()))?
    {
        let entry = entry.with_context(|| format!("读取目录 {} 失败", versions_dir.display()))?;
        let version_dir = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_valid_version = version_dir.join(format!("{name}.json")).is_file();
        let has_own_config =
            version_dir.join("options.txt").is_file() || version_dir.join("mods").is_dir();
        if is_valid_version && has_own_config {
            isolated.push(DetectedInstance {
                name,
                launcher: detect_launcher_kind(&version_dir),
                game_dir: version_dir,
                is_isolated: true,
            });
        }
    }
    isolated.sort_by(|a, b| a.name.cmp(&b.name));
    instances.extend(isolated);
    Ok(instances)
}

fn detect_launcher_kind(version_dir: &Path) -> LauncherKind {
    if version_dir.join("PCL").is_dir() {
        LauncherKind::Pcl2
    } else {
        LauncherKind::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    fn create_version(root: &Path, name: &str) -> Result<PathBuf> {
        let dir = root.join(VERSIONS_DIR_NAME).join(name);
        fs::create_dir_all(&dir)?;
        fs::write(dir.join(format!("{name}.json")), "{}")?;
        Ok(dir)
    }

    #[test]
    fn detects_isolated_instances_and_launcher_markers() -> Result<()> {
        let base = TempDir::new("launcher")?;
        let root = base.path().join(MINECRAFT_DIR_NAME);

        let pcl = create_version(&root, "pcl-pack")?;
        fs::create_dir_all(pcl.join("PCL"))?;
        fs::create_dir_all(pcl.join("mods"))?;
        let unmarked = create_version(&root, "other-pack")?;
        fs::write(unmarked.join("options.txt"), "")?;
        create_version(&root, "shared-vanilla")?;
        let invalid = root.join(VERSIONS_DIR_NAME).join("no-json");
        fs::create_dir_all(invalid.join("mods"))?;

        let instances = scan_minecraft_root(&root)?;

        let summary: Vec<(&str, LauncherKind, bool)> = instances
            .iter()
            .map(|i| (i.name.as_str(), i.launcher, i.is_isolated))
            .collect();
        let root_name = format!(
            "{}/.minecraft",
            base.path()
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        );
        assert_eq!(
            summary,
            vec![
                (root_name.as_str(), LauncherKind::Unknown, false),
                ("other-pack", LauncherKind::Unknown, true),
                ("pcl-pack", LauncherKind::Pcl2, true),
            ]
        );
        Ok(())
    }

    #[test]
    fn resolves_root_from_parent_or_version_dir() -> Result<()> {
        let base = TempDir::new("launcher-resolve")?;
        let root = base.path().join(MINECRAFT_DIR_NAME);
        let version = create_version(&root, "pack")?;

        assert_eq!(resolve_minecraft_root(base.path()), Some(root.clone()));
        assert_eq!(resolve_minecraft_root(&root), Some(root.clone()));
        assert_eq!(resolve_minecraft_root(&version), Some(root));
        assert_eq!(resolve_minecraft_root(&base.path().join("missing")), None);
        Ok(())
    }
}
