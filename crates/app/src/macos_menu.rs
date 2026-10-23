//! macOS 原生菜单栏(AppKit `NSMenu`,v0.4.0 设计:顶栏移除后,配置
//! 的常驻入口回归系统菜单;颜色/滑杆类控件进独立设置窗)。
//!
//! 结构:单个 `MenuHandler`(NSObject 子类)承接全部菜单动作,按 tag
//! 分发为 [`MenuCommand`],由 UI 线程每帧 `poll`。声源子菜单随设备
//! 列表重建。勾选态由 [`NativeMenu::sync`] 每帧刷新。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use objc2::rc::Retained;
use objc2::runtime::NSObjectProtocol;
use objc2::{AnyThread, DefinedClass, MainThreadMarker, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSControlStateValueOff, NSControlStateValueOn, NSEventModifierFlags, NSMenu,
    NSMenuItem,
};
use objc2_foundation::NSObject;
use objc2_foundation::NSString;

use audio_spectrum_capture::DeviceInfo;

use crate::ViewMode;

/// 菜单栏发往 UI 线程的命令。
#[derive(Debug)]
pub enum MenuCommand {
    OpenSettings,
    SetView(ViewMode),
    ToggleFullscreen,
    ToggleAlwaysOnTop,
    ToggleFrozen,
    ToggleHud,
    SelectSource(DeviceInfo),
}

#[derive(Default)]
struct MenuShared {
    commands: VecDeque<MenuCommand>,
    /// 声源子菜单 tag(TAG_SOURCE + index)对应的设备快照。
    sources: Vec<DeviceInfo>,
}

type Shared = Arc<Mutex<MenuShared>>;

// tag 分配:1..10 固定项;≥1000 为声源索引。
const TAG_SETTINGS: isize = 1;
const TAG_VIEW_SPECTRUM: isize = 2;
const TAG_VIEW_SPECTROGRAM: isize = 3;
const TAG_FULLSCREEN: isize = 4;
const TAG_PIN: isize = 5;
const TAG_FREEZE: isize = 6;
const TAG_HUD: isize = 7;
const TAG_SOURCE: isize = 1000;

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "AudioSpectrumMenuHandler"]
    #[ivars = Shared]
    struct MenuHandler;

    unsafe impl NSObjectProtocol for MenuHandler {}

    impl MenuHandler {
        /// 全部菜单动作的统一入口:按 tag 分发。
        #[unsafe(method(handleMenu:))]
        #[allow(non_snake_case)]
        unsafe fn handleMenu(&self, sender: &NSMenuItem) {
            let tag = sender.tag();
            let Ok(mut guard) = self.ivars().lock() else { return };
            let command = match tag {
                TAG_SETTINGS => Some(MenuCommand::OpenSettings),
                TAG_VIEW_SPECTRUM => Some(MenuCommand::SetView(ViewMode::Spectrum)),
                TAG_VIEW_SPECTROGRAM => Some(MenuCommand::SetView(ViewMode::Spectrogram)),
                TAG_FULLSCREEN => Some(MenuCommand::ToggleFullscreen),
                TAG_PIN => Some(MenuCommand::ToggleAlwaysOnTop),
                TAG_FREEZE => Some(MenuCommand::ToggleFrozen),
                TAG_HUD => Some(MenuCommand::ToggleHud),
                n if n >= TAG_SOURCE => guard
                    .sources
                    .get((n - TAG_SOURCE) as usize)
                    .cloned()
                    .map(MenuCommand::SelectSource),
                _ => None,
            };
            if let Some(c) = command {
                guard.commands.push_back(c);
            }
        }
    }
);

impl MenuHandler {
    fn new(shared: Shared) -> Retained<Self> {
        let this = Self::alloc().set_ivars(shared);
        unsafe { msg_send![super(this), init] }
    }
}

fn ns_str(s: &str) -> Retained<NSString> {
    NSString::from_str(s)
}

/// 带命令 tag 的菜单项(handler 为动作目标)。
fn command_item(
    mtm: MainThreadMarker,
    title: &str,
    tag: isize,
    key: Option<&str>,
    handler: &MenuHandler,
) -> Retained<NSMenuItem> {
    let item = NSMenuItem::new(mtm);
    item.setTitle(&ns_str(title));
    item.setTag(tag);
    if let Some(key) = key {
        item.setKeyEquivalent(&ns_str(key));
        item.setKeyEquivalentModifierMask(NSEventModifierFlags::Command);
    }
    // SAFETY: handler 生命周期由 NativeMenu 持有;选择器在本类定义。
    unsafe {
        item.setAction(Some(sel!(handleMenu:)));
        item.setTarget(Some(handler.as_ref()));
    }
    item
}

