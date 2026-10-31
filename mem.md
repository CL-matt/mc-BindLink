# 项目记忆文档: BindLink 键链（Minecraft 按键与配置迁移器）

## 1. 项目定位与目标
- **核心诉求**：解决玩家在跨版本（如 1.20 到 1.21 等）、切整合包时需重复设置大量按键的痛点。
- **形态方案**：基于 **Rust (eframe / egui)** 构建的单文件独立桌面应用（零运行时依赖、秒级冷启动、极小分发体积）。
- **软件名称**：BindLink，中文名"键链"，取"按键绑定（Bind）+ 链接（Link）"之意；仓库名与 Cargo 包名保持 `mc-bindsync`，生成的可执行文件名为 `bindlink.exe`（`Cargo.toml` 的 `[[bin]]` 配置）。
- **版本号**：在 `Cargo.toml` 的 `version` 中维护，当前为 `1.0.0+0105`（`+` 后为构建标识，语义化版本不允许数字段带前导零，因此 0103 放在构建标识中）；窗口标题通过 `CARGO_PKG_VERSION` 显示为 `BindLink 键链 v<版本号>`，界面内标题栏不显示版本号。每完成并验证通过一个功能修改，把 `+` 后的四位构建号加 1（如 `0104` 之后为 `0105`），与该功能的代码在同一次提交中修改。
- **标志**：两个互扣的圆角方形键帽，绿色 `#4C9162` 代表模板实例，深灰 `#2F3B45` 代表目标实例，右上交叉处绿色压在深灰之上形成链环。矢量原稿为 `logos/bindlink.svg`（64×64 视图），`logos/bindlink-256.png` 按同一几何参数绘制，用作窗口图标与标题栏标志；修改标志时两者必须同步更新。
- **支持启动器**：适配国内主流的 **PCL2**，兼容全局根目录与版本隔离实例；HMCL 适配列入 [todolist.md](todolist.md)。

---

## 2. 核心架构与功能规划

### 2.1 实例自动发现模块 (Launcher Detection)
- **扫描源**：
  1. 用户选择目录
  2. 程序当前工作目录及其父级的 `.minecraft`
  - 启动器配置中登记的游戏目录暂不作为扫描源，见 [todolist.md](todolist.md)。
- **版本隔离识别**：
  - 深度检索 `.minecraft/versions/<Version_Name>/`，仅当该目录下存在 `<Version_Name>.json` 时视为合法版本。
  - 合法版本目录下存在独立 `options.txt` 或 `mods/` 目录时识别为隔离实例，否则归入根目录 `.minecraft` 的共享配置。
- **启动器标注**：版本目录下存在 `PCL/` 子目录标注为 PCL2，否则标注为 Unknown；标注结果仅用于界面展示，不参与实例判定。

### 2.2 Mod ID 扫描机制 (Mod Detection)
- 遍历目标实例 `mods/` 目录下的所有 `.jar` 文件。
- 解析内部元数据，而非匹配文件名：
  - **Fabric**：解压读取 `fabric.mod.json` 中的 `id` 字段。
  - **Quilt**：解压读取 `quilt.mod.json` 中的 `quilt_loader.id` 字段。
  - **Forge / NeoForge**：解压读取 `META-INF/mods.toml` 或 `META-INF/neoforge.mods.toml`（NeoForge 1.20.5 起使用，已在 1.21.1 NeoForge 实例中实测）中 `[[mods]]` 的 `modId` 字段；跳过未经构建替换的 `${...}` 占位符。
- 只扫描扩展名为 `.jar` 的文件，`.jar.disabled` 等已禁用的 Mod 不计入。
- 容错：每份元数据单独解析，一份无法解析只记录问题，不影响从同一 jar 其他元数据读到的 ID（"universal" 类 jar 常残留未替换占位符的 `mods.toml`）；`fabric.mod.json`、`quilt.mod.json` 解析前把字符串中直接出现的换行等控制字符转义，与 Fabric 加载器的宽松读取一致；`mods.toml` 整体解析失败时，逐行查找 `[[mods]]` 下的 `modId`。只在一个 ID 都读不到时记为警告，读到 ID 但有元数据损坏时记为跳过。
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
  - 1.13 之前的按键使用数字键码，与新版 `key.keyboard.*` 格式不兼容；模板与目标分处 1.13 两侧时拒绝迁移按键条目，并在日志中说明原因。判定依据为 `version` 数据版本号是否小于 1519（1.13 正式版），缺少 `version` 时按 `key_` 条目的值是否为纯数字判断。
  - 目标实例不存在 `options.txt` 时跳过并给出警告，提示用户先启动一次游戏：新文件缺少 `version` 条目会让游戏按最旧格式升级配置，而 `version` 又禁止从模板复制。

