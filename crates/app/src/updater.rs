//! 自更新:GitHub Releases 检查 + 带进度下载 + 原地换装重启。
//!
//! 线程模型与 ADR-0002 同思路:后台 `std::thread` 做全部 I/O(HTTP、
//! 解压、换装),事件经 `std::sync::mpsc` 送回;UI 线程每帧 `try_recv`
//! 轮询(`SpectrumApp::poll_update`)。纯逻辑(版本比较、资产匹配)为
//! 无 I/O 函数,单测覆盖;HTTP 依赖 `ureq`(rustls),解压用 `zip`/`tar`
//! 纯 Rust 实现,无需系统工具。

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};

/// 当前版本(Cargo workspace 版本,About 面板与比较基准)。
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 发布仓库(检查与下载的基址;Asset 命名约定见 .github/workflows/build.yml)。
pub const UPDATE_REPO: &str = "xm-18y/audio-spectrum";

/// 一个可下载的新版本(平台资产已匹配)。
#[derive(Clone, Debug)]
pub struct ReleaseInfo {
    /// 版本标签,如 `v0.7.0-20250115-1030`(取自 Asset 文件名)。
    pub label: String,
    /// 语义化版本 (major, minor, patch)。
    pub version: (u64, u64, u64),
    /// 资产文件名(兼作下载落盘名)。
    pub asset_name: String,
    /// 资产下载直链。
    pub asset_url: String,
    /// 资产字节数(API 提供;下载进度分母)。
    pub asset_size: u64,
}

/// 检查结果(无更新时附上线上版本标签)。
#[derive(Clone, Debug)]
pub enum CheckOutcome {
    /// 线上没有比当前更新的版本;`String` 为线上最新版本标签。
    UpToDate(String),
    /// 发现新版本。
    Available(ReleaseInfo),
}

/// 更新线程发回 UI 的事件流。
#[derive(Debug)]
pub enum UpdateEvent {
    /// 检查完成。
    Checked(Result<CheckOutcome, String>),
    /// 下载进度(字节)。
    Progress { received: u64, total: u64 },
    /// 新版本已就绪(本地可执行文件/包路径),UI 收到即换装重启。
    Downloaded(PathBuf),
    /// 下载或解压失败。
    Failed(String),
}

/// UI 侧更新状态机(菜单展示与按钮可用性由它驱动)。
#[derive(Clone, Debug)]
pub enum UpdatePhase {
    Idle,
    Checking,
    UpToDate(String),
    Available(ReleaseInfo),
    Downloading { received: u64, total: u64 },
    Failed(String),
}

// ---------------------------------------------------------------------------
// 入口:后台线程
// ---------------------------------------------------------------------------

/// 后台检查最新 Release;完成/失败各发一条 [`UpdateEvent::Checked`]。
pub fn spawn_check() -> Receiver<UpdateEvent> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let _ = tx.send(UpdateEvent::Checked(check_latest()));
    });
    rx
}

/// 后台下载并解压新版本;进度多帧上报,结束发 [`UpdateEvent::Downloaded`]
/// 或 [`UpdateEvent::Failed`]。
pub fn spawn_download(info: ReleaseInfo) -> Receiver<UpdateEvent> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let result = download_and_extract(&info, |received, total| {
            let _ = tx.send(UpdateEvent::Progress { received, total });
        });
        let _ = tx.send(match result {
            Ok(path) => UpdateEvent::Downloaded(path),
            Err(e) => UpdateEvent::Failed(e),
        });
    });
    rx
}

// ---------------------------------------------------------------------------
// 检查(GitHub Releases API)
// ---------------------------------------------------------------------------

/// 本平台在 Release Asset 名中的关键字(命名约定见 build.yml)。
fn platform_key() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => "windows-x64",
        ("macos", "aarch64") => "macos-arm64",
        ("linux", "x86_64") => "linux-x64",
        _ => "",
    }
}