/// 标准动作项(目标为 nil,走响应链:terminate:/hide: 等)。
fn standard_item(
    mtm: MainThreadMarker,
    title: &str,
    action: Sel,
    key: Option<&str>,
) -> Retained<NSMenuItem> {
    let item = NSMenuItem::new(mtm);
    item.setTitle(&ns_str(title));
    if let Some(key) = key {
        item.setKeyEquivalent(&ns_str(key));
        item.setKeyEquivalentModifierMask(NSEventModifierFlags::Command);
    }
    // SAFETY: 标准 AppKit 选择器。
    unsafe { item.setAction(Some(action)) }
    item
}

use objc2::runtime::Sel;
use std::convert::AsRef;

/// 勾选态可同步的菜单项集合。
struct MenuItems {
    view_spectrum: Retained<NSMenuItem>,
    view_spectrogram: Retained<NSMenuItem>,
    fullscreen: Retained<NSMenuItem>,
    pin: Retained<NSMenuItem>,
    freeze: Retained<NSMenuItem>,
    hud: Retained<NSMenuItem>,
}

/// 已安装的原生菜单;持有 handler 与需要同步状态的项。
pub struct NativeMenu {
    handler: Retained<MenuHandler>,
    shared: Shared,
    items: MenuItems,
    source_menu: Retained<NSMenu>,
    source_signature: String,
}

impl NativeMenu {
    /// 构建并安装主菜单;仅可在主线程调用(首帧 ui() 内)。
    pub fn install() -> Option<NativeMenu> {
        let mtm = MainThreadMarker::new()?;
        let shared: Shared = Arc::new(Mutex::new(MenuShared::default()));
        let handler = MenuHandler::new(shared.clone());

        let view_spectrum = command_item(mtm, "频谱", TAG_VIEW_SPECTRUM, Some("1"), &handler);
        let view_spectrogram = command_item(mtm, "语谱", TAG_VIEW_SPECTROGRAM, Some("2"), &handler);
        let fullscreen = command_item(mtm, "全屏", TAG_FULLSCREEN, Some("f"), &handler);
        let pin = command_item(mtm, "窗口置顶", TAG_PIN, None, &handler);
        let freeze = command_item(mtm, "冻结显示", TAG_FREEZE, None, &handler);
        let hud = command_item(mtm, "显示性能信息", TAG_HUD, None, &handler);

        // 视图菜单
        let view_menu = NSMenu::new(mtm);
        view_menu.addItem(&view_spectrum);
        view_menu.addItem(&view_spectrogram);
        view_menu.addItem(&NSMenuItem::separatorItem(mtm));
        view_menu.addItem(&fullscreen);
        view_menu.addItem(&pin);
        view_menu.addItem(&freeze);
        view_menu.addItem(&hud);
        let view_item = NSMenuItem::new(mtm);
        view_item.setTitle(&ns_str("视图"));
        view_item.setSubmenu(Some(&view_menu));

        // 声源菜单(内容随设备列表重建)
        let source_menu = NSMenu::new(mtm);
        let placeholder = NSMenuItem::new(mtm);
        placeholder.setTitle(&ns_str("扫描设备…"));
        placeholder.setEnabled(false);
        source_menu.addItem(&placeholder);
        let source_item = NSMenuItem::new(mtm);
        source_item.setTitle(&ns_str("声源"));
        source_item.setSubmenu(Some(&source_menu));

        // 应用菜单(第一项,含标准动作)
        let app_menu = NSMenu::new(mtm);
        app_menu.addItem(&standard_item(
            mtm,
            "关于 audio-spectrum",
            sel!(orderFrontStandardAboutPanel:),
            None,
        ));
        app_menu.addItem(&NSMenuItem::separatorItem(mtm));
        app_menu.addItem(&command_item(
            mtm,
            "设置…",
            TAG_SETTINGS,
            Some(","),
            &handler,
        ));
        app_menu.addItem(&NSMenuItem::separatorItem(mtm));
        app_menu.addItem(&standard_item(
            mtm,
            "隐藏 audio-spectrum",
            sel!(hide:),
            Some("h"),
        ));
        app_menu.addItem(&standard_item(
            mtm,
            "隐藏其他",
            sel!(hideOtherApplications:),
            Some("h"),
        ));
        app_menu.addItem(&NSMenuItem::separatorItem(mtm));
        app_menu.addItem(&standard_item(
            mtm,
            "退出 audio-spectrum",
            sel!(terminate:),
            Some("q"),
        ));
        // 「隐藏其他」需要 Option 修饰。
        // SAFETY: AppKit API。
        let hide_others = app_menu.itemArray().objectAtIndex(4);
        hide_others.setKeyEquivalentModifierMask(
            NSEventModifierFlags::Command.union(NSEventModifierFlags::Option),
        );
        let app_item = NSMenuItem::new(mtm);
        app_item.setTitle(&ns_str("audio-spectrum"));
        app_item.setSubmenu(Some(&app_menu));

        // 主菜单
        let main_menu = NSMenu::new(mtm);
        main_menu.addItem(&app_item);
        main_menu.addItem(&view_item);
        main_menu.addItem(&source_item);

        let app = NSApplication::sharedApplication(mtm);
        app.setMainMenu(Some(&main_menu));

        Some(NativeMenu {
            handler,
            shared,
            items: MenuItems {
                view_spectrum,
                view_spectrogram,
                fullscreen,
                pin,
                freeze,
                hud,
            },
            source_menu,
            source_signature: String::new(),
        })
    }

