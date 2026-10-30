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
    /// 为 `None` 时不处理 Axiom 蓝图。
    pub axiom_blueprints: Option<PackTransfer>,
}

/// 可复制或通过共享目录共用的玩家资源，冲突处理规则相同。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SharedAsset {
    ResourcePacks,
    AxiomBlueprints,
}

impl fmt::Display for SharedAsset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            SharedAsset::ResourcePacks => "材质包",
            SharedAsset::AxiomBlueprints => "Axiom 蓝图",
        };
        f.write_str(label)
    }
}

/// 材质包与 Axiom 蓝图的迁移方式。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackTransfer {
    /// 把模板有而目标没有的条目复制到目标。
    CopyMissing,
    /// 把模板与目标的条目合并进共享目录，两个实例都建立 Junction 指向该目录。
    LinkToShared(PathBuf),
}

/// 同名冲突中的一方，供用户比较后决定保留哪个。
#[derive(Debug, Clone)]
pub struct PackVersion {
    /// 所在实例名，或位于共享目录时为“共享目录”。
    pub owner: String,
    pub size: u64,
    pub modified: Option<SystemTime>,
    /// 蓝图保存时的游戏数据版本号；材质包或读取失败时为 `None`。
    pub data_version: Option<i64>,
}

/// 两个同名但内容不同的材质包或蓝图。
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

/// 一次迁移的执行结果，供界面汇总展示。
#[derive(Debug, Default)]
pub struct MigrationResult {
    /// 被写入、复制或建立链接的路径。
    pub changed_paths: Vec<PathBuf>,
    /// 被跳过但不影响其他步骤的问题。
    pub warnings: Vec<String>,
}
