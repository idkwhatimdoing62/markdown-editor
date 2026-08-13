fn main() {
    println!("cargo:rerun-if-changed=assets/app-icon.ico");

    #[cfg(target_os = "windows")]
    winresource::WindowsResource::new()
        .set_icon("assets/app-icon.ico")
        .compile()
        .expect("无法把应用图标写入 Windows 可执行文件");
}
