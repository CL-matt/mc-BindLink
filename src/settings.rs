//! 跨次保存的界面偏好，位于 `%APPDATA%\BindLink\settings.json`。
//!
//! 只保存不影响迁移安全的选择；文件缺失或损坏时使用默认值，不阻止程序启动。

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::types::ShareItem;

const SETTINGS_PATH: [&str; 2] = ["BindLink", "settings.json"];

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Settings {
    /// 设为通用（不分组）的内容，按共享子目录名记录；未列出的默认隔离。
    #[serde(default)]
    common_items: Vec<String>,
}

impl Settings {
    pub fn is_common(&self, item: ShareItem) -> bool {
        self.common_items
            .iter()
            .any(|key| key == item.shared_subdir())
    }

    pub fn set_common(&mut self, item: ShareItem, common: bool) {
        self.common_items.retain(|key| key != item.shared_subdir());
        if common {
            self.common_items.push(item.shared_subdir().to_string());
            self.common_items.sort();
        }
    }
}

pub fn settings_path() -> Result<PathBuf> {
    let app_data = env::var_os("APPDATA").context("找不到 APPDATA 环境变量")?;
    Ok(SETTINGS_PATH
        .iter()
        .fold(PathBuf::from(app_data), |path, part| path.join(part)))
}

/// 读取设置；文件不存在时返回默认值。
pub fn load(path: &Path) -> Result<Settings> {
    if !path.exists() {
        return Ok(Settings::default());
    }
    let text = fs::read_to_string(path).with_context(|| format!("读取 {} 失败", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("解析 {} 失败", path.display()))
}

pub fn save(path: &Path, settings: &Settings) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("创建目录 {} 失败", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(settings)?;
    fs::write(path, text).with_context(|| format!("写入 {} 失败", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    #[test]
    fn round_trips_common_items() -> Result<()> {
        let dir = TempDir::new("settings")?;
        let path = dir.path().join("BindLink/settings.json");
        assert!(!load(&path)?.is_common(ShareItem::Screenshots));

        let mut settings = Settings::default();
        settings.set_common(ShareItem::Screenshots, true);
        settings.set_common(ShareItem::Voxy, true);
        settings.set_common(ShareItem::Voxy, false);
        save(&path, &settings)?;

        let loaded = load(&path)?;
        assert!(loaded.is_common(ShareItem::Screenshots));
        assert!(!loaded.is_common(ShareItem::Voxy));
        Ok(())
    }
}
