//! 把界面日志同时写入 `%APPDATA%\BindLink\logs` 下的文本文件，便于关闭程序后查看或附在问题反馈中。
//!
//! 日志文件可能被公开附在问题反馈中，写入前用 [`Redactor`] 把用户目录、游戏目录与服务器地址
//! 替换为占位符；界面中的日志保持原样，方便本人排查。

use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use crate::types::{LogEntry, LogLevel};

const LOGS_DIR: [&str; 2] = ["BindLink", "logs"];
const LOG_PREFIX: &str = "bindlink-";
const LOG_SUFFIX: &str = ".log";
/// 每次启动生成一个文件，只保留最近若干个，避免长期使用后堆积。
const MAX_LOG_FILES: usize = 20;

pub fn logs_dir() -> Result<PathBuf> {
    let app_data = env::var_os("APPDATA").context("找不到 APPDATA 环境变量")?;
    Ok(LOGS_DIR
        .iter()
        .fold(PathBuf::from(app_data), |path, part| path.join(part)))
}

/// 本次运行的日志文件。
pub struct LogFile {
    file: File,
    path: PathBuf,
    redactor: Redactor,
}

/// 写入日志文件前替换敏感文本的规则，按原文长度从长到短依次替换，避免短规则截断长路径。
#[derive(Debug, Default)]
pub struct Redactor {
    rules: Vec<(String, String)>,
}

impl Redactor {
    /// `game_roots` 依次替换为 `<游戏目录1>`、`<游戏目录2>`……，服务器地址替换为 [`server_alias`]。
    pub fn new(
        user_profile: Option<&Path>,
        game_roots: &[PathBuf],
        shared_dir: Option<&Path>,
        servers: &[String],
    ) -> Self {
        let mut rules = Vec::new();
        for (index, root) in game_roots.iter().enumerate() {
            rules.push((
                root.display().to_string(),
                format!("<游戏目录{}>", index + 1),
            ));
        }
        if let Some(dir) = shared_dir {
            rules.push((dir.display().to_string(), "<共享目录>".to_string()));
        }
        if let Some(profile) = user_profile {
            rules.push((profile.display().to_string(), "%USERPROFILE%".to_string()));
        }
        for server in servers {
            rules.push((server.clone(), server_alias(server)));
        }
        rules.retain(|(original, _)| !original.is_empty());
        // 同长度按原文排序，使重复规则相邻以便去重
        rules.sort_by_key(|(original, _)| (std::cmp::Reverse(original.len()), original.clone()));
        rules.dedup_by(|a, b| a.0 == b.0);
        Self { rules }
    }

    pub fn apply(&self, text: &str) -> String {
        self.rules
            .iter()
            .fold(text.to_string(), |text, (original, alias)| {
                text.replace(original, alias)
            })
    }
}

/// 服务器地址的不可还原代号：同一地址在任何日志中都得到相同代号，便于对照排查。
///
/// 使用 64 位 FNV-1a 哈希，算法固定，不随 Rust 版本变化。
pub fn server_alias(server: &str) -> String {
    let hash = server
        .to_lowercase()
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
        });
    format!("服务器-{:08x}", hash >> 32)
}

impl LogFile {
    /// 在 `dir` 下新建本次运行的日志文件，并删除超出保留数量的旧文件。
    pub fn create(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir).with_context(|| format!("创建目录 {} 失败", dir.display()))?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("系统时间早于 1970 年")?;
        // 文件名按毫秒区分，同一秒内多次启动也不会互相覆盖
        let path = dir.join(format!("{LOG_PREFIX}{:015}{LOG_SUFFIX}", now.as_millis()));
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("创建日志文件 {} 失败", path.display()))?;
        prune_old_logs(dir)?;
        Ok(Self {
            file,
            path,
            redactor: Redactor::default(),
        })
    }

    pub fn set_redactor(&mut self, redactor: Redactor) {
        self.redactor = redactor;
    }

    pub fn append(&mut self, entry: &LogEntry) -> Result<()> {
        let line = format!(
            "{} {} {}\r\n",
            format_utc(SystemTime::now()),
            level_tag(entry.level),
            self.redactor.apply(&entry.message)
        );
        self.file
            .write_all(line.as_bytes())
            .with_context(|| format!("写入日志文件 {} 失败", self.path.display()))
    }
}

