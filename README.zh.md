<div align="center">

# Asmoday

**自托管的文件管理、Web 终端、媒体库与下载引擎**

[![Release](https://img.shields.io/github/v/release/Asmoday-lab/asmoday?include_prereleases&style=flat-square)](https://github.com/Asmoday-lab/asmoday/releases)
[![License](https://img.shields.io/badge/license-MIT-green?style=flat-square)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.99%2B-orange?style=flat-square)](https://www.rust-lang.org)
[![Platform](https://img.shields.io/badge/platform-Windows%20x64%20%7C%20Linux%20x64%2Farm64-lightgrey?style=flat-square)](#快速开始)

[特性](#特性) · [快速开始](#快速开始) · [配置](#配置) · [命令行](#命令行) · [常见问题](#常见问题) · [开发](#开发) · [English](README.md)

</div>

---

Asmoday 是一个轻量的自托管面板，把**文件管理**、**Web 终端**、**照片 / 影视 / 音乐库**、**下载引擎**、**分享链接**与 **WebDAV** 放进一个可执行文件里。后端用 Rust（axum），前端用 Vue 3，构建时前端资源全部内嵌进二进制；运行时只额外需要一个 SQLite 文件。

它面向的是「一台自己的机器 + 浏览器」这种用法：NAS、树莓派、迷你主机、旧笔记本，或者日常使用的 Windows 电脑。不依赖 Node、数据库服务或任何外部 CDN，装上就能用。

## 特性

- 📂 **文件管理** —— 多挂载点（可单独设为只读）、网格 / 列表视图、拖拽上传（大文件自动分块、断点续传）、批量复制移动删除、整目录 / 多选打包下载、递归搜索。
- ✏️ **在线编辑** —— CodeMirror 6 编辑器，语法高亮、Markdown 预览、CSV 与 PDF 只读预览；保存时原样写回换行符（LF / CRLF）与编码。
- 🖥️ **Web 终端** —— 基于 `portable-pty` 的真实 PTY，多标签、断线保活、手机快捷键条；可在文件页右键「在此处打开终端」。
- 🖼️ **照片库** —— Immich 式按天时间线（等高行保留每张图真实比例、右侧年份刻度）与文件夹墙两种入口，缩略图未就绪时用 ThumbHash 占位，支持幻灯片与收藏。
- 🎬 **影视库** —— 全部影片走同一套按天时间线，文件夹卡片、上次位置续播、外挂字幕（`.srt` 实时转 WebVTT，或 `.vtt`）、Video.js 与原生播放器切换。
- 🎵 **音乐库** —— 读取 ID3v2 / FLAC / MP4 / Ogg 标签与 LRC 歌词，底部常驻播放条，列表循环 / 单曲循环 / 随机。
- 🔗 **分享链接** —— 文件或文件夹生成不可猜链接，可加访问口令、有效期、禁止下载、Referer 白名单。
- ⬇️ **下载引擎** —— 与文件管理、媒体库平级的独立页面：加链接 / 磁力链 / `.torrent`、暂停继续、保存目录直接从挂载点里选。设置是页内视图而不是弹窗，有改动没保存时离开会先确认，系统返回键先回任务列表而不是直接跳出页面。RPC 密钥由服务端生成、存在数据库里，页内可查看与刷新，不支持手填。保存目录与 BT Tracker 改完立即生效；引擎参数要重启后生效，页面上会标出来。
- 🔌 **aria2 兼容 RPC** —— `/jsonrpc`，AriaNg / Aria2 Explorer 填地址与密钥即用：HTTP 多连接分段与断点续传、BitTorrent（磁力链 / `.torrent` / DHT / PEX / 做种）。浏览器插件传下来的 Cookie / Referer 会带到每一个分段和每一次重定向跳转，**不需要另装 aria2**。
- 🗄️ **WebDAV** —— 标准方法集与真锁，认证只认令牌（用户名填令牌名称、密码填 `asm_…` 令牌本身），可限定单个挂载点、可只读、可设有效期。手机同步 App、Cyberduck、rclone 与系统文件管理器可直接挂载。
- 🔒 **安全** —— Argon2id 哈希、密码只收可见 ASCII（8~128 位，不含空格）、TOTP 两步验证（登录页只有一个输入框：开了 2FA 就变成 6 位动态码框；验证器丢了用一次性恢复码）、会话滑动续期、登录 / 分享 / WebDAV 三套独立限流、路径穿越防护、默认安全响应头。
- 🌗 **界面** —— Tailwind CSS v4 毛玻璃风格，深浅色切换，中英双语，桌面与手机自适应。
- 🧩 **服务聚合** —— 通过 `[[services]]` 把其它自建服务挂到首页，经内置反向代理打开，可注入请求头。
- 🧭 **生态推荐** —— 离线可用的清单页，列出与本项目配合良好的客户端（Syncthing / PhotoSync / FolderSync、AriaNg、RaiDrive / Cyberduck / WinSCP、Caddy / Tailscale、PicGo / Typora / Obsidian），外链直达官网。
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

打开 `http://127.0.0.1:4242`，用**控制台打印的一次性密码**登录（网页端没有用户名），登录后按提示设置正式密码即可。横幅里同时打印入口地址，在 Windows Terminal / iTerm2 / VS Code 终端里是超链接，Ctrl/⌘+点击直接打开；双击 exe 打开的经典控制台点不动链接，把地址复制到浏览器即可。配 `host = "0.0.0.0"` 时启动日志还会多一行 `network:` 给出局域网地址，手机照着填就行。密码没记下来也不要紧：只要还没设过正式密码，重启进程会**重新签发一个**（旧的即刻作废），横幅会照原样再打一遍。

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

### Docker

```bash
docker compose up -d --build     # 配置、数据库、缩略图缓存、下载都在 asmoday-data 卷里
docker compose logs -f asmoday   # 首启的一次性管理员密码只在这里出现一次
```

不用 compose 也可以：

```bash
docker build -t asmoday .
docker run -d --name asmoday \
  -p 4242:4242 -p 6881:6881 -p 6881:6881/udp \
  -v asmoday-data:/data \
  asmoday
```

镜像默认 `ASMODAY_HOST=auto`：先试 IPv6 双栈，环境给不了（Docker 默认 bridge 就是 IPv4-only）就降到 `0.0.0.0`。别写死 `::`——那种网络里进程会直接起不来；写死 `0.0.0.0` 又在双栈宿主上丢掉了 IPv6。

- 端口：`4242` Web UI / API，`6881` BT 的 TCP + UDP
- 卷：`/data` 放 `config.toml`、`asmoday.db`、`thumbnails/`、`downloads/`
- 运行层是 alpine，静态 musl 二进制、非 root（uid 10001）、自带 healthcheck
- 首次构建要在 musl 上重编整棵依赖树，十几分钟属正常；之后走 BuildKit 缓存
- 发版会推 `ghcr.io/<owner>/asmoday:<tag>` 的多架构镜像（amd64 + arm64）

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
host = "127.0.0.1"        # 默认只监听本机；要局域网访问改成 0.0.0.0 / ::
port = 4242

[media]
ffmpeg_path = ""            # 留空则从 PATH 探测，仅用于截取视频首帧
thumb_cache_max_size_mb = 1024

[[system.mounts]]
name = "媒体库"
path = "/mnt/media"
readonly = false

[[services]]
id = "grafana"              # 永久地址：/proxy/grafana/，起个能读的
name = "监控面板"
icon = "gear"               # 图标在界面上选；留空或写了图标集之外的值会退默认
target_url = "http://192.168.1.50:3000"
inject_headers = { X-WEBAUTH-USER = "admin" }
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
| `asmoday --init` | 只生成默认配置文件后退出；不启动服务、不建库、不发密码（正常启动时才打印一次性管理员密码） |
| `asmoday --reset-password` | 重置管理员密码，输出一次性新密码 |
| `asmoday --reset-2fa` | 清除 TOTP 并关闭两步验证（需当前密码） |
| `asmoday --check-config` | 只校验配置，不启动服务 |
| `asmoday --backup ./asmoday-backup.db` | 导出一致性数据库备份后退出（服务运行中也可执行） |
| `asmoday -c ./config.toml -d ./data -p 4242 -H 0.0.0.0` | 指定配置、数据目录、端口与监听地址 |

## WebDAV

在系统文件管理器、Cyberduck、rclone 或手机同步 App 里添加服务器：

- 地址：`http://<host>:4242/dav/`（`/dav/` 会列出当前可见的挂载点）
- 用户名填**令牌名称**，密码填 `asm_…` 令牌本身。令牌在「设置 → 安全」里创建，可绑定单个挂载点、设为只读、设置有效期，随时吊销。
- 管理员登录密码**不能**用于 WebDAV；此前用它连上的客户端升级后会拿到 `401`，建个令牌换上去即可。
- 支持 `OPTIONS`、`PROPFIND`、`GET`、`HEAD`、`PUT`、`DELETE`、`MKCOL`、`MOVE`、`COPY`、`LOCK`、`UNLOCK`

## 常见问题

**忘记密码了怎么办？**
在服务器上执行 `asmoday --reset-password`（配置里没有免密后门）。忘记两步验证可用 `--reset-2fa`。

**WebDAV 客户端突然报 `401`？**
WebDAV 只认令牌。到「设置 → 安全」建一个，客户端用户名填令牌名称、密码填 `asm_…` 令牌；管理员登录密码是被明确拒绝的。删掉或过期的令牌会当场失效，正在连的客户端下一次请求就被挡回。

**局域网里其它设备打不开？**
默认只监听回环（`[server].host = "127.0.0.1"`），把 host 改成 `0.0.0.0`（或 `::`）再放行防火墙端口；用了反向代理时回到 `127.0.0.1` 更安全。

**端口被占用？**
`asmoday -p 8080` 覆盖端口，或改配置里的 `[server].port`。

**视频放不出来？**
播放交给浏览器解码，项目不做转码。HEVC / AV1 等编码能否播放取决于浏览器与系统解码器。

**缩略图一直是占位图？**
视频首帧需要外部 FFmpeg：在设置里填入 `ffmpeg_path`，或让 `ffmpeg` 出现在 `PATH` 中。图片缩略图不依赖 FFmpeg。

**复制 / 移动提示超出限制？**
默认单次上限 100 个选中条目与 2 GiB，可在 `[server.limits]` 调整；更大的任务请在终端里用 `cp` / `mv`。

**数据在哪，怎么备份？**
全部在数据目录：`data/asmoday.db`（账号、会话、分享、索引）、`data/logs/`、`data/thumbnails/`。运行中可用 `asmoday --backup` 或设置页下载一致性备份。

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

测试与调试流程（门禁分层、探针套件、数据与产物规矩）见 `doc/testing.md`——该目录只在本机维护，有意不入版本库。

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
