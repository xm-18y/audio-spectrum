//! Windows 资源嵌入:程序文件图标(资源管理器/任务栏显示)与基础
//! 版本元数据。仅 Windows 目标生效;MSVC 工具链用 Windows SDK 的
//! rc.exe(winresource 自动定位,GitHub windows-latest 自带),GNU
//! 工具链用 windres。

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    println!("cargo:rerun-if-changed=../../assets/icons/audio-spectrum.ico");

    let mut res = winresource::WindowsResource::new();
    res.set_icon("../../assets/icons/audio-spectrum.ico");
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    res.set("FileDescription", "audio-spectrum");
    res.set("ProductName", "audio-spectrum");
    res.set("FileVersion", &version);
    res.set("ProductVersion", &version);
    res.compile().expect("嵌入 Windows 图标资源失败");
}