### 2.4 独立 Mod 适配策略
- 仅当目标实例 `mods/` 中识别到对应 Mod ID 时才处理该 Mod 的配置文件。
- **Axiom (JSON)**：Mod ID `axiom`，文件路径 `config/axiom/axiom.json`（已在 PCL 实例中实测）。合并模式下仅替换顶层 `keybinds` 字段；目标文件不存在时复制整个模板文件。

### 2.5 独立地图与缓存同步器 (`map_sync.rs`)
- **常规地图与路点 (Xaero)**：
  - 目录：
    - Xaero 新版（已在 PCL 实例中实测）：`xaero/minimap/<世界键>/`（路点）与 `xaero/world-map/<世界键>/`（地图）。
    - Xaero 旧版：`XaeroWaypoints`, `XaeroWorldMap`。
    - JourneyMap 适配列入 [todolist.md](todolist.md)。
    - `<世界键>` 为单人世界存档名，或 `Multiplayer_<服务器地址>`（如 `Multiplayer_example.com`）。
  - 策略：以 `<世界键>` 为单位进行文件夹级无损递归同步：
    - 目标缺少的文件直接复制。
    - 同名文件仅在模板文件修改时间更新时覆盖。
    - 目标独有的文件不删除。
- **服务器缓存（Voxy 与 Bobby）**：两者都按世界或服务器地址分子目录存放，使用同一套迁移规则，界面上分别选择处理方式。
  - Voxy 超远视距缓存（已在 PCL 实例中实测）：`.voxy/saves/<世界或服务器地址>/`，内含 `config.json` 与 `<哈希>/storage`（RocksDB 数据库），体积通常达数百 MB 至数 GB。
  - Bobby 区块缓存（已在两个 PCL 实例中实测）：`.bobby/<服务器地址>/<种子哈希>/<命名空间>/<维度>/`，内含 `*.mca` 区块文件与 `last_access`，只缓存多人服务器，单个实例实测 27 MB 至 505 MB。Bobby 自身配置 `config/bobby.conf` 不同步。
  - 同步单位：缓存根目录下的每个 `<世界或服务器地址>` 子目录整体迁移。
  - 每次迁移由用户为 Voxy 与 Bobby 分别选择处理方式：
    - 物理复制：迁移后用户将删除源缓存时使用。
    - Junction 指向源实例：源实例继续使用时使用，源与目标共用同一份缓存且不占额外磁盘空间；源实例被删除后链接失效。
    - Junction 指向共享目录：先把源缓存移动到用户选择的共享目录，再让源与目标都链接到该目录；适用于同一服务器反复更新整合包的场景，新版本整合包只需重新建立链接。共享目录下按种类分 `voxy/`、`bobby/` 子目录，避免同名服务器目录冲突；两种缓存共用用户选择的同一个共享目录。
  - Junction 通过 `cmd /c mklink /J` 创建，不需要管理员权限或开发者模式，也不引入新依赖。
  - 目标已存在同名缓存目录时，先重命名为 `<目录名>.<unix秒>.bak` 再复制或建立链接，禁止直接删除。
  - 游戏运行时会一直打开正在读写的缓存文件：复制或移动缓存前必须确认对应实例未在运行，否则中止并记录日志；共享同一份缓存的多个实例不能同时运行。判定方式为以不共享模式逐个打开缓存目录下的所有文件，任一文件打开失败即视为正在运行。
  - 源缓存本身已是指向其他位置的链接时，拒绝移动到共享目录，提示改用"Junction 指向源实例"，避免其他实例的链接失效。
  - 源缓存移动到共享目录后，若源实例的链接建立失败，必须把缓存移回原处。

