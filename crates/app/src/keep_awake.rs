//! 阻止系统休眠(「设置」菜单勾选项;语义 = 防 idle 睡眠,不含显示器常亮)。
//!
//! 平台机制:Windows `SetThreadExecutionState`(线程级,进程退出即失效);
//! macOS IOKit `IOPMAssertionCreateWithName`(进程级断言,退出自动释放);
//! Linux `systemd-inhibit … sleep infinity` 子进程(关闭时 kill;崩溃会
//! 残留至其退出,正常退出路径由 main 负责复位)。全部 best-effort 静默。

#[allow(unsafe_code)]
#[cfg(target_os = "windows")]
mod imp {
    use windows::Win32::System::Power::{
        ES_CONTINUOUS, ES_SYSTEM_REQUIRED, SetThreadExecutionState,
    };

    pub fn set_enabled(enable: bool) {
        // SAFETY: 标准 Win32 调用,无内存参予;返回值(上一状态)无需使用。
        unsafe {
            let flags = if enable {
                ES_CONTINUOUS | ES_SYSTEM_REQUIRED
            } else {
                ES_CONTINUOUS
            };
            let _ = SetThreadExecutionState(flags);
        }
    }
}

#[allow(unsafe_code)]
#[cfg(target_os = "macos")]
mod imp {
    use std::sync::Mutex;

    use objc2_foundation::NSString;

    type CFStringRef = *const core::ffi::c_void;
    /// IOPMAssertionID;0 = 无效。
    type AssertionId = u32;
    /// kIOPMAssertionLevelOn。
    const ASSERT_LEVEL_ON: u32 = 255;
    /// KERN_SUCCESS。
    const KERN_SUCCESS: i32 = 0;

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOPMAssertionCreateWithName(
            assert_type: CFStringRef,
            assert_level: u32,
            assert_name: CFStringRef,
            assert_id: *mut AssertionId,
        ) -> i32;
        fn IOPMAssertionRelease(assert_id: AssertionId);
    }

    /// 持有中的断言(释放用);进程退出时 IOKit 自动回收。
    static ASSERTION: Mutex<Option<AssertionId>> = Mutex::new(None);

    pub fn set_enabled(enable: bool) {
        let Ok(mut guard) = ASSERTION.lock() else { return };
        // SAFETY: 标准 IOKit 调用;NSString 与 CFString 免桥接(toll-free),
        // 指针在调用期间存活;断言 id 由 out 参数返回。
        unsafe {
            if enable {
                if guard.is_some() {
                    return;
                }
                let ty = NSString::from_str("PreventUserIdleSystemSleep");
                let name = NSString::from_str("audio-spectrum 阻止休眠");
                let mut id: AssertionId = 0;
                let kr = IOPMAssertionCreateWithName(
                    &*ty as *const NSString as CFStringRef,
                    ASSERT_LEVEL_ON,
                    &*name as *const NSString as CFStringRef,
                    &mut id,
                );
                if kr == KERN_SUCCESS {
                    *guard = Some(id);
                }
            } else if let Some(id) = guard.take() {
                IOPMAssertionRelease(id);
            }
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::process::{Child, Command, Stdio};
    use std::sync::Mutex;

    /// 运行中的 systemd-inhibit 子进程(kill 即释放抑制锁)。
    static CHILD: Mutex<Option<Child>> = Mutex::new(None);

    pub fn set_enabled(enable: bool) {
        let Ok(mut guard) = CHILD.lock() else { return };
        if enable {
            if guard.is_some() {
                return;
            }
            let spawned = Command::new("systemd-inhibit")
                .args([
                    "--what=sleep",
                    "--who=audio-spectrum",
                    "--why=用户开启阻止休眠",
                    "--mode=block",
                    "sleep",
                    "infinity",
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
            if let Ok(child) = spawned {
                *guard = Some(child);
            }
        } else if let Some(mut child) = guard.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// 其他平台(Android 等):空操作。
#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
mod imp {
    pub fn set_enabled(_enable: bool) {}
}

/// 开/关系统休眠抑制;best-effort,失败静默。
pub fn set_enabled(enable: bool) {
    imp::set_enabled(enable);
}
