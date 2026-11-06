<div align="center">

# Asmoday

**自托管的文件管理、Web 终端与媒体库**

[![Release](https://img.shields.io/github/v/release/Asmoday-lab/asmoday?include_prereleases&style=flat-square)](https://github.com/Asmoday-lab/asmoday/releases)
[![License](https://img.shields.io/badge/license-MIT-green?style=flat-square)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.99%2B-orange?style=flat-square)](https://www.rust-lang.org)
[![Platform](https://img.shields.io/badge/platform-Windows%20x64%20%7C%20Linux%20x64%2Farm64-lightgrey?style=flat-square)](#快速开始)

[特性](#特性) · [快速开始](#快速开始) · [配置](#配置) · [命令行](#命令行) · [常见问题](#常见问题) · [开发](#开发) · [English](README.md)

</div>

---

Asmoday 是一个轻量的自托管面板，把**文件管理**、**Web 终端**、**照片 / 影视 / 音乐库**、**分享链接**与 **WebDAV** 放进一个可执行文件里。后端用 Rust（axum），前端用 Vue 3，构建时前端资源全部内嵌进二进制；运行时只额外需要一个 SQLite 文件。

它面向的是「一台自己的机器 + 浏览器」这种用法：NAS、树莓派、迷你主机、旧笔记本，或者日常使用的 Windows 电脑。不依赖 Node、数据库服务或任何外部 CDN，装上就能用。

## 特性

- 📂 **文件管理** —— 多挂载点（可单独设为只读）、网格 / 列表视图、拖拽上传（大文件自动分块、断点续传）、批量复制移动删除、整目录打包下载、递归搜索。
- ✏️ **在线编辑** —— CodeMirror 6 编辑器，语法高亮、Markdown 预览、CSV 与 PDF 只读预览；保存时原样写回换行符（LF / CRLF）与编码。
- 🖥️ **Web 终端** —— 基于 `portable-pty` 的真实 PTY，多标签、断线保活、手机快捷键条；可在文件页右键「在此处打开终端」。
- 🖼️ **照片库** —— Immich 式按天时间线（等高行保留每张图真实比例、右侧年份刻度）与文件夹墙两种入口，缩略图未就绪时用 ThumbHash 占位，支持幻灯片与收藏。
- 🎬 **影视库** —— 全部影片走同一套按天时间线，文件夹卡片、上次位置续播、外挂字幕（`.srt` 实时转 WebVTT，或 `.vtt`）、Video.js 与原生播放器切换。
- 🎵 **音乐库** —— 读取 ID3v2 / FLAC / MP4 / Ogg 标签与 LRC 歌词，底部常驻播放条，列表循环 / 单曲循环 / 随机。
- 🔗 **分享链接** —— 文件或文件夹生成不可猜链接，可加访问口令、有效期、禁止下载、Referer 白名单。
- ⬇️ **内置下载引擎** —— 自带「下载」页面（与文件管理、媒体库同级：任务列表、新建、暂停/继续、引擎设置、RPC 密钥查看与刷新——密钥由服务生成并存在数据库里，不手填），同时讲 aria2 兼容 RPC（`/jsonrpc`，AriaNg / Aria2 Explorer 填地址与密钥即用）：HTTP 多连接分段与断点续传、BitTorrent（磁力链 / `.torrent` / DHT / PEX / 做种），**不需要另装 aria2**。
- 🗄️ **WebDAV** —— 标准方法集与真锁，令牌可绑定单个挂载点；手机同步 App、系统文件管理器可直接挂载。
- 🔒 **安全** —— Argon2id 哈希、TOTP 两步验证、会话滑动续期、登录 / 分享 / WebDAV 三套独立限流、路径穿越防护、默认安全响应头。
- 🌗 **界面** —— Tailwind CSS v4 毛玻璃风格，深浅色切换，中英双语，桌面与手机自适应。
- 🧩 **服务聚合** —— 通过 `[[services]]` 把其它自建服务挂到首页，经内置反向代理打开，可注入请求头。
- 🪶 **轻量** —— 单个可执行文件（已含前端与下载引擎），无运行时依赖，适合长期常开。

## 快速开始

### 下载运行

从 [Releases](https://github.com/Asmoday-lab/asmoday/releases) 下载对应平台的压缩包：

| 平台 | 产物 |
| --- | --- |
| Windows x64 | `asmoday-x86_64-pc-windows-msvc.zip` |
| Linux x64 | `asmoday-x86_64-unknown-linux-musl.tar.gz` |
| Linux arm64 | `asmoday-aarch64-unknown-linux-musl.tar.gz` |

```bash
# Linux / macOS
tar -xzf asmoday-*.tar.gz && chmod +x asmoday
./asmoday

# Windows：解压后双击 asmoday.exe，或在命令行运行
asmoday.exe
```

打开 `http://127.0.0.1:4242`，用**控制台打印的一次性密码**登录（网页端没有用户名），登录后按提示设置正式密码即可。

首次启动会在当前目录生成 `config.toml` 与 `data/`。未配置挂载点时，程序把当前工作目录作为兜底根；要管理其它磁盘，到「设置 → 高级」添加挂载点，或直接编辑配置：

```toml
[[system.mounts]]
path = "D:/Media"        # Linux 上形如 /mnt/media；同一路径不能重复挂载
readonly = false         # 只读挂载点拒绝一切写入

[[system.mounts]]
name = "备份盘"          # 可选：留空则取目录末段（这里会是「Media」）
path = "E:/Media"
readonly = true
```

> macOS 没有预编译产物，请从源码构建（见[开发](#开发)）。

### 从源码构建

需要 Rust 1.99+、Node.js 20+ 与 pnpm：

```bash
cd web && pnpm install && pnpm run build && cd ..
cargo build --release          # 产物：target/release/asmoday
```

### 用 systemd 托管（Linux）

```ini
[Unit]
Description=Asmoday
After=network.target

[Service]
User=asmoday
WorkingDirectory=/opt/asmoday
ExecStart=/opt/asmoday/asmoday -c /opt/asmoday/config.toml -d /opt/asmoday/data
Restart=on-failure
RestartSec=3

[Install]
WantedBy=multi-user.target
```

### 前置反向代理（可选）

```caddyfile
asmoday.example.com {
    reverse_proxy 127.0.0.1:4242
}
```

走 HTTPS 反代时，建议把 `[server].host` 改成 `127.0.0.1`，并按需打开 `[server.proxy].trust_proxy_headers` 与 `trusted_ips`。挂到子路径（如 `https://example.com/asmoday`）时设置 `base_url`。项目不内置 ACME，证书请自行签发或交给 Caddy / Nginx。

## 配置

配置文件为 TOML，默认读取顺序：`-c` 参数 → `ASMODAY_CONFIG_PATH` → 当前目录 `config.toml`；数据目录为 `-d` → `ASMODAY_DATA_DIR` → 当前目录 `data/`。设置页提供可视化表单与源码模式，保存时会保留注释与原有顺序。

| 配置段 | 作用 |
| --- | --- |
| `[server]` | 监听地址、端口、TLS、反向代理兼容、批量操作上限 |
| `[auth]` | 本地登录开关、代理信任（SSO） |
| `[system]` | 挂载点 `[[system.mounts]]`、软链接策略 |
| `[media]` | 缩略图缓存容量与闲置天数、FFmpeg 路径、上传大小上限、分块上传片大小 |
| `[terminal]` | 自定义 shell / 参数、断线保活时长、并发会话数 |
| `[log]` | 控制台与文件级别、访问日志、文本或 JSON 输出 |
| `[[services]]` | 首页聚合的外部服务与反代选项 |

```toml
[server]
host = "0.0.0.0"
port = 4242

[media]
ffmpeg_path = ""            # 留空则从 PATH 探测，仅用于截取视频首帧
thumb_cache_max_size_mb = 1024

[[system.mounts]]
name = "媒体库"
path = "/mnt/media"
readonly = false

[[services]]
id = "grafana"
name = "监控面板"
icon = "chart"
target_url = "http://192.168.1.50:3000"
strip_prefix = true
```

环境变量（优先级高于配置文件）：

| 变量 | 说明 |
| --- | --- |
| `ASMODAY_CONFIG_PATH` | 配置文件路径 |
| `ASMODAY_DATA_DIR` | 数据目录 |
| `ASMODAY_HOST` / `ASMODAY_PORT` | 监听地址与端口 |
| `ASMODAY_FFMPEG_PATH` | FFmpeg 路径 |
| `ASMODAY_RESET_PASSWORD` | 配合 `--reset-password` 非交互重置密码（用完请删除） |

## 命令行

| 命令 | 说明 |
| --- | --- |
| `asmoday --init` | 生成默认配置与数据目录，输出一次性管理员密码 |
| `asmoday --reset-password` | 重置管理员密码，输出一次性新密码 |
| `asmoday --reset-2fa` | 清除 TOTP 并关闭两步验证（需当前密码） |
| `asmoday --check-config` | 只校验配置，不启动服务 |
| `asmoday --backup ./asmoday-backup.db` | 导出一致性数据库备份后退出（服务运行中也可执行） |
| `asmoday -c ./config.toml -d ./data -p 4242 -H 0.0.0.0` | 指定配置、数据目录、端口与监听地址 |

## WebDAV

在系统文件管理器、Cyberduck、rclone 或手机同步 App 里添加服务器：

- 地址：`http://<host>:4242/dav/`（`/dav/` 会列出当前可见的挂载点）
- 认证：WebDAV 令牌（可在设置里绑定单个挂载点、设为只读），或管理员账号 + 登录密码
- 支持 `OPTIONS`、`PROPFIND`、`GET`、`HEAD`、`PUT`、`DELETE`、`MKCOL`、`MOVE`、`COPY`、`LOCK`、`UNLOCK`

## 常见问题

**忘记密码了怎么办？**
在服务器上执行 `asmoday --reset-password`（配置里没有免密后门）。忘记两步验证可用 `--reset-2fa`。

**局域网里其它设备打不开？**
确认 `[server].host` 是 `0.0.0.0`，并放行防火墙端口；用了反向代理时把 host 收回到 `127.0.0.1`。

**端口被占用？**
`asmoday -p 8080` 覆盖端口，或改配置里的 `[server].port`。

**视频放不出来？**
播放交给浏览器解码，项目不做转码。HEVC / AV1 等编码能否播放取决于浏览器与系统解码器。

**缩略图一直是占位图？**
视频首帧需要外部 FFmpeg：在设置里填入 `ffmpeg_path`，或让 `ffmpeg` 出现在 `PATH` 中。图片缩略图不依赖 FFmpeg。

**复制 / 移动提示超出限制？**
默认单次上限 100 个选中条目与 2 GiB，可在 `[server.limits]` 调整；更大的任务请在终端里用 `cp` / `mv`。

**数据在哪，怎么备份？**
全部在数据目录：`data/asmoday.db`（用户、会话、分享、索引）、`data/logs/`、`data/thumbnails/`。运行中可用 `asmoday --backup` 或设置页下载一致性备份。

**支持多用户吗？删除能恢复吗？**
不支持，单实例只有一个管理员账号；删除为物理删除，没有回收站。分享链接删除后保留 30 天墓碑，旧链接返回 `410`。

## 技术栈

| 层 | 技术 |
| --- | --- |
| 后端 | Rust 2024、axum 0.8、tokio、rusqlite（bundled, WAL） |
| 终端 | portable-pty + xterm.js（ConPTY 死锁缓解、断线保活） |
| 媒体 | 外置 FFmpeg（仅截帧）、lofty（音频标签）、Video.js |
| WebDAV | dav-server（协议实现）+ 自建认证与边界 |
| 前端 | Vue 3.5、Vite、Tailwind CSS v4、CodeMirror 6、vue-i18n |
| 打包 | rust-embed 内嵌前端；GitHub Actions 产出三个平台包 |

## 开发

```bash
# 前端（热更新）
cd web && pnpm install && pnpm run dev     # http://localhost:5173

# 后端
cargo run                                   # 默认 http://127.0.0.1:4242

# 发布构建（自动内嵌 web/dist）
cd web && pnpm run build && cd .. && cargo build --release
```

提交前请通过门禁：

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cd web && pnpm exec vue-tsc -b && pnpm run build
```

目录结构：

```
src/            Rust 后端（api/ 路由与处理器，core/ 配置、认证、文件系统、日志）
web/            Vue 3 前端（views/ 页面，components/ 组件，locales/ 中英文案）
Design/         产品与系统设计文档
.github/        发布工作流（打 v* tag 触发）
```

## 贡献

欢迎 issue 与 PR。提交前请：

1. 大改动先开 issue 说明动机与方案，避免白做；
2. 跑通上面的门禁，新增行为补测试；
3. 界面文案一律走 `vue-i18n`，不要硬编码中文或英文；
4. 提交信息用 `type(scope): 摘要`，如 `fix(player): 修播放条高度`。

## 许可

[MIT](LICENSE) © Asmoday-lab

## 致谢

感谢 axum、rusqlite、portable-pty、dav-server、lofty、Vue、Vite、Tailwind CSS、CodeMirror、xterm.js、Video.js 等开源项目。

<div align="center">
  <sub>如果这个项目对你有帮助，欢迎点一个 ⭐️</sub>
</div>
