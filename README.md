# audio-spectrum

跨平台(Windows / macOS / Linux)实时音频频谱显示应用。领域术语见
[GLOSSARY.md](GLOSSARY.md),架构决策见 [docs/adr/](docs/adr/)。

## 运行

```bash
cargo run -p audio-spectrum-app   # macOS 首次运行会请求麦克风权限
# 快捷键:S = 频谱⇄语谱,Space = 冻结/解冻,⌘, = 设置,
# F11 = 全屏(仅 Windows/Linux;macOS 用 ⌘F),双击 = 全屏⇄窗口,ESC 关窗/退全屏
# 配置入口:原生菜单栏(macOS)/ 应用菜单栏(Windows/Linux);设置存于
#   macOS ~/Library/Application Support/audio-spectrum/settings.ron
#   Windows %APPDATA%\audio-spectrum\settings.ron · Linux ~/.config/audio-spectrum/
scripts/bundle-macos.sh       # 打包 .app(Dock/启动台图标 + Info.plist + 麦克风用途声明)
open target/release/audio-spectrum.app
cargo test                    # core 管线 9 个行为测试 + 性能预算断言 + SCK 实机集成
cargo clippy --workspace --all-targets
```

窗口图标(标题栏/任务栏)由 256px PNG 经 `include_bytes!` 编进二进制;
Windows 程序文件图标(资源管理器)经 `build.rs` + `winresource` 嵌入
多尺寸 `assets/icons/audio-spectrum.ico`(16–256px,构建期自动编译,
同时写入 FileDescription/ProductVersion 元数据);macOS Dock/启动台
图标由 bundle 脚本装配(`assets/audio-spectrum.icns`)。
Windows/Linux 构建说明:Linux 需系统音频开发库 `libasound2-dev`(+ `pkg-config`);
Windows 交叉检查可用 `rustup target add x86_64-pc-windows-msvc` +
`cargo check --target x86_64-pc-windows-msvc -p audio-spectrum-capture`。

## 架构

```
crates/
├── core      纯函数频谱管线：samples → FFT(2048/realfft/Hann)→ Bin → Band 聚合
│             (对数轴 20Hz–20kHz，取 max + 低频插值)→ dBFS；Smoother(快攻慢放
│             + Peak Hold)。无 I/O、无 FFI，全部行为测试覆盖。
├── capture   CaptureBackend：cpal 输入采集(三平台)+ 音频线程回调(只写环缓冲，
│             ADR-0002)→ 归一化(f32 / 48kHz / 单声道)→ ringbuf SPSC。
└── app       iced 壳(Elm 架构)：声源选择、断线自动 Reconnect、深色频谱画布
             (canvas：渐变柱 + Peak Hold 线 + 频率/dB 网格)、常驻 60fps 重绘
             (画布在每次重绘请求时发布 Frame 消息，自维持 vsync 帧循环)。
```

线程模型(ADR-0002):音频回调只写环缓冲,不分配不加锁;FFT 在 UI 线程每帧执行
(2048 点实 FFT 微秒级)。窗口在后台时音频继续写入,溢出丢弃(仅影响显示)。

## 状态

- ✅ v0.6.0 应用菜单栏 + Windows Loopback 修复:非 macOS 取消右键菜单,
  改为窗口顶部常驻应用菜单栏;视图菜单聚合视图/全屏/窗口(启动全屏、
  置顶、记忆)/频段数(32–192 六档)/衰减速度(极慢—极快五档),
  设置窗只剩 Peak Hold、网格、样式、背景图;左下状态浮层移除;绘图区
  全出血铺满窗体(频率刻度内嵌底缘);双击主体内容切换全屏;修复背景图
  遮挡频谱/语谱(iced canvas 同帧内图像
  恒绘制在几何之上:背景图/语谱纹理拆为独立 canvas widget 层,经
  stack 分层叠放,频谱主体在最上);Windows 后端修复两处导致 Loopback 永远不可用的缺陷
  (枚举器 CLSID 写错 → REGDB_E_CLASSNOTREG;流启动后无就绪回报 →
  打开必然超时)
- ✅ v0.4.0 原生菜单与全幅画布:macOS 原生菜单栏(AppKit NSMenu:应用
  菜单/视图/动态声源,⌘, 设置);顶栏移除,频谱铺满窗体,状态降为
  左下浮层(HUD 右下,默认关);设置改为独立滚动窗口
- ✅ v0.3.0 个性化中枢:设置持久化(ron,`dirs` 平台惯例目录,300ms 防抖,
  损坏静默回默认)——样式主题(5 预设 + 逐槽颜色覆盖/圆角/间隙/网格)、
  背景图(选择/缩放 填满·适应·平铺/不透明度/预烘焙模糊)、窗口(默认
  全屏、尺寸位置记忆);频谱占满窗体,配置全部收进 ⚙ 菜单;全屏时
  顶栏自动隐藏(贴顶 8px 浮现),ESC/右键退全屏