### 2.6 固定配置 (Presets)
- 界面在"同步内容"上方提供固定配置按钮，点击后只填充界面选项，不直接执行迁移，用户仍可逐项调整。
- 每个固定配置都勾选"按键与选项"、"Mod 独立配置"与"地图与路点"，并把 Voxy 与 Bobby 设为同一种处理方式：

  | 名称 | 同步模式 | Voxy 与 Bobby 缓存 |
  | --- | --- | --- |
  | 洛林 | 局部合并 | 物理复制 |
  | neko喵喵喵 | 全量覆写 | Junction 指向共享目录（共享目录仍需用户选择） |

- 固定配置不修改材质包的处理方式；Axiom 蓝图的处理方式统一设为"不处理"；截图统一设为"Junction 指向共享目录"。

### 2.7 材质包同步 (`pack_sync.rs`)
- 目录：实例的 `resourcepacks/`，其中每个 `.zip` 文件或文件夹为一个材质包。光影包（`shaderpacks/`）不同步。
- Junction 只能链接文件夹，单个 zip 无法免权限跨分区链接，因此共享时以整个 `resourcepacks/` 目录为单位。
- 处理方式由用户选择：
  - 不处理。
  - 复制缺少的材质包：模板有而目标没有的直接复制；内容完全相同的跳过；同名但内容不同的交给用户决定。
  - Junction 指向共享目录：共享目录为 `<共享目录>\resourcepacks`，与 Voxy、Bobby 共用同一个用户选择的共享目录。先处理模板再处理目标：共享目录不存在时把该实例的整个 `resourcepacks/` 移入；已存在时逐个并入，共享目录缺少的移入，内容相同的跳过，同名但内容不同的交给用户决定；并入后实例目录中剩余的材质包整体重命名为 `resourcepacks.<unix秒>.bak` 保留（为空则删除），再建立 Junction。实例的 `resourcepacks/` 已链接到别处时拒绝合并。
- 同名冲突：比较文件或文件夹的全部字节，内容不同时弹窗列出每个冲突的名称，以及双方的来源（实例名或"共享目录"）、大小与修改距今天数，由用户逐个选择"保留现有"或"使用新版本"，默认保留现有。被替换的版本移入备份目录：复制模式为 `<目标游戏目录>\resourcepacks.bak\<名称>.<unix秒>`，共享模式为 `<共享目录>\resourcepacks.bak\<名称>.<unix秒>`；备份放在 `resourcepacks/` 之外，避免游戏把备份列为可选材质包。
- 移动或替换前按 §2.5 的逐文件独占打开方式检查占用。
- 启用列表：仅在共享模式下，把模板 `options.txt` 的 `resourcePacks` 与 `incompatibleResourcePacks` 写入目标（写入前备份）；只有共享目录能保证两个实例拥有相同的材质包文件。复制模式与全量覆写仍保留目标原有的启用列表。

