//! 发布命令:为当前 HEAD 打上版本 tag,并把代码与 tag 一起推送到远端,
//! 触发 GitHub Actions 三平台构建与 Release 发布。
//!
//! 用法:
//!   cargo run -p xtask -- release           # 版本号取自根 Cargo.toml 的 workspace 版本
//!   cargo run -p xtask -- release 0.3.0     # 显式指定版本号(自动补 v 前缀)

use std::process::{Command, exit};

fn main() {
    let arg = std::env::args().nth(1);
    if arg.as_deref() != Some("release") {
        eprintln!("用法: cargo run -p xtask -- release [版本号]");
        exit(2);
    }

    // 版本号:优先用参数,否则解析根 Cargo.toml 的 [workspace.package] version
    let version = match std::env::args().nth(2) {
        Some(v) => v.trim_start_matches('v').to_string(),
        None => workspace_version(),
    };
    let tag = format!("v{version}");

    // 工作区必须干净,避免把未提交改动留在本地
    if !git_out(&["status", "--porcelain"]).is_empty() {
        eprintln!("错误:工作区有未提交改动,请先提交或暂存。");
        exit(1);
    }

    // tag 不得与已有 tag 冲突
    if git_ok(&["rev-parse", "-q", "--verify", &format!("refs/tags/{tag}")]) {
        eprintln!("错误:tag {tag} 已存在。");
        exit(1);
    }

    let branch = git_out(&["rev-parse", "--abbrev-ref", "HEAD"]);

    git_run(&["tag", &tag]);
    git_run(&["push", "origin", &branch]);
    git_run(&["push", "origin", &tag]);

    println!("已推送 {branch} 与 tag {tag},Actions 构建已触发。");
}

fn workspace_version() -> String {
    let toml = std::fs::read_to_string("Cargo.toml").expect("无法读取根 Cargo.toml");
    let ws = toml
        .split("[workspace.package]")
        .nth(1)
        .expect("根 Cargo.toml 缺少 [workspace.package]");
    ws.lines()
        .find_map(|l| {
            l.trim()
                .strip_prefix("version")
                .and_then(|r| r.trim().strip_prefix('='))
                .map(|r| r.trim().trim_matches('"').to_string())
        })
        .expect("[workspace.package] 缺少 version 字段")
}

fn git_out(args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .output()
        .expect("git 不可用");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn git_ok(args: &[&str]) -> bool {
    Command::new("git")
        .args(args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn git_run(args: &[&str]) {
    let ok = Command::new("git")
        .args(args)
        .status()
        .expect("git 不可用")
        .success();
    if !ok {
        exit(1);
    }
}