/// 拉取 Releases 列表,取语义化版本大于当前的平台资产;无则视为已最新。
fn check_latest() -> Result<CheckOutcome, String> {
    let key = platform_key();
    if key.is_empty() {
        return Err(format!(
            "暂不支持 {}/{} 平台的自动更新",
            std::env::consts::OS,
            std::env::consts::ARCH
        ));
    }
    let url = format!("https://api.github.com/repos/{UPDATE_REPO}/releases?per_page=20");
    let body = http_get(&url)?;
    let json: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("解析 Releases 响应失败:{e}"))?;
    let releases = json
        .as_array()
        .ok_or_else(|| "Releases 响应格式异常".to_string())?;

    let current = parse_semver(CURRENT_VERSION).ok_or("内置版本号格式异常")?;
    let mut best: Option<ReleaseInfo> = None;
    for rel in releases {
        for asset in rel["assets"].as_array().into_iter().flatten() {
            let Some(name) = asset["name"].as_str() else { continue };
            // 平台匹配 + 从文件名解析版本,如
            // audio-spectrum-v0.7.0-20250115-1030-windows-x64.zip
            if !name.contains(key) {
                continue;
            }
            let Some(version) = parse_semver(name) else {
                continue;
            };
            if version <= current {
                continue;
            }
            let candidate = ReleaseInfo {
                label: format!("v{}", version_label(name)),
                version,
                asset_name: name.to_string(),
                asset_url: asset["browser_download_url"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                asset_size: asset["size"].as_u64().unwrap_or(0),
            };
            if asset_url_ok(&candidate) && best.as_ref().is_none_or(|b| b.version < version) {
                best = Some(candidate);
            }
        }
    }
    match best {
        Some(info) => Ok(CheckOutcome::Available(info)),
        // 线上版本标签仅作展示;都不可用说明没有可用构建。
        None => Ok(CheckOutcome::UpToDate(CURRENT_VERSION.to_string())),
    }
}

/// 资产下载直链必须存在,缺它更新无从谈起。
fn asset_url_ok(info: &ReleaseInfo) -> bool {
    info.asset_url.starts_with("https://")
}

fn http_get(url: &str) -> Result<String, String> {
    ureq::get(url)
        .timeout(std::time::Duration::from_secs(30))
        .call()
        .map_err(|e| format!("网络请求失败:{e}"))?
        .into_string()
        .map_err(|e| format!("读取响应失败:{e}"))
}

// ---------------------------------------------------------------------------
// 下载 + 解压
// ---------------------------------------------------------------------------

/// 下载资产到可执行文件同卷的临时目录(换装需 rename 同卷),解压并定位
/// 新版可执行产物;`progress` 按已下载字节节流回调。
fn download_and_extract(
    info: &ReleaseInfo,
    progress: impl Fn(u64, u64),
) -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("无法定位当前程序:{e}"))?;
    let work = exe
        .parent()
        .ok_or("当前程序路径异常")?
        .join(".update-tmp");
    std::fs::create_dir_all(&work).map_err(|e| format!("无法创建临时目录:{e}"))?;
    let archive = work.join(&info.asset_name);
    download_to(info, &archive, &progress)?;

    let payload = work.join("payload");
    let _ = std::fs::remove_dir_all(&payload);
    std::fs::create_dir_all(&payload).map_err(|e| format!("无法创建解压目录:{e}"))?;
    extract(&archive, &payload)?;
    locate_artifact(info, &payload, &exe)
}

fn download_to(
    info: &ReleaseInfo,
    dest: &Path,
    progress: &impl Fn(u64, u64),
) -> Result<(), String> {
    let resp = ureq::get(&info.asset_url)
        .timeout(std::time::Duration::from_secs(1800))
        .call()
        .map_err(|e| format!("下载失败:{e}"))?;
    let total = resp
        .header("Content-Length")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(info.asset_size);
    let mut reader = resp.into_reader();
    let mut file = std::fs::File::create(dest).map_err(|e| format!("无法写入下载文件:{e}"))?;
    let mut received: u64 = 0;
    let mut last_report: u64 = 0;
    use std::io::{Read as _, Write as _};
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let n = reader
            .read(&mut chunk)
            .map_err(|e| format!("下载中断:{e}"))?;
        if n == 0 {
            break;
        }
        file.write_all(&chunk[..n])
            .map_err(|e| format!("写入下载文件失败:{e}"))?;
        received += n as u64;
        // 节流:每 256KB 上报一次,尾包收尾必报。
        if received - last_report >= 256 * 1024 {
            last_report = received;
            progress(received, total);
        }
    }
    progress(received, total);
    Ok(())
}

/// 按资产扩展名解压(zip 或 tar.gz),全部在进程内完成,
/// 不依赖系统 unzip / Expand-Archive / tar 命令。
fn extract(archive: &Path, dest: &Path) -> Result<(), String> {
    let is_zip = archive
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("zip"));
    if is_zip {
        extract_zip(archive, dest)
    } else {
        extract_tar_gz(archive, dest)
    }
}

