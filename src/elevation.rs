//! 写入权限检查与以管理员身份重新启动。
//!
//! 安装在 `Program Files` 等受保护目录的实例，普通权限无法写入；程序默认以普通权限运行，
//! 只在检测到需要写入的目录不可写时提示用户提权重启。

use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{self, Command};

use anyhow::{Context, Result, bail};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const ARG_DIR: &str = "--dir";
const ARG_TEMPLATE: &str = "--template";
const ARG_TARGET: &str = "--target";

/// 返回 `dirs` 中当前进程无法写入的目录（去重，保持原顺序）。
///
/// 尚不存在的目录检查其最近的已存在上级目录，因为迁移时会在那里创建它。
pub fn find_unwritable(dirs: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut unwritable: Vec<PathBuf> = Vec::new();
    for dir in dirs {
        let Some(existing) = dir.ancestors().find(|path| path.is_dir()) else {
            continue;
        };
        if !unwritable.iter().any(|known| known == existing) && !is_writable(existing)? {
            unwritable.push(existing.to_path_buf());
        }
    }
    Ok(unwritable)
}

/// 通过实际创建并删除一个临时文件判断目录是否可写；ACL、只读介质等任何原因导致失败都视为不可写。
fn is_writable(dir: &Path) -> Result<bool> {
    let probe = dir.join(format!(".bindlink-write-check-{}", process::id()));
    match OpenOptions::new().write(true).create_new(true).open(&probe) {
        Ok(file) => {
            drop(file);
            fs::remove_file(&probe)
                .with_context(|| format!("删除权限检查文件 {} 失败", probe.display()))?;
            Ok(true)
        }
        Err(_) => Ok(false),
    }
}

/// 以管理员身份重新启动时需要恢复的界面状态。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct StartupArgs {
    /// 用户通过"添加目录…"加入的目录。
    pub dirs: Vec<PathBuf>,
    /// 模板与目标实例的游戏目录，重启后据此重新选中。
    pub template: Option<PathBuf>,
    pub target: Option<PathBuf>,
}

impl StartupArgs {
    /// 解析命令行参数，跳过无法识别的参数。
    pub fn parse(args: impl IntoIterator<Item = OsString>) -> Self {
        let mut parsed = Self::default();
        let mut args = args.into_iter();
        while let Some(flag) = args.next() {
            let Some(value) = args.next().map(PathBuf::from) else {
                break;
            };
            match flag.to_str() {
                Some(ARG_DIR) => parsed.dirs.push(value),
                Some(ARG_TEMPLATE) => parsed.template = Some(value),
                Some(ARG_TARGET) => parsed.target = Some(value),
                _ => {}
            }
        }
        parsed
    }

    fn to_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        for dir in &self.dirs {
            args.extend([ARG_DIR.to_string(), dir.display().to_string()]);
        }
        if let Some(template) = &self.template {
            args.extend([ARG_TEMPLATE.to_string(), template.display().to_string()]);
        }
        if let Some(target) = &self.target {
            args.extend([ARG_TARGET.to_string(), target.display().to_string()]);
        }
        args
    }
}

/// 通过 PowerShell 的 `Start-Process -Verb RunAs` 以管理员身份启动新的实例并等待其返回。
///
/// 用户在 UAC 窗口中拒绝时返回错误，调用方据此决定是否关闭当前窗口。
pub fn restart_as_admin(args: &StartupArgs) -> Result<()> {
    let exe = std::env::current_exe().context("无法确定程序路径")?;
    let quote = |text: &str| format!("'{}'", text.replace('\'', "''"));
    // Start-Process 用空格拼接参数，含空格的路径需要再包一层双引号
    let arguments: Vec<String> = args
        .to_args()
        .iter()
        .map(|arg| quote(&format!("\"{arg}\"")))
        .collect();
    let mut command = format!(
        "Start-Process -FilePath {} -Verb RunAs",
        quote(&exe.display().to_string())
    );
    if !arguments.is_empty() {
        command.push_str(&format!(" -ArgumentList @({})", arguments.join(",")));
    }
    let status = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &command])
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .context("无法启动 PowerShell")?;
    if !status.success() {
        bail!("未能以管理员身份启动，可能在系统提示中选择了\"否\"");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    #[test]
    fn startup_args_round_trip() {
        let original = StartupArgs {
            dirs: vec![PathBuf::from(r"D:\Games\My PCL"), PathBuf::from(r"E:\mc")],
            template: Some(PathBuf::from(r"D:\Games\My PCL\.minecraft\versions\old")),
            target: Some(PathBuf::from(r"D:\Games\My PCL\.minecraft\versions\new")),
        };
        let parsed = StartupArgs::parse(original.to_args().into_iter().map(OsString::from));
        assert_eq!(parsed, original);
    }

    #[test]
    fn writable_temp_dir_and_missing_child_pass_the_check() -> Result<()> {
        let dir = TempDir::new("writable")?;
        let missing = dir.path().join("not-yet-created/bindlink-shared");
        assert!(find_unwritable(&[dir.path().to_path_buf(), missing])?.is_empty());
        assert_eq!(fs::read_dir(dir.path())?.count(), 0);
        Ok(())
    }
}
