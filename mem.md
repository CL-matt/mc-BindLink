# 项目记忆文档: Minecraft 按键与配置迁移器 (MC Keybind Migrator)

## 1. 项目定位与目标
- **核心诉求**：解决玩家在跨版本（如 1.20 到 1.21 等）、切整合包时需重复设置大量按键的痛点。
- **形态方案**：基于 **Rust (eframe / egui)** 构建的单文件独立桌面应用（零运行时依赖、秒级冷启动、极小分发体积）。
- **支持启动器**：深度适配国内主流的 **HMCL** 与 **PCL2**，兼容全局根目录与版本隔离实例。

---

## 2. 核心架构与功能规划

### 2.1 实例自动发现模块 (Launcher Detection)
- **扫描源**：
  1. 用户选择目录
  2. 程序当前工作目录及其父级的 `.minecraft`
  3. 启动器配置中登记的游戏目录（可选，读取或解析失败时跳过并记录日志）：
     - HMCL：`%APPDATA%\.hmcl\hmcl.json` 与 HMCL 同目录下的 `hmcl.json`
     - PCL2：PCL 同目录下 `PCL\Setup.ini` 的 `LaunchFolders` 项
     - 上述文件格式需用真实安装验证后再实现解析
- **版本隔离识别**：
  - 深度检索 `.minecraft/versions/<Version_Name>/`，仅当该目录下存在 `<Version_Name>.json` 时视为合法版本。
  - 合法版本目录下存在独立 `options.txt` 或 `mods/` 目录时识别为隔离实例，否则归入根目录 `.minecraft` 的共享配置。
- **启动器标注**：版本目录下存在 `hmclversion.cfg` 标注为 HMCL，存在 `PCL/` 子目录标注为 PCL2，否则标注为 Unknown；标注结果仅用于界面展示，不参与实例判定。

### 2.2 Mod ID 扫描机制 (Mod Detection)
- 遍历目标实例 `mods/` 目录下的所有 `.jar` 文件。
- 解析内部元数据，而非匹配文件名：
  - **Fabric / Quilt**：解压读取 `fabric.mod.json` 中的 `id` 字段。
  - **Forge / NeoForge**：解压读取 `META-INF/mods.toml` 中的 `modId` 字段。
- 产出集合：`HashSet<String>`，供后续精准触发特定 Mod 配置规则。

### 2.3 双同步模式设计 (Sync Modes)
- **模式 A：局部合并模式 (Merge Mode)**
  - `options.txt`：保留目标整合包的视距、FOV、画质和音量设置，仅处理以 `key_` 开头的按键条目：目标已有的条目用模板值覆盖，模板有而目标缺少的条目追加到文件末尾，目标独有的条目保持不变。
  - `key_` 条目不按 Mod ID 筛选（按键名与 Mod ID 无可靠对应关系）；Minecraft 加载时忽略未知键，未安装对应 Mod 的追加条目会在游戏下次保存配置时被移除。
  - 独立 Mod 配置文件：若 Mod 存在，通过 JSONPath/TOML 键路径仅同步键位相关字段（如 Axiom 的 `keybinds`），不破坏其他个性化设置。
- **模式 B：全量覆写模式 (Full Overwrite Mode)**
  - `options.txt`：以模板实例为准，覆盖所有渲染距离、模拟距离、FOV、Gamma、各类别音量及按键绑定，下列黑名单条目保留目标原值（`fullscreen`、`maxFps`、`enableVsync` 不在黑名单内，照常覆盖）：

    | 类别 | 键 | 排除原因 |
    | --- | --- | --- |
    | 数据版本 | `version` | 跨版本复制会导致游戏误判配置版本 |
    | 窗口与显示器 | `overrideWidth`, `overrideHeight`, `fullscreenResolution` | 与显示器硬件绑定 |
    | 音频设备 | `soundDevice` | 与本机声卡绑定 |
    | 资源包 | `resourcePacks`, `incompatibleResourcePacks` | 依赖目标实例实际存在的资源包文件 |
    | 会话状态 | `lastServer` | 属于个人会话状态 |

  - 独立 Mod 配置文件：直接全量替换为模板配置，或全字段覆盖。
- **跨版本保护（两种模式通用）**：
  - `options.txt` 的 `version` 条目任何模式下都不复制。
  - 1.13 之前的按键使用数字键码，与新版 `key.keyboard.*` 格式不兼容；模板与目标分处 1.13 两侧时拒绝迁移按键条目，并在日志中说明原因。

### 2.4 独立 Mod 适配策略
- **Axiom (JSON)**：文件路径 `config/axiom.json`。合并模式下提取 `keybinds` 字段进行局部替换。


---

## 3. 技术栈与模块设计

- **语言版本**：Rust
- **核心依赖**：
  - GUI：`eframe`, `rfd`
  - 格式解析：`serde`, `serde_json`, `toml_edit`
  - IO / 压缩：`zip`, `walkdir`, `anyhow`

### 文件模块组织
- `src/main.rs`: 界面逻辑、状态存储、事件处理与日志流展示。
- `src/engine.rs`: 核心业务逻辑（`options.txt` 解析、Mod 检测、配置读写与备份）。
- `src/launcher.rs`: 启动器路径嗅探、隔离版本扫描。
- `src/types.rs`: 全局公用枚举与数据结构定义。

### 核心数据结构
- `DetectedInstance` 包含 `launcher: LauncherKind` 字段，`LauncherKind` 取值为 `Hmcl / Pcl2 / Unknown`，定义在 `src/types.rs`。

---

## 4. 健壮性与安全规范
- **自动备份**：任何写入操作前，必须先备份目标原始文件：
  - 目标旁不存在 `<文件>.bak` 时生成该文件，作为最早的原始备份永不覆盖。
  - 已存在 `<文件>.bak` 时生成 `<文件>.<unix秒>.bak`，同一文件仅保留最近 5 份带时间戳的备份。
  - 时间戳使用标准库 `std::time::SystemTime` 生成，不引入 `chrono`。
- **容错防崩**：遇到破损的 `.jar`、格式不规范的 JSON/TOML 时跳过并记录日志，不得触发 `panic!`。
- **无黑框分发**：发布模式需开启 `#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]`。
