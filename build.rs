fn main() {
    println!("cargo:rerun-if-changed=assets/app-icon.ico");

    // 构建脚本在主机构上运行，#[cfg(target_os)] 反映的是主机而非目标；
    // 交叉编译（如 Windows 主机构建 macOS 包）时不能用主机的 cfg 决定
    // 是否嵌图标，这里按目标平台的环境变量判断。
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set_icon("assets/app-icon.ico")
            .compile()
            .expect("无法把应用图标写入 Windows 可执行文件");
    }
}
