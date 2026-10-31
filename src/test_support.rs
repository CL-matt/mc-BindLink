use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;

/// 测试用临时目录，离开作用域时删除。
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new(label: &str) -> Result<Self> {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = std::env::temp_dir().join(format!(
            "mc-bindsync-{label}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // remove_dir_all 不会跟随 Junction 删除其指向的目录
        if let Err(err) = fs::remove_dir_all(&self.path) {
            eprintln!("清理临时目录 {} 失败：{err}", self.path.display());
        }
    }
}

/// 写入临时目录的迁移记录；返回的 `TempDir` 需与记录一起保持存活。
pub fn test_journal() -> Result<(TempDir, crate::journal::Journal)> {
    let history = TempDir::new("history")?;
    let journal = crate::journal::Journal::begin(history.path(), "模板", "目标")?;
    Ok((history, journal))
}