    /// 取走自上次轮询以来的全部菜单命令。
    pub fn poll(&self) -> Vec<MenuCommand> {
        match self.shared.lock() {
            Ok(mut guard) => guard.commands.drain(..).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// 刷新勾选态(每帧调用,开销可忽略)。
    #[allow(clippy::too_many_arguments)]
    pub fn sync(&self, view: ViewMode, fullscreen: bool, pin: bool, frozen: bool, hud: bool) {
        let on = NSControlStateValueOn;
        let off = NSControlStateValueOff;
        self.items
            .view_spectrum
            .setState(if view == ViewMode::Spectrum { on } else { off });
        self.items
            .view_spectrogram
            .setState(if view == ViewMode::Spectrogram {
                on
            } else {
                off
            });
        self.items
            .fullscreen
            .setState(if fullscreen { on } else { off });
        self.items.pin.setState(if pin { on } else { off });
        self.items.freeze.setState(if frozen { on } else { off });
        self.items.hud.setState(if hud { on } else { off });
    }

    /// 设备列表或当前声源变化时重建声源子菜单(签名去抖)。
    pub fn rebuild_sources(
        &mut self,
        loopback: &[DeviceInfo],
        input: &[DeviceInfo],
        current_id: Option<&str>,
    ) {
        let signature = format!("{loopback:?}|{input:?}|{}", current_id.unwrap_or(""));
        if signature == self.source_signature {
            return;
        }
        self.source_signature = signature;
        self.source_menu.removeAllItems();
        let Ok(mut guard) = self.shared.lock() else {
            return;
        };
        guard.sources.clear();

        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        for (devices, label) in [(loopback, "Loopback(系统回环)"), (input, "Input(输入)")] {
            if devices.is_empty() {
                continue;
            }
            let section = NSMenuItem::new(mtm);
            section.setTitle(&ns_str(label));
            section.setEnabled(false);
            self.source_menu.addItem(&section);
            for device in devices {
                let tag = TAG_SOURCE + guard.sources.len() as isize;
                let item = NSMenuItem::new(mtm);
                item.setTitle(&ns_str(&device.name));
                item.setTag(tag);
                // SAFETY: 同 command_item。
                unsafe {
                    item.setAction(Some(sel!(handleMenu:)));
                    item.setTarget(Some(self.handler.as_ref()));
                }
                if current_id == Some(device.id.as_str()) {
                    item.setState(NSControlStateValueOn);
                }
                self.source_menu.addItem(&item);
                guard.sources.push(device.clone());
            }
        }
        if guard.sources.is_empty() {
            let empty = NSMenuItem::new(mtm);
            empty.setTitle(&ns_str("未发现设备"));
            empty.setEnabled(false);
            self.source_menu.addItem(&empty);
        }
    }
}