fn extract_zip(archive: &Path, dest: &Path) -> Result<(), String> {
    let file = std::fs::File::open(archive).map_err(|e| format!("无法读取压缩包:{e}"))?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| format!("解析 zip 失败:{e}"))?;
    zip.extract(dest).map_err(|e| format!("解压失败:{e}"))
}

fn extract_tar_gz(archive: &Path, dest: &Path) -> Result<(), String> {
    let file = std::fs::File::open(archive).map_err(|e| format!("无法读取压缩包:{e}"))?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
    tar.unpack(dest).map_err(|e| format!("解压失败:{e}"))
}

/// 在解压产物中定位新版可执行文件:
/// Windows zip → 根目录 `audio-spectrum.exe`;Linux tar.gz → `audio-spectrum/`
/// 目录内同名二进制;macOS zip → `audio-spectrum.app` 整包(裸二进制运行时
/// 取包内 `Contents/MacOS/audio-spectrum`)。
fn locate_artifact(info: &ReleaseInfo, payload: &Path, exe: &Path) -> Result<PathBuf, String> {
    let candidates: &[&str] = match std::env::consts::OS {
        "windows" => &["audio-spectrum.exe"],
        "macos" => {
            if macos_bundle_dir(exe).is_some() {
                &["audio-spectrum.app"]
            } else {
                &["audio-spectrum.app/Contents/MacOS/audio-spectrum"]
            }
        }
        _ => &["audio-spectrum/audio-spectrum"],
    };
    for c in candidates {
        let path = payload.join(c);
        if path.exists() {
            return Ok(path);
        }
    }
    Err(format!(
        "解压产物中未找到新版本({};资产 {})",
        candidates.join(" / "),
        info.asset_name
    ))
}

// ---------------------------------------------------------------------------
// 换装重启
// ---------------------------------------------------------------------------

/// macOS 下当前进程是否运行在 .app 包内;是则返回包目录。
fn macos_bundle_dir(exe: &Path) -> Option<PathBuf> {
    if std::env::consts::OS != "macos" {
        return None;
    }
    let macos = exe.parent()?;
    if macos.file_name()? != "MacOS" {
        return None;
    }
    let contents = macos.parent()?;
    contents.parent().map(|b| b.to_path_buf())
}

/// 用下载好的新版本原地换装并重启:当前可执行/包重命名为 `.old`,
/// 新版就位,拉起新进程后退出本进程。失败时回滚改名。
pub fn install(new_path: &Path) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("无法定位当前程序:{e}"))?;
    if let Some(bundle) = macos_bundle_dir(&exe) {
        return install_bundle(&bundle, new_path);
    }
    let old = exe.with_extension("old");
    rename_or_rollback(&exe, &old, &exe, new_path)?;
    // 新进程先拉起再退出,保证无缝衔接(子进程与父进程生命周期解耦)。
    if let Err(e) = std::process::Command::new(&exe).spawn() {
        let _ = std::fs::rename(&old, &exe);
        return Err(format!("无法启动新版本:{e}"));
    }
    cleanup_stale_sibling(&old);
    std::process::exit(0);
}

/// .app 包模式:整包换装(旧包改名让位 → 新包移入 → `open` 拉起)。
fn install_bundle(bundle: &Path, new_app: &Path) -> Result<(), String> {
    let old = bundle.with_extension("app.old");
    rename_or_rollback(bundle, &old, bundle, new_app)?;
    let launch = std::process::Command::new("open")
        .arg(bundle)
        .spawn()
        .or_else(|_| std::process::Command::new(new_app).spawn());
    if launch.is_err() {
        let _ = std::fs::rename(&old, bundle);
        return Err("无法启动新版本".to_string());
    }
    cleanup_stale_sibling(&old);
    std::process::exit(0);
}

/// 两步换装:旧→old、新→位;第二步失败时把 old 还原回去。
fn rename_or_rollback(
    a: &Path,
    b: &Path,
    a_back: &Path,
    new_src: &Path,
) -> Result<(), String> {
    std::fs::rename(a, b).map_err(|e| format!("无法让位旧版本:{e}"))?;
    if let Err(e) = std::fs::rename(new_src, a_back) {
        let _ = std::fs::rename(b, a_back); // 回滚,保住当前可用版本
        return Err(format!("无法安置新版本:{e}"));
    }
    Ok(())
}