- ✅ v0.2.0 功能纵深(陪伴式视觉化,ADR-0003):
  - **语谱图**(Spectrogram):12s 滚动历史、20ms 列聚合(窗口最大值)、
    复用 96 Band 对数轴、纹理单 quad 渲染;`S` 键或顶栏按钮切换
  - **检视交互**:悬停显示 `频率 · dB`(光标跟随);`Space` 冻结画面
  - **极简三件套设置**:频段数 64/96、Peak Hold 开关、衰减速度(慢/中/快)
  - **窗口置顶** toggle(陪伴场景钉在角落)
- ✅ 输入采集(麦克风/线路输入):三平台,含设备选择与断线重连
- ✅ Loopback(系统回环):三平台后端已实现
  - macOS:ScreenCaptureKit,实机验证(集成测试:`cargo test -p audio-spectrum-capture`)
  - Windows:WASAPI loopback,交叉编译验证(x86_64-pc-windows-msvc,待实机)
  - Linux:PulseAudio monitor(PipeWire 经兼容层),源码级验证(待 Linux 机器编译)
- ✅ 频谱管线 + 平滑 + Peak Hold + 语谱历史 + 设置中枢:22 个测试全绿,clippy 零警告
- ✅ 性能预算:已仪表化
  - 自动断言:`cargo test -p audio-spectrum-core --test perf`(实测 release 25.6µs/帧,
    帧预算 16.7ms 的 0.15%,验证 ADR-0002 的微秒级假设)
  - 运行时 HUD:顶栏实时显示「ms/帧 · fps · ≈% CPU」(帧耗时 EMA ≈ 60 帧)
  - 端到端延迟 < 50ms:受采集后端影响,需实机人工验收(轮询后端 10ms 周期)
    
## CI / 下载

GitHub Actions(`.github/workflows/build.yml`)三平台构建:

- **触发**:每次 push + 手动 `workflow_dispatch`
- **矩阵**:macOS arm64(`.app` zip)、Windows x64(zip)、Linux x64(tar.gz),
  含 `cargo test` 与性能预算断言(CI 无音频硬件时自动跳过 SCK 实机集成测试)
- **下载链接**:main 分支/手动触发 → 滚动 prerelease `latest`(链接永久稳定);
  推送 `v*` tag → 该 tag 的正式 Release;
  其他分支 → run 级 Artifacts
- 首次接入:
  ```bash
  git remote add origin git@github.com:<你>/audio-spectrum.git
  git push -u origin main
  ```

## 已知取舍

- 设备身份:macOS 用 SCK 伪设备“系统音频输出”;Windows 用 WASAPI 端点 ID;
  Linux 用 PulseAudio monitor source 名;设备列表每秒轮询刷新。
- macOS 26 的 SCStreamConfiguration 已无 `capturesCurrentAudioOutput`(13–15 的
  API),实现改用 display 范围 filter + `capturesAudio`(详见
  `crates/capture/src/macos.rs` 注释)。
- CoreMedia 的 AudioBufferList 提取要求两段式精确尺寸查询(SCK 怪癖),
  同样记录在 macos.rs 注释里。
- WASAPI/PulseAudio 后端在专用线程上 10ms 轮询取包(与 SCK 回调等价的
  “只写环缓冲”不变量);COM/Pulse 锁均隔离在采集线程内。
- Smoother 系数按 60fps 标定，高刷新率屏幕上衰减会略快(纯视觉，可接受)。
- 文本渲染用 iced/cosmic-text 的系统字体库：主字体指平台 CJK 家族
  (微软雅黑/PingFang/Noto CJK)并注册候选字体字节作字形回退后备，找不到时
  逐字形回退，中文不会变方框(与旧 egui 壳手装字体的行为等价)。
- iced 壳的已知取舍(相对旧 egui 壳)：
  - 设置窗的颜色覆盖用「色板 + 十六进制输入 + ↺ 重置」代替原生取色器；
  - 非 macOS 应用菜单栏为扁平下拉(无子菜单,声源直接列出);
  - 全屏状态经 250ms 节流回读 `window::mode` 同步(兼容系统绿色按钮)，
    绿色按钮进入全屏的勾选态可能滞后至多 250ms；
  - 键盘快捷键只在无文本输入框抢占键盘时生效(输入十六进制色值时 S/Space
    照常输入，不触发切换；iced 0.14 的按钮不接键盘焦点，点击按钮后
    快捷键不受影响)。
