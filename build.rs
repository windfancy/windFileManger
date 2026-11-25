//! 构建脚本：编译 Slint UI + 把 `assets/app.ico` 编进 Windows 可执行文件
//! 的图标资源（`RT_GROUP_ICON`），资源管理器/任务栏显示程序图标而非白板。
//!
//! 本机环境的坑（与 windPc86Box / windMusic 同源）：
//! * VS2022 精简安装没有 `vcvars64.bat`，`rc.exe` 不在 PATH；
//! * `winresource` 默认靠 `reg.exe` 查注册表定位 SDK，沙箱把 `reg.exe` 拉黑；
//! * 因此自己扫 SDK 目录挑版本号最高的 `rc.exe`，用 `RC_PATH` 直接告知，
//!   且 `RC_PATH` 必须在 `WindowsResource::new()` **之前**设置（new() 内部
//!   就会定位工具链）。`INCLUDE` 未设时补齐 ucrt/shared/um。
//!
//! 本 crate 是 edition 2024，`std::env::set_var` 是 unsafe。

fn main() {
    slint_build::compile("ui/app.slint").expect("slint compile failed");

    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if os != "windows" || env != "msvc" {
        return; // 非 MSVC 目标没有 PE 资源一说
    }

    println!("cargo:rerun-if-changed=assets/app.ico");
    println!("cargo:rerun-if-changed=build.rs");

    ensure_include();

    let Some(rc) = find_rc() else {
        println!(
            "cargo:warning=未找到 Windows SDK 的 rc.exe，跳过图标资源编译（exe 将显示系统默认图标）"
        );
        return;
    };
    // edition 2024：set_var 是 unsafe
    unsafe { std::env::set_var("RC_PATH", &rc) };

    // 必须在设定 RC_PATH 之后构造：new() 里就会用 get_sdk() 定工具链。
    let mut res = winresource::WindowsResource::new();
    res.set_toolkit_path(
        &rc.parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default(),
    );
    res.add_toolkit_include(true); // 让 rc.exe 能找到 windows.h

    res.set_icon("assets/app.ico");
    res.set("ProductName", "windFileManger");
    res.set("FileDescription", "windFileManger —— 平铺式导航的轻量文件管理器");
    res.set("OriginalFilename", "windFileManger.exe");
    res.set_language(0x0804); // 简体中文
    if let Err(e) = res.compile() {
        println!("cargo:warning=未能编译 Windows 图标资源（程序仍可正常运行，仅 exe 文件图标缺失）：{e}");
    }
}

/// 在 Windows SDK 的 bin 目录里挑版本号最高的 `rc.exe`（架构跟随编译目标）。
fn find_rc() -> Option<std::path::PathBuf> {
    let root = std::path::Path::new(r"C:\Program Files (x86)\Windows Kits\10\bin");
    let arch = match std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default().as_str() {
        "x86" => "x86",
        "aarch64" => "arm64",
        _ => "x64",
    };
    let mut best: Option<(Vec<u32>, std::path::PathBuf)> = None;
    for entry in std::fs::read_dir(root).ok()?.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let name = dir.file_name()?.to_string_lossy().to_string();
        // 只要形如 10.0.22621.0 的版本目录
        let parts: Vec<u32> = name.split('.').filter_map(|p| p.parse().ok()).collect();
        if parts.len() < 3 {
            continue;
        }
        let cand = dir.join(arch).join("rc.exe");
        if !cand.exists() {
            continue;
        }
        if best.as_ref().map(|(v, _)| &parts > v).unwrap_or(true) {
            best = Some((parts, cand));
        }
    }
    best.map(|(_, p)| p)
}

/// `INCLUDE` 未设置时，按版本号最高的 SDK 补上 ucrt / shared / um 三个 include 目录。
fn ensure_include() {
    if std::env::var_os("INCLUDE").is_some() {
        return;
    }
    let root = std::path::Path::new(r"C:\Program Files (x86)\Windows Kits\10\Include");
    let mut best: Option<(Vec<u32>, std::path::PathBuf)> = None;
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let Some(name) = dir.file_name() else {
            continue;
        };
        let name = name.to_string_lossy().to_string();
        let parts: Vec<u32> = name.split('.').filter_map(|p| p.parse().ok()).collect();
        if parts.len() < 3 {
            continue;
        }
        if best.as_ref().map(|(v, _)| &parts > v).unwrap_or(true) {
            best = Some((parts, dir));
        }
    }
    let Some((_, dir)) = best else {
        return;
    };
    let include = [dir.join("ucrt"), dir.join("shared"), dir.join("um")]
        .iter()
        .filter(|p| p.is_dir())
        .map(|p| p.to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join(";");
    if !include.is_empty() {
        // 派生 rc.exe 的子进程会继承它。edition 2024 下 set_var 是 unsafe。
        unsafe { std::env::set_var("INCLUDE", include) };
    }
}