/// 删除换装残留(old 文件与临时目录);失败静默——下次启动再清。
pub fn cleanup_stale() {
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let _ = std::fs::remove_dir_all(dir.join(".update-tmp"));
        let _ = std::fs::remove_file(exe.with_extension("old"));
        let _ = std::fs::remove_file(exe.with_extension("app.old"));
    }
}

/// 换装成功后、退出前清理本次残留(尽力而为)。
fn cleanup_stale_sibling(old: &Path) {
    let _ = std::fs::remove_file(old);
    if let Some(dir) = old.parent() {
        let _ = std::fs::remove_dir_all(dir.join(".update-tmp"));
    }
}

// ---------------------------------------------------------------------------
// 纯逻辑:版本解析与比较(单测覆盖)
// ---------------------------------------------------------------------------

/// 从任意字符串中提取首个 `x.y.z` 语义化版本(`audio-spectrum-v0.7.0-…-windows-x64.zip` → (0,7,0))。
fn parse_semver(text: &str) -> Option<(u64, u64, u64)> {
    let bytes = text.as_bytes();
    for (i, w) in bytes.windows(3).enumerate() {
        // 候选起点:数字且前一位不是数字(避免命中时间戳中段)。
        if !w[0].is_ascii_digit() || (i > 0 && bytes[i - 1].is_ascii_digit()) {
            continue;
        }
        let rest = &text[i..];
        let mut parts = [0u64; 3];
        let mut consumed = 0;
        let mut ok = true;
        for (k, part) in parts.iter_mut().enumerate() {
            let digits: String =
                rest[consumed..].chars().take_while(|c| c.is_ascii_digit()).collect();
            if digits.is_empty() {
                ok = false;
                break;
            }
            *part = digits.parse().ok()?;
            consumed += digits.len();
            if k < 2 {
                if rest[consumed..].starts_with('.') {
                    consumed += 1;
                } else {
                    ok = false;
                    break;
                }
            }
        }
        if ok {
            return Some((parts[0], parts[1], parts[2]));
        }
    }
    None
}

/// 版本号后的构建标签(如 `0.7.0-20250115-1030`):保留纯数字组(时间戳),
/// 在首个含字母的组(平台关键字,如 `windows-x64`)前截断,并去掉扩展名;
/// 无法解析时原样返回。
fn version_label(name: &str) -> String {
    let base = name.rsplit_once('.').map_or(name, |(b, _)| b);
    let Some((a, b, c)) = parse_semver(base) else {
        return name.to_string();
    };
    let mut label = format!("{a}.{b}.{c}");
    let rest = base[base.find(&label).map_or(0, |i| i + label.len())..]
        .trim_start_matches('-');
    for group in rest.split('-') {
        if group.is_empty() || !group.chars().all(|ch| ch.is_ascii_digit()) {
            break;
        }
        label.push('-');
        label.push_str(group);
    }
    label
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_parses_from_asset_names() {
        assert_eq!(parse_semver("0.6.0"), Some((0, 6, 0)));
        assert_eq!(
            parse_semver("audio-spectrum-v0.7.0-20250115-1030-windows-x64.zip"),
            Some((0, 7, 0))
        );
        assert_eq!(parse_semver("audio-spectrum-v1.2.3-macos-arm64.zip"), Some((1, 2, 3)));
        // 时间戳中段不误报;无版本返回 None。
        assert_eq!(parse_semver("audio-spectrum-20250115-1030-linux-x64.tar.gz"), None);
        assert_eq!(parse_semver("nothing here"), None);
    }

    #[test]
    fn semver_takes_first_standalone_number() {
        // 前缀数字相连时不误读(如 hash 前缀)。
        assert_eq!(parse_semver("build123-1.0.0.zip"), Some((1, 0, 0)));
    }

    #[test]
    fn version_label_keeps_build_tag() {
        assert_eq!(
            version_label("audio-spectrum-v0.7.0-20250115-1030-windows-x64.zip"),
            "0.7.0-20250115-1030"
        );
        assert_eq!(version_label("audio-spectrum-v1.2.3-macos-arm64.zip"), "1.2.3");
    }

    #[test]
    fn platform_keys_match_ci_matrix() {
        // build.yml 的三平台命名;非目标平台只验证映射表本身。
        let key = platform_key();
        assert!(
            ["windows-x64", "macos-arm64", "linux-x64", ""].contains(&key),
            "未预期平台:{key}"
        );
    }
}
