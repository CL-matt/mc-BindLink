use std::path::Path;

/// LorianTool 图标需要授权，不随仓库分发。本地放有该文件时才开启 `has_lorian_tool_logo`，
/// 使没有授权文件的环境也能正常编译。
const LORIAN_TOOL_LOGO: &str = "logos/logo-square-LorianTool.png";

fn main() {
    println!("cargo::rustc-check-cfg=cfg(has_lorian_tool_logo)");
    // 监视整个目录，图标文件新增或删除后都会重新检测
    println!("cargo::rerun-if-changed=logos");
    if Path::new(LORIAN_TOOL_LOGO).is_file() {
        println!("cargo::rustc-cfg=has_lorian_tool_logo");
    }
}
