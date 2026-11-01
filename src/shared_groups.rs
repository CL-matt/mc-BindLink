//! 共享目录中的共享组：组名校验、列出已有组，以及识别实例当前链接到哪个共享目录与组。
//!
//! 通用内容直接放在共享目录根下（`<共享目录>\screenshots`），隔离内容放在组目录下
//! （`<共享目录>\<组名>\screenshots`），组名不得与各内容的子目录同名。

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::fs_ops::{is_link, resolve_real_path};
use crate::map_sync;
use crate::pack_sync;
use crate::types::{ServerCache, ShareItem, SharedAsset};

const MAX_GROUP_NAME_CHARS: usize = 64;
const FORBIDDEN_CHARS: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];
const WINDOWS_DEVICE_NAMES: &[&str] = &["CON", "PRN", "AUX", "NUL"];

/// 一项内容的实际种类：按服务器分目录的缓存，或整个目录共享的资源。
#[derive(Debug, Clone, Copy)]
enum ItemKind {
    Cache(ServerCache),
    Asset(SharedAsset),
}

impl ShareItem {
    fn kind(self) -> ItemKind {
        match self {
            ShareItem::Voxy => ItemKind::Cache(ServerCache::Voxy),
            ShareItem::Bobby => ItemKind::Cache(ServerCache::Bobby),
            ShareItem::ResourcePacks => ItemKind::Asset(SharedAsset::ResourcePacks),
            ShareItem::ShaderPacks => ItemKind::Asset(SharedAsset::ShaderPacks),
            ShareItem::AxiomBlueprints => ItemKind::Asset(SharedAsset::AxiomBlueprints),
            ShareItem::Screenshots => ItemKind::Asset(SharedAsset::Screenshots),
        }
    }

    /// 内容在共享目录或组目录下的子目录名，也用作设置文件中的键。
    pub fn shared_subdir(self) -> &'static str {
        match self.kind() {
            ItemKind::Cache(kind) => map_sync::shared_subdir(kind),
            ItemKind::Asset(kind) => pack_sync::shared_subdir(kind),
        }
    }
}

/// 校验用户输入的组名，返回界面可直接显示的原因。
pub fn validate_group_name(name: &str) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err("请填写共享组名称".to_string());
    }
    if name != name.trim() || name.ends_with('.') {
        return Err("组名首尾不能有空格，末尾不能是句点".to_string());
    }
    if name.starts_with('.') {
        return Err("组名不能以句点开头".to_string());
    }
    if name.chars().count() > MAX_GROUP_NAME_CHARS {
        return Err(format!("组名不能超过 {MAX_GROUP_NAME_CHARS} 个字符"));
    }
    if name
        .chars()
        .any(|c| c.is_control() || FORBIDDEN_CHARS.contains(&c))
    {
        return Err("组名不能包含 < > : \" / \\ | ? * 等字符".to_string());
    }
    let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    let is_device = WINDOWS_DEVICE_NAMES.contains(&stem.as_str())
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem.ends_with(|c: char| c.is_ascii_digit()));
    if is_device {
        return Err("组名不能使用 Windows 保留名称".to_string());
    }
    if is_reserved(name) {
        return Err("组名不能与共享目录中的内容子目录同名".to_string());
    }
    Ok(())
}

/// 与内容子目录同名，或是其备份目录（如 `screenshots.bak`）。
fn is_reserved(name: &str) -> bool {
    let lower = name.to_lowercase();
    ShareItem::ALL.iter().any(|item| {
        let subdir = item.shared_subdir();
        lower == subdir || lower.starts_with(&format!("{subdir}."))
    })
}

