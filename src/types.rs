use std::fmt;
use std::path::PathBuf;
use std::time::SystemTime;

/// 实例所属的启动器，仅用于界面展示，不参与实例判定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LauncherKind {
    Pcl2,
    Unknown,
}

impl fmt::Display for LauncherKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            LauncherKind::Pcl2 => "PCL2",
            LauncherKind::Unknown => "未知",
        };
        f.write_str(label)
    }
}

/// 一个可读写配置的游戏实例。
///
/// `game_dir` 指向 `options.txt`、`mods/`、`config/` 所在目录：隔离实例为
/// `versions/<名称>/`，共享实例为 `.minecraft` 根目录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedInstance {
    pub name: String,
    pub game_dir: PathBuf,
    /// 实例所属的 `.minecraft` 根目录，用于确定默认共享目录的位置。
    pub minecraft_root: PathBuf,
    pub launcher: LauncherKind,
    pub is_isolated: bool,
}

impl fmt::Display for DetectedInstance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let scope = if self.is_isolated {
            "隔离"
        } else {
            "根目录"
        };
        write!(f, "{} [{} · {}]", self.name, self.launcher, scope)
    }
}

/// `options.txt` 与 Mod 配置文件的同步方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncMode {
    /// 只同步按键相关条目，保留目标的其他设置。
    Merge,
    /// 以模板为准覆盖全部设置，黑名单条目除外。
    FullOverwrite,
}

/// 按服务器地址分子目录存放的客户端缓存，迁移规则相同，仅目录位置不同。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerCache {
    Voxy,
    Bobby,
}

impl fmt::Display for ServerCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            ServerCache::Voxy => "Voxy",
            ServerCache::Bobby => "Bobby",
        };
        f.write_str(label)
    }
}

/// 服务器缓存的迁移方式，每次迁移由用户为每种缓存分别选择。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheTransfer {
    /// 物理复制，用于迁移后将删除源缓存的场景。
    Copy,
    /// 目标建立 Junction 指向源实例的缓存目录。
    LinkToSource,
    /// 先把源缓存移动到共享目录，源与目标都建立 Junction 指向该目录。
    /// 路径为内容所在的目录：通用时为共享目录本身，隔离时为其下的共享组目录。
    LinkToShared(PathBuf),
}

/// 一次迁移的全部输入。
#[derive(Debug, Clone)]
pub struct MigrationRequest {
    pub template: DetectedInstance,
    pub target: DetectedInstance,
    pub mode: SyncMode,
    pub sync_options: bool,
    pub sync_mod_configs: bool,
    pub sync_maps: bool,
    /// 为 `None` 时不处理 Voxy 缓存。
    pub voxy: Option<CacheTransfer>,
    /// 为 `None` 时不处理 Bobby 缓存。
    pub bobby: Option<CacheTransfer>,
    /// 为 `None` 时不处理材质包。
    pub resource_packs: Option<PackTransfer>,
    /// 为 `None` 时不处理光影包。
    pub shader_packs: Option<PackTransfer>,
    /// 为 `None` 时不处理 Axiom 蓝图。
    pub axiom_blueprints: Option<PackTransfer>,
    /// 为 `None` 时不处理截图。
    pub screenshots: Option<PackTransfer>,
    /// 迁移结束后对比两个实例的 Mod 列表，结果放入 `MigrationResult::mod_diff`。
    pub report_mod_changes: bool,
}

/// 可复制或通过共享目录共用的玩家资源，冲突处理规则相同。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SharedAsset {
    ResourcePacks,
    ShaderPacks,
    AxiomBlueprints,
    Screenshots,
}

impl fmt::Display for SharedAsset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            SharedAsset::ResourcePacks => "材质包",
            SharedAsset::ShaderPacks => "光影包",
            SharedAsset::AxiomBlueprints => "Axiom 蓝图",
            SharedAsset::Screenshots => "截图",
        };
        f.write_str(label)
    }
}

/// 可通过共享目录共用的一项内容，用于选择隔离（放在共享组下）还是通用（放在共享目录根下）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ShareItem {
    Voxy,
    Bobby,
    ResourcePacks,
    ShaderPacks,
    AxiomBlueprints,
    Screenshots,
}

impl ShareItem {
    pub const ALL: [ShareItem; 6] = [
        ShareItem::Voxy,
        ShareItem::Bobby,
        ShareItem::ResourcePacks,
        ShareItem::ShaderPacks,
        ShareItem::AxiomBlueprints,
        ShareItem::Screenshots,
    ];
}

