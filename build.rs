//! 构建期信息注入（设计文档 3.7.1：启动日志必须能看出前端构建版本）。
//!
//! 只读 `git`，不引任何依赖；取不到就退化为 `dev`，绝不让构建失败。

use std::process::Command;

fn main() {
    let hash = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "dev".to_string());

    let date = Command::new("git")
        .args(["log", "-1", "--format=%cd", "--date=format:%Y-%m-%dT%H:%M:%SZ"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=ASMODAY_BUILD_ID={hash} ({date})");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/heads");
}