### 2.8 Axiom 蓝图同步 (`pack_sync.rs`)
- 目录（已在两个 PCL 实例中实测）：`config/axiom/blueprints/`，每个蓝图为一个 `.bp` 文件；同目录下的 `.axiom_blueprint_ordering` 为纯文本，每行一个蓝图文件名，决定 Axiom 界面中的排列顺序。
- 仅当目标实例 `mods/` 中识别到 Mod ID `axiom` 时执行。复制、共享、同名冲突与备份规则与 §2.7 相同，处理对象只限 `.bp` 文件；共享目录为 `<共享目录>\axiom-blueprints`，复制模式备份到 `config/axiom/blueprints.bak/`，共享模式备份到 `<共享目录>\axiom-blueprints.bak\`，共享时目标原有的剩余文件整体重命名为 `blueprints.<unix秒>.bak`。
- 版本兼容：较新版本保存的蓝图在旧版本中无法加载，26.3（数据版本 5023）配合 Axiom 6.1.3 的蓝图放入 26.2 会直接崩溃。
  - 蓝图文件格式：4 字节魔数 `0A E5 BB 36`，随后依次为长度前缀（4 字节大端）的头部 NBT、缩略图、gzip 压缩的方块数据 NBT。头部的 `Version` 是 Axiom 自身的格式版本；游戏数据版本记录在方块数据的 `DataVersion` 整数标签中。
  - 实例的数据版本号取自其 `options.txt` 的 `version` 条目。
  - 复制模式：只放入 `DataVersion` 不超过目标实例数据版本号的蓝图；更新或无法读取版本的蓝图跳过并在日志中说明。目标缺少数据版本号时整个蓝图步骤失败并提示先启动一次游戏。
  - 共享模式：仅当模板与目标的数据版本号相同时允许执行，否则拒绝并提示改用复制模式。共享后若某个实例升级游戏版本，需先删除其蓝图链接。
  - 同名冲突弹窗额外显示双方蓝图的数据版本号。
- 排序文件：不参与冲突处理。复制或并入完成后，把新放入的蓝图名追加到目的目录（复制模式为目标实例，共享模式为共享目录）排序文件的末尾，已记录的名称不重复追加；追加顺序跟随模板排序文件，模板未记录的按名称排序。写入前按 §4 备份。

### 2.9 截图同步 (`pack_sync.rs`)
- 目录（已在 PCL 实例中实测）：`screenshots/`，原版与 Better Screenshots（Modrinth `better-mc-screenshots`）都直接读写该目录，Better Screenshots 配置中没有自定义截图路径的选项。只处理目录下的文件。
- 复制、共享、同名冲突与备份规则与 §2.7 相同；共享目录为 `<共享目录>\screenshots`，复制模式备份到 `screenshots.bak/`，共享模式备份到 `<共享目录>\screenshots.bak\`，共享时目标原有的剩余文件整体重命名为 `screenshots.<unix秒>.bak`。
- Junction 对游戏透明：链接后新截图直接写入共享目录，所有链接到同一共享目录的实例在游戏内画廊中都能看到；在任一实例中删除截图会同时影响其他实例。
- 界面默认处理方式为"Junction 指向共享目录"。

### 2.10 共享目录结构
- 默认位置为目标实例所属 `.minecraft` 下的 `bindlink-shared`，即迁移后继续使用的实例旁边；模板实例在其他磁盘时，首次共享会把内容复制到共享目录后删除原目录（`rename` 返回 `CrossesDevices` 时回退为复制，已在 C 盘与 F 盘之间实测）；目录在迁移确实用到时才由各同步步骤创建。用户可在界面中改选其他目录，或恢复默认；手动选择不跨次保存。
- Voxy、Bobby、材质包、Axiom 蓝图与截图共用同一个共享目录，按种类分子目录：

  ```
  bindlink-shared\
  ├─ voxy\<服务器地址>\
  ├─ bobby\<服务器地址>\
  ├─ resourcepacks\
  ├─ resourcepacks.bak\
  ├─ axiom-blueprints\
  ├─ axiom-blueprints.bak\
  ├─ screenshots\
  └─ screenshots.bak\
  ```

  `.bak` 子目录只在同名冲突选择"使用新版本"时出现。

### 2.11 迁移记录与回退 (`journal.rs`)
- 每次迁移在 `%APPDATA%\BindLink\history\<12 位 unix 秒>-<3 位序号>\` 下生成一份记录：`manifest.json` 按执行顺序登记所有改动，`files\` 保存被覆盖文件的迁移前版本（包括地图与路点文件）。迁移没有产生任何改动时删除该记录。
- 迁移代码中所有写入、复制、移动、改名、建立 Junction 与删除空目录都必须经由 `Journal` 执行，不得直接调用文件系统接口，否则回退会遗漏该改动。`.bak` 备份文件不登记，回退后保留。
- 登记的操作：写入文件（附迁移前版本）、新建目录（复制的缓存或文件夹材质包）、移动或改名、建立 Junction、删除空目录。迁移结束时记录每个文件与新建目录的文件数、总大小与最新修改时间，作为回退时判断"迁移后是否被改动"的依据。
- 回退范围：历史按时间全局排列，不区分目标实例，只能从最新一次开始逐次回退。共享目录可能被后续迁移的其他实例链接，先回退较早的迁移会使这些链接失效。
- 回退方式：按登记的逆序撤销。迁移后被游戏或用户改动过的文件或目录，先改名为 `<原名>.<unix秒>.rollback` 保留，再恢复迁移前版本或移除迁移新增的内容；不删除迁移后产生的任何内容。对 Junction 只删除链接本身。
- 回退中途失败（例如游戏仍在运行）时保存尚未撤销的部分，处理掉失败原因后可再次回退；全部撤销后删除该记录。
- 界面左侧"迁移历史"卡片列出尚未回退的记录，只有最新一条提供"回退"按钮，点击后需在确认窗口中再次确认。

---

## 3. 技术栈与模块设计

- **语言版本**：Rust
- **核心依赖**：
  - GUI：`eframe`, `rfd`
  - 格式解析：`serde`（`derive` 特性，读写迁移记录）, `serde_json`（开启 `preserve_order`，写回 JSON 时保持字段顺序）, `toml_edit`
  - IO / 压缩：`zip`, `walkdir`, `anyhow`, `flate2`（解压 Axiom 蓝图的 gzip 方块数据；已是 `zip` 的间接依赖，经用户批准改为直接依赖）, `image`（仅开启 `png` 特性，解码标题栏的 LorianTool 图标；已是 eframe 的间接依赖，经用户批准改为直接依赖）

### 文件模块组织
- `src/main.rs`: 界面逻辑、状态存储、事件处理与日志流展示。界面为左右分栏：左侧可滚动的分组卡片（实例、固定配置、按键与配置、地图与缓存、材质包与蓝图、按需出现的共享目录），右侧为按级别着色的日志，底部操作栏放置状态与"开始迁移"按钮；主题默认跟随系统，右上角可切换浅色或深色；主题按钮左侧为 GitHub 仓库（https://github.com/CL-matt/mc-bindsync）与 LorianTool（https://tools.voxisle.art/）链接按钮，LorianTool 图标取自 `logos/logo-square-LorianTool.png`，该图标需要授权，已加入 `.gitignore` 不随仓库分发；`build.rs` 检测到本地存在该文件时开启 `has_lorian_tool_logo` 并编译进程序，否则按钮只显示文字，两种情况都能正常编译；标题栏左侧显示软件标志与名称，窗口标题与任务栏图标同样使用 BindLink 标志。
- `src/engine.rs`: 核心业务逻辑（`options.txt` 解析、Mod 检测、配置读写与备份）。
- `src/launcher.rs`: 启动器路径嗅探、隔离版本扫描。
- `src/map_sync.rs`: 地图路点目录递归同步与 Voxy、Bobby 缓存链接或拷贝；同时提供 Junction、移动、占用检测等目录操作供 `pack_sync.rs` 复用。
- `src/pack_sync.rs`: 材质包、Axiom 蓝图与截图的复制、合并到共享目录、同名冲突处理，以及蓝图数据版本读取。
- `src/journal.rs`: 迁移记录与回退，迁移中所有改动文件系统的操作都经由其中的 `Journal` 执行。
- `src/log_file.rs`: 把界面日志同时写入 `%APPDATA%\BindLink\logs\bindlink-<毫秒时间戳>.log`，每次启动一个文件，保留最近 20 个；时间统一用 UTC；日志区"清空"按钮左侧的"打开日志文件夹"用资源管理器打开该目录。界面中的所有日志都必须经由 `add_logs` 进入，保证与文件一致。
- `src/fs_ops.rs`: 不经过迁移记录的文件系统基础操作（路径解析、占用检测、目录移动、Junction 创建），供 `journal.rs`、`map_sync.rs`、`pack_sync.rs` 使用。
- `src/types.rs`: 全局公用枚举与数据结构定义。

### 核心数据结构
- `DetectedInstance` 包含 `minecraft_root`（所属 `.minecraft` 根目录，决定默认共享目录位置；模板与目标可以来自不同根目录或不同磁盘）与 `launcher: LauncherKind` 字段，`LauncherKind` 取值为 `Pcl2 / Unknown`，定义在 `src/types.rs`。
- `ServerCache`（`Voxy / Bobby`）标识缓存种类，`CacheTransfer`（`Copy / LinkToSource / LinkToShared(共享目录)`）表示迁移方式；`MigrationRequest` 的 `voxy`、`bobby` 字段各为 `Option<CacheTransfer>`，`None` 表示不处理。
- `SharedAsset`（`ResourcePacks / AxiomBlueprints / Screenshots`）标识资源种类；`PackTransfer`（`CopyMissing / LinkToShared(共享目录)`）表示处理方式，对应 `MigrationRequest.resource_packs`、`MigrationRequest.axiom_blueprints` 与 `MigrationRequest.screenshots`；同名冲突以 `PackConflict`（种类、名称、`existing` 与 `incoming` 两个 `PackVersion`：来源、大小、修改时间、蓝图数据版本）交给界面，用户返回 `ConflictChoice`（`KeepExisting / UseIncoming`）。后台线程通过消息把冲突发给界面并阻塞等待回复。
- 业务模块通过 `LogSink` 输出 `LogEntry`（`LogLevel`：`Info / Skip / Success / Warning / Error`），界面据此着色：跳过的操作用 `Skip`，已写入、复制或链接用 `Success`，单个条目失败用 `Warning`，整个步骤失败用 `Error`。
- `src/test_support.rs` 仅在测试编译时引入，提供测试用临时目录。

### 开发环境
- 工具链：rustup `stable-x86_64-pc-windows-gnu`，安装在开发机自选的独立目录（通过用户级环境变量 `RUSTUP_HOME`、`CARGO_HOME` 指定）。
- `windows` 系列依赖以 raw-dylib 方式链接，需要完整 MinGW-w64 提供的 `dlltool` 与 `as`：使用 WinLibs（GCC 16.2.0，UCRT），与工具链放在同一目录下，其 `bin` 已加入用户 PATH。
- 项目位于网盘同步目录时，构建产物目录在本地 `.cargo/config.toml` 中指向同步范围之外；该文件含开发机路径，已加入 `.gitignore`。
- 发布版构建前，在同一份本地配置的 `[build] rustflags` 中用 `--remap-path-prefix` 把工具链目录与项目目录替换为固定名称，避免 exe 中带有本机路径与用户名；发布前用字符串搜索确认 exe 中不含本机目录。
- 文档、注释与提交信息中禁止写入开发机的真实目录、用户名或个人服务器地址，统一使用 `<游戏目录>`、`example.com` 等占位描述。

---

## 4. 健壮性与安全规范
- **自动备份**：任何写入操作前，必须先备份目标原始文件：
  - 目标旁不存在 `<文件>.bak` 时生成该文件，作为最早的原始备份永不覆盖。
  - 已存在 `<文件>.bak` 时生成 `<文件>.<unix秒>.bak`，同一文件仅保留最近 5 份带时间戳的备份。
  - 时间戳使用标准库 `std::time::SystemTime` 生成，不引入 `chrono`。
  - 例外：地图与路点目录（§2.5）文件数量大，豁免逐文件 `.bak` 备份；同步前必须在日志中列出将被覆盖的文件，被覆盖文件的迁移前版本保存在迁移记录中（§2.11）。Voxy 与 Bobby 缓存目录按 §2.5 整目录重命名备份；材质包、Axiom 蓝图与截图按 §2.7、§2.8、§2.9 移入同级 `.bak` 目录或整体重命名备份。
- **容错防崩**：遇到破损的 `.jar`、格式不规范的 JSON/TOML 时跳过并记录日志，不得触发 `panic!`。
- **无黑框分发**：发布模式需开启 `#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]`。

## 5. 开发约定
- 规则优先级：`CLAUDE.md` 与 `docs/` 规范 > 本文件 > 其余。本文件是项目架构、模块划分与同步策略的唯一事实来源；遇到未定义的业务逻辑，先说明变更点并取得确认再修改代码，涉及规则或数据结构的变更必须同步更新本文件。
- 技术栈：纯 Rust，使用 §3 列出的依赖，新增或更换依赖需经用户批准。
- 命名：类型使用 PascalCase，函数使用动词开头的 snake_case，名称直接表达职责（如 `scan_all_instances`、`detect_installed_mod_ids`、`apply_options_mapping`、`backup_file_safely`）。
- 错误处理：业务层统一返回 `anyhow::Result<T>`，禁止 `unwrap` / `expect`；IO 与解析异常记录日志并向上传递，不得触发 panic。
- 文件系统改动：迁移中的写入、复制、移动、改名、建立 Junction 与删除空目录一律经由 `Journal`（§2.11）；覆写配置文件前按 §4 生成 `.bak`。
- 交付说明：修改代码时指明涉及的模块文件。
- 不使用跨会话保留的自动文件式记忆；需要长期遵守的约定写入本文件或 `docs/`。