fn prune_old_logs(dir: &Path) -> Result<()> {
    let mut logs = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("读取目录 {} 失败", dir.display()))?
    {
        let path = entry
            .with_context(|| format!("读取目录 {} 失败", dir.display()))?
            .path();
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        if name.starts_with(LOG_PREFIX) && name.ends_with(LOG_SUFFIX) {
            logs.push(path);
        }
    }
    // 文件名中的时间戳等宽，按名称倒序即从新到旧
    logs.sort_by(|a, b| b.cmp(a));
    for stale in logs.into_iter().skip(MAX_LOG_FILES) {
        fs::remove_file(&stale).with_context(|| format!("删除旧日志 {} 失败", stale.display()))?;
    }
    Ok(())
}

/// 用资源管理器打开目录。
pub fn open_in_explorer(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("创建目录 {} 失败", dir.display()))?;
    // explorer.exe 成功打开窗口时也可能返回非零退出码，只检查能否启动
    Command::new("explorer")
        .arg(dir)
        .spawn()
        .with_context(|| format!("无法打开 {}", dir.display()))?;
    Ok(())
}

fn level_tag(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Info => "[信息]",
        LogLevel::Skip => "[跳过]",
        LogLevel::Success => "[完成]",
        LogLevel::Warning => "[警告]",
        LogLevel::Error => "[失败]",
    }
}

/// 格式化为 `YYYY-MM-DD HH:MM:SS UTC`；不引入时区依赖，统一使用 UTC。
fn format_utc(time: SystemTime) -> String {
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    let (days, rest) = (seconds / 86_400, seconds % 86_400);
    let (year, month, day) = civil_from_days(days as i64);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        rest / 3_600,
        rest % 3_600 / 60,
        rest % 60
    )
}

/// 把 1970-01-01 起的天数换算为公历日期（Howard Hinnant 的 civil_from_days 算法）。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_index + 2) / 5 + 1) as u32;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    } as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::test_support::TempDir;

    #[test]
    fn formats_known_dates_in_utc() {
        let at = |seconds| UNIX_EPOCH + Duration::from_secs(seconds);
        assert_eq!(format_utc(at(0)), "1970-01-01 00:00:00 UTC");
        assert_eq!(format_utc(at(951_782_400)), "2000-02-29 00:00:00 UTC");
        assert_eq!(format_utc(at(1_793_405_330)), "2026-10-31 00:08:50 UTC");
    }

    #[test]
    fn redactor_hides_paths_and_servers() {
        let redactor = Redactor::new(
            Some(Path::new(r"C:\Users\someone")),
            &[PathBuf::from(r"C:\Users\someone\Games\.minecraft")],
            Some(Path::new(r"D:\shared")),
            &["mc.example.com".to_string()],
        );
        let text = redactor.apply(
            r"复制 C:\Users\someone\Games\.minecraft\versions\a\xaero\minimap\Multiplayer_mc.example.com 到 D:\shared\voxy\mc.example.com，日志在 C:\Users\someone\AppData",
        );
        let alias = server_alias("mc.example.com");
        assert_eq!(
            text,
            format!(
                r"复制 <游戏目录1>\versions\a\xaero\minimap\Multiplayer_{alias} 到 <共享目录>\voxy\{alias}，日志在 %USERPROFILE%\AppData"
            )
        );
        assert_eq!(alias, server_alias("MC.Example.com"));
        assert_ne!(alias, server_alias("other.example.com"));
    }

    #[test]
    fn keeps_only_recent_log_files() -> Result<()> {
        let dir = TempDir::new("logs")?;
        for index in 0..MAX_LOG_FILES + 3 {
            fs::write(
                dir.path()
                    .join(format!("{LOG_PREFIX}{index:015}{LOG_SUFFIX}")),
                "",
            )?;
        }
        let mut log = LogFile::create(dir.path())?;
        log.append(&LogEntry {
            level: LogLevel::Warning,
            message: "测试".to_string(),
        })?;

        let mut remaining: Vec<PathBuf> = fs::read_dir(dir.path())?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<_>>()?;
        remaining.sort();
        assert_eq!(remaining.len(), MAX_LOG_FILES);
        let newest = remaining
            .last()
            .ok_or_else(|| anyhow::anyhow!("没有日志文件"))?;
        assert!(fs::read_to_string(newest)?.contains("[警告] 测试"));
        Ok(())
    }
}
