//! 不经过迁移记录的文件系统基础操作：路径解析、占用检测、目录移动与 Junction 创建。

use std::fs::{self, OpenOptions};
use std::io::ErrorKind;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use walkdir::WalkDir;

const ERROR_SHARING_VIOLATION: i32 = 32;
const ERROR_LOCK_VIOLATION: i32 = 33;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 解析链接后的真实路径，并去掉 `canonicalize` 附加的 `\\?\` 前缀，使其可直接传给 `mklink`。
pub fn resolve_real_path(path: &Path) -> Result<PathBuf> {
    let canonical =
        fs::canonicalize(path).with_context(|| format!("解析路径 {} 失败", path.display()))?;
    let text = canonical.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(stripped) if !stripped.starts_with("UNC\\") => Ok(PathBuf::from(stripped)),
        _ => Ok(canonical),
    }
}

pub fn is_link(path: &Path) -> Result<bool> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("读取 {} 的属性失败", path.display()))?;
    Ok(metadata.file_type().is_symlink())
}

/// 逐个以不共享模式打开缓存中的文件，任何文件仍被其他进程打开时都会失败。
///
/// 游戏运行时会一直打开正在读写的缓存文件（Voxy 的 RocksDB、Bobby 的区块文件），
/// 据此判断对应实例是否正在运行。
pub fn ensure_cache_not_in_use(cache_dir: &Path) -> Result<()> {
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

/// 把 `path` 改名为同目录下的 `<原名>.<unix秒>.<suffix>`，返回新路径；用于在覆盖或删除前保留原内容。
pub fn rename_aside(path: &Path, suffix: &str) -> Result<PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("无法取得名称：{}", path.display()))?
        .to_string_lossy()
        .into_owned();
    let aside = path.with_file_name(format!("{name}.{}.{suffix}", unix_seconds()?));
    if aside.exists() {
        bail!("{} 已存在", aside.display());
    }
    fs::rename(path, &aside).with_context(|| format!("重命名 {} 失败", path.display()))?;
    Ok(aside)
}

pub fn unix_seconds() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("系统时间早于 1970 年")?
        .as_secs())
}

pub fn move_dir(from: &Path, to: &Path) -> Result<()> {
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

pub fn copy_dir_recursive(source: &Path, destination: &Path) -> Result<()> {
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
pub fn create_junction(link: &Path, destination: &Path) -> Result<()> {
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
