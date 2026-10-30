# 待办事项

以下功能已从当前版本移除，后续可考虑重新实现。实现前必须先用真实安装核实文件格式，并同步更新 `mem.md`。

## HMCL 适配

- 启动器标注：版本目录下存在 `hmclversion.cfg` 时标注为 HMCL，需在 `LauncherKind` 中恢复 `Hmcl` 取值。
- 扫描源：读取 `%APPDATA%\.hmcl\hmcl.json` 与 HMCL 同目录下的 `hmcl.json` 中登记的游戏目录。
- 阻塞原因：开发机未安装 HMCL，无法核实 `hmcl.json` 结构与 `hmclversion.cfg` 的存放位置。

## JourneyMap 地图与路点同步

- 预期目录：`journeymap/data/sp/<世界>/` 与 `journeymap/data/mp/<服务器>/`，按世界键做与 Xaero 相同的无损递归同步。
- 阻塞原因：现有实例均未安装 JourneyMap，目录结构未经实测。

## PCL 启动器配置作为扫描源

- 已核实：
  - PCL 每个安装目录下的 `PCL\Setup.ini` 记录 `LaunchFolderSelect:$.minecraft\`，`$` 表示相对 PCL 程序所在目录。这个目录已被"程序工作目录及其父级的 `.minecraft`"扫描源覆盖（程序与 PCL 放在一起时）。
  - 注册表 `HKCU\Software\PCL` 的 `LaunchFolders` 由同一用户的所有 PCL 安装共用，开发机上该项为空，`<PCL 安装目录>` 与 `<另一 PCL 安装目录>` 两个安装都没有使用它。
- 待办：在 PCL 中手动添加一个游戏文件夹后核实 `LaunchFolders` 的取值格式，再决定是否读取注册表（读取注册表需调用 `reg query`，不引入新依赖）。