impl fmt::Display for ShareItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ShareItem::Voxy => write!(f, "{} 缓存", ServerCache::Voxy),
            ShareItem::Bobby => write!(f, "{} 缓存", ServerCache::Bobby),
            ShareItem::ResourcePacks => SharedAsset::ResourcePacks.fmt(f),
            ShareItem::ShaderPacks => SharedAsset::ShaderPacks.fmt(f),
            ShareItem::AxiomBlueprints => SharedAsset::AxiomBlueprints.fmt(f),
            ShareItem::Screenshots => SharedAsset::Screenshots.fmt(f),
        }
    }
}

/// 材质包、光影包、Axiom 蓝图与截图的迁移方式。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackTransfer {
    /// 把模板有而目标没有的条目复制到目标。
    CopyMissing,
    /// 把模板与目标的条目合并进共享目录，两个实例都建立 Junction 指向该目录。
    /// 路径为内容所在的目录：通用时为共享目录本身，隔离时为其下的共享组目录。
    LinkToShared(PathBuf),
}

/// 同名冲突中的一方，供用户比较后决定保留哪个。
#[derive(Debug, Clone)]
pub struct PackVersion {
    /// 所在实例名，或位于共享目录时为“共享目录”。
    pub owner: String,
    pub size: u64,
    pub modified: Option<SystemTime>,
    /// 蓝图保存时的游戏数据版本号；其他资源或读取失败时为 `None`。
    pub data_version: Option<i64>,
}

/// 两个同名但内容不同的资源条目。
#[derive(Debug, Clone)]
pub struct PackConflict {
    pub kind: SharedAsset,
    pub pack_name: String,
    /// 已在目的位置的版本。
    pub existing: PackVersion,
    /// 准备放入目的位置的版本。
    pub incoming: PackVersion,
}

/// 用户对单个同名冲突的选择。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictChoice {
    KeepExisting,
    /// 现有版本移入备份目录，放入新版本。
    UseIncoming,
}

/// 日志级别，界面据此选择颜色与图标。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    /// 进度与中性信息。
    Info,
    /// 有意未执行的操作，例如内容相同无需修改、条件不满足而跳过。
    Skip,
    /// 已写入、复制或建立链接。
    Success,
    /// 单个条目失败，不影响同一步骤的其他条目。
    Warning,
    /// 整个步骤失败。
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    pub level: LogLevel,
    pub message: String,
}

/// 业务模块输出日志的统一入口，把带级别的条目交给调用方提供的回调。
pub struct LogSink<'a> {
    emit: &'a mut dyn FnMut(LogEntry),
}

impl<'a> LogSink<'a> {
    pub fn new(emit: &'a mut dyn FnMut(LogEntry)) -> Self {
        Self { emit }
    }

    pub fn write(&mut self, level: LogLevel, message: impl Into<String>) {
        (self.emit)(LogEntry {
            level,
            message: message.into(),
        });
    }

    pub fn info(&mut self, message: impl Into<String>) {
        self.write(LogLevel::Info, message);
    }

    pub fn skip(&mut self, message: impl Into<String>) {
        self.write(LogLevel::Skip, message);
    }

    pub fn success(&mut self, message: impl Into<String>) {
        self.write(LogLevel::Success, message);
    }

    pub fn warn(&mut self, message: impl Into<String>) {
        self.write(LogLevel::Warning, message);
    }

    pub fn error(&mut self, message: impl Into<String>) {
        self.write(LogLevel::Error, message);
    }
}

/// 一个已安装的 Mod；元数据与 jar 清单都没有版本号时为"未知"。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModEntry {
    pub id: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModVersionChange {
    pub id: String,
    pub from: String,
    pub to: String,
}

/// 模板（旧）与目标（新）实例按 Mod ID 对比的结果，各列表按 ID 排序。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModDiff {
    pub changed: Vec<ModVersionChange>,
    /// 只在目标实例中存在。
    pub added: Vec<ModEntry>,
    /// 只在模板实例中存在。
    pub removed: Vec<ModEntry>,
}

/// 一次迁移的执行结果，供界面汇总展示。
#[derive(Debug, Default)]
pub struct MigrationResult {
    /// 被写入、复制或建立链接的路径。
    pub changed_paths: Vec<PathBuf>,
    /// 被跳过但不影响其他步骤的问题。
    pub warnings: Vec<String>,
    /// 整个步骤失败的原因，例如没有写入权限。
    pub failures: Vec<String>,
    /// 请求了 Mod 对比且对比成功时才有值。
    pub mod_diff: Option<ModDiff>,
}

/// 一条尚未回退的迁移记录，供界面列出迁移历史。
#[derive(Debug, Clone)]
pub struct HistoryEntry {
    /// 迁移开始时的 Unix 秒数。
    pub created_at: u64,
    pub template: String,
    pub target: String,
    pub operation_count: usize,
    /// 记录所在目录，回退时传给 `journal::rollback`。
    pub dir: PathBuf,
}
