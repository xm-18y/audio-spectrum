//! 开机自启动(陪伴应用可选项;「设置」菜单勾选)。
//!
//! 平台机制:Windows 写 `HKCU\...\CurrentVersion\Run` 注册表值;macOS 写
//! `~/Library/LaunchAgents` plist(RunAtLoad);Linux 写 XDG
//! `~/.config/autostart/*.desktop`。全部 best-effort:写入失败静默忽略
//! (零打扰原则),勾选态以持久化设置为准,启动时对开启态再落一次,
//! 修复 exe 移动/换装后的失效路径。

#[allow(unsafe_code)]
#[cfg(target_os = "windows")]
mod imp {
    use std::os::windows::ffi::OsStrExt;

    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_SZ, RegCloseKey, RegDeleteValueW,
        RegOpenKeyExW, RegSetValueExW,
    };
    use windows::core::w;

    /// Run 键里的值名(与应用同名,用户可在任务管理器「启动」页识别)。
    const VALUE_NAME: windows::core::PCWSTR = w!("audio-spectrum");
    const RUN_SUBKEY: windows::core::PCWSTR =
        w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");

    pub fn set_enabled(enable: bool) {
        // SAFETY: 标准 Win32 注册表调用;句柄成对开闭;数据缓冲区在调用期间存活。
        unsafe {
            let mut key = HKEY::default();
            if RegOpenKeyExW(HKEY_CURRENT_USER, RUN_SUBKEY, Some(0), KEY_SET_VALUE, &mut key)
                != ERROR_SUCCESS
            {
                return;
            }
            if enable {
                let Ok(exe) = std::env::current_exe() else {
                    let _ = RegCloseKey(key);
                    return;
                };
                // 引号包裹防空格;REG_SZ 需 UTF-16 带 NUL 终止的字节串。
                let quoted = format!("\"{}\"", exe.display());
                let wide: Vec<u16> = std::ffi::OsStr::new(&quoted)
                    .encode_wide()
                    .chain(std::iter::once(0))
                    .collect();
                let bytes = std::slice::from_raw_parts(
                    wide.as_ptr() as *const u8,
                    wide.len() * 2,
                );
                let _ = RegSetValueExW(key, VALUE_NAME, Some(0), REG_SZ, Some(bytes));
            } else {
                let _ = RegDeleteValueW(key, VALUE_NAME);
            }
            let _ = RegCloseKey(key);
        }
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::path::PathBuf;

    const LABEL: &str = "com.audio-spectrum";

    fn plist_path() -> Option<PathBuf> {
        dirs::home_dir().map(|h| h.join(format!("Library/LaunchAgents/{LABEL}.plist")))
    }

    /// plist 字符串段的 XML 转义(exe 路径理论上可含 `&` 等)。
    fn xml_escape(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }

    pub fn set_enabled(enable: bool) {
        let Some(path) = plist_path() else { return };
        if !enable {
            let _ = std::fs::remove_file(path);
            return;
        }
        let Ok(exe) = std::env::current_exe() else { return };
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
</dict>
</plist>
"#,
            xml_escape(&exe.display().to_string())
        );
        let _ = std::fs::write(path, plist);
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::path::PathBuf;

    fn desktop_path() -> Option<PathBuf> {
        dirs::config_dir().map(|c| c.join("autostart/audio-spectrum.desktop"))
    }

    pub fn set_enabled(enable: bool) {
        let Some(path) = desktop_path() else { return };
        if !enable {
            let _ = std::fs::remove_file(path);
            return;
        }
        let Ok(exe) = std::env::current_exe() else { return };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let entry = format!(
            "[Desktop Entry]\nType=Application\nName=audio-spectrum\nExec=\"{}\"\nX-GNOME-Autostart-enabled=true\n",
            exe.display()
        );
        let _ = std::fs::write(path, entry);
    }
}

/// 其他平台(Android 等)无自启动概念:空操作。
#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
mod imp {
    pub fn set_enabled(_enable: bool) {}
}

/// 写入/移除系统级自启动项;best-effort,失败静默。
pub fn set_enabled(enable: bool) {
    imp::set_enabled(enable);
}