/// 列出共享目录中已有的组，按名称排序；共享目录不存在时为空。
pub fn list_groups(shared_root: &Path) -> Result<Vec<String>> {
    if !shared_root.is_dir() {
        return Ok(Vec::new());
    }
    let mut groups = Vec::new();
    for entry in fs::read_dir(shared_root)
        .with_context(|| format!("读取目录 {} 失败", shared_root.display()))?
    {
        let entry = entry.with_context(|| format!("读取目录 {} 失败", shared_root.display()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.path().is_dir() && validate_group_name(&name).is_ok() {
            groups.push(name);
        }
    }
    groups.sort();
    Ok(groups)
}

/// 实例中一项内容当前链接到的位置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkedLocation {
    pub item: ShareItem,
    pub shared_root: PathBuf,
    /// 为 `None` 时链接到通用目录。
    pub group: Option<String>,
}

/// 检查实例各项内容的 Junction，找出链接到 `candidate_roots` 中某个共享目录的项。
///
/// 只读取链接目标，不读取内容；链接到候选共享目录以外的项不列出。
pub fn detect_linked_locations(
    game_dir: &Path,
    candidate_roots: &[PathBuf],
) -> Result<Vec<LinkedLocation>> {
    let roots: Vec<(PathBuf, PathBuf)> = candidate_roots
        .iter()
        .filter(|root| root.is_dir())
        .map(|root| Ok((root.clone(), resolve_real_path(root)?)))
        .collect::<Result<_>>()?;
    let mut found = Vec::new();
    for item in ShareItem::ALL {
        for content_dir in linked_content_dirs(game_dir, item)? {
            if let Some(location) = locate(item, &content_dir, &roots)
                && !found.contains(&location)
            {
                found.push(location);
            }
        }
    }
    Ok(found)
}

/// 实例中该项内容各个 Junction 所指向的内容目录（缓存为服务器目录的上一级）。
fn linked_content_dirs(game_dir: &Path, item: ShareItem) -> Result<Vec<PathBuf>> {
    let mut dirs = Vec::new();
    match item.kind() {
        ItemKind::Cache(kind) => {
            let cache_root = game_dir.join(map_sync::cache_root(kind));
            if !cache_root.is_dir() {
                return Ok(dirs);
            }
            for entry in fs::read_dir(&cache_root)
                .with_context(|| format!("读取目录 {} 失败", cache_root.display()))?
            {
                let entry =
                    entry.with_context(|| format!("读取目录 {} 失败", cache_root.display()))?;
                let path = entry.path();
                if path.exists()
                    && is_link(&path)?
                    && let Some(parent) = resolve_real_path(&path)?.parent()
                {
                    dirs.push(parent.to_path_buf());
                }
            }
        }
        ItemKind::Asset(kind) => {
            let path = game_dir.join(pack_sync::asset_dir(kind));
            if path.exists() && is_link(&path)? {
                dirs.push(resolve_real_path(&path)?);
            }
        }
    }
    Ok(dirs)
}

fn locate(
    item: ShareItem,
    content_dir: &Path,
    roots: &[(PathBuf, PathBuf)],
) -> Option<LinkedLocation> {
    roots.iter().find_map(|(root, real_root)| {
        let relative = content_dir.strip_prefix(real_root).ok()?;
        let parts: Vec<String> = relative
            .components()
            .map(|part| part.as_os_str().to_string_lossy().into_owned())
            .collect();
        let group = match parts.as_slice() {
            [subdir] if subdir.eq_ignore_ascii_case(item.shared_subdir()) => None,
            [group, subdir]
                if subdir.eq_ignore_ascii_case(item.shared_subdir())
                    && validate_group_name(group).is_ok() =>
            {
                Some(group.clone())
            }
            _ => return None,
        };
        Some(LinkedLocation {
            item,
            shared_root: root.clone(),
            group,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs_ops::create_junction;
    use crate::test_support::TempDir;

    #[test]
    fn validates_group_names() {
        assert!(validate_group_name("neko").is_ok());
        assert!(validate_group_name("neko 1.20.1").is_ok());
        assert!(validate_group_name("").is_err());
        assert!(validate_group_name(" neko").is_err());
        assert!(validate_group_name("neko.").is_err());
        assert!(validate_group_name(".hidden").is_err());
        assert!(validate_group_name("a/b").is_err());
        assert!(validate_group_name("Screenshots").is_err());
        assert!(validate_group_name("voxy.bak").is_err());
        assert!(validate_group_name("com1").is_err());
        assert!(validate_group_name("nul.txt").is_err());
        assert!(validate_group_name("console").is_ok());
    }

    #[test]
    fn lists_only_group_directories() -> Result<()> {
        let root = TempDir::new("groups-list")?;
        for dir in [
            "screenshots",
            "screenshots.bak",
            "neko",
            "loring",
            ".hidden",
        ] {
            fs::create_dir_all(root.path().join(dir))?;
        }
        fs::write(root.path().join("note.txt"), "")?;
        assert_eq!(list_groups(root.path())?, vec!["loring", "neko"]);
        Ok(())
    }

    #[test]
    fn detects_common_and_group_links() -> Result<()> {
        let game = TempDir::new("groups-detect-game")?;
        let root = TempDir::new("groups-detect-root")?;
        let other = TempDir::new("groups-detect-other")?;
        let common_shots = root.path().join("screenshots");
        let group_server = root.path().join("neko/voxy/example.com");
        let foreign_packs = other.path().join("resourcepacks");
        for dir in [&common_shots, &group_server, &foreign_packs] {
            fs::create_dir_all(dir)?;
        }
        fs::create_dir_all(game.path().join(".voxy/saves"))?;
        create_junction(&game.path().join("screenshots"), &common_shots)?;
        create_junction(&game.path().join(".voxy/saves/example.com"), &group_server)?;
        create_junction(&game.path().join("resourcepacks"), &foreign_packs)?;

        let found = detect_linked_locations(game.path(), &[root.path().to_path_buf()])?;

        assert_eq!(
            found,
            vec![
                LinkedLocation {
                    item: ShareItem::Voxy,
                    shared_root: root.path().to_path_buf(),
                    group: Some("neko".to_string()),
                },
                LinkedLocation {
                    item: ShareItem::Screenshots,
                    shared_root: root.path().to_path_buf(),
                    group: None,
                },
            ]
        );
        Ok(())
    }
}
