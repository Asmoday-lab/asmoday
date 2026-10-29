<div align="center">

# Asmoday

**个人媒体与设备控制中枢 · 极致轻量的单二进制极客面板**

[![Build](https://img.shields.io/badge/build-passing-brightgreen?style=flat-square)](#)
[![Version](https://img.shields.io/badge/version-v3.15.0-blue?style=flat-square)](#)
[![Platform](https://img.shields.io/badge/platform-Windows%20%7C%20Linux%20%7C%20macOS-lightgrey?style=flat-square)](#)
[![License](https://img.shields.io/badge/license-MIT-green?style=flat-square)](#)

*Rust(Axum) + Vue 3(Vite) + TailwindCSS v4*

**[English](README.md) | [简体中文](README.zh.md)**

</div>

---

> ⚠️ **为什么选择 Asmoday？**  
> 告别 `FileBrowser + ttyd + Nginx` 的繁琐拼装。我们用极致克制的设计，将**文件管理**、**原生终端**、**媒体画廊**与**外部服务聚合**完美整合到一个 **仅 6MB** 的跨平台单二进制文件中。

## ✨ 核心特性

- 🪶 **极致轻量**：单二进制原生打包（含前端 UI），体积仅约 6MB，极低常驻内存，完美适配树莓派及各类低功耗 ARM 设备。
- 🖥️ **原生 PTY 终端**：内置真实 Web 终端，支持多标签、断线保活、软键盘快捷键（Termux 风格），并支持在文件目录中一键“在此处打开终端”。
- 🖼️ **沉浸式媒体预览**：无刷新瀑布流画廊、全格式图片/音视频预览、外挂字幕加载、全局音乐播放条（支持 LRC 歌词与音量记忆）。
- 📂 **现代化文件管理**：摒弃繁琐的左侧目录树，采用多挂载点卡片 + 面包屑导航。内置 CodeMirror 6 代码编辑器，支持行尾换行符（EOL）与编码安全保护。
- 🔒 **公网级安全**：支持 2FA 认证、独立 WebDAV Token、防爆破限流、CSRF 保护，并原生支持反向代理与 SSO 单点登录。
- 🎨 **高定 UI/UX**：基于 TailwindCSS v4 的毛玻璃质感 UI，全局深浅色无缝切换，支持多语言（i18n）。

> **📸 界面预览**  
> *(此处放置你的主页和画廊截图)*  
> `<img src=".shots/home.png" width="800" alt="主页">`

## 🚀 快速开始

### 方式一：下载可执行文件（推荐）
前往 [Releases](https://github.com/Asmoday-lab/asmoday/releases) 页面，下载对应系统的压缩包，解压并运行：

```bash
# Linux / macOS
chmod +x asmoday
./asmoday

# Windows
asmoday.exe
```
浏览器访问 `http://127.0.0.1:4242`，首次访问将引导您设置管理员账号。

### 方式二：Docker 部署
```bash
docker run -d \
  -p 4242:4242 \
  -v /path/to/data:/app/data \
  -v /path/to/media:/mnt/media \
  --name asmoday \
  asmoday/asmoday:latest
```

### 命令行参数与重置
```powershell
# 脚本化部署（终端输出一次性密码）
asmoday --init

# 忘记密码：终端重置（配置中无免密后门）
asmoday --reset-password

# 自定义配置与数据目录
asmoday -c ./config.toml -d ./data -p 4242
```

## 🏗️ 开发与构建

如果你是开发者，请确保安装了 Rust (MSVC 工具链)、Node.js (v20+) 与 pnpm。

```bash
# 1. 编译前端
cd web
pnpm install
pnpm run build

# 2. 编译后端 (自动内嵌前端产物)
cd ..
cargo build --release

# 3. 开发模式 (前端热更新)
cd web && pnpm run dev  # http://localhost:5173
```

**跨平台交叉编译**（在 Windows 编译 Linux ARM64 目标）：
```bash
cargo zigbuild --release --target aarch64-unknown-linux-musl
```

## 🛡️ 安全模型与红线

1. **零外部 CDN**：所有前端资源、字体、图标全量内嵌，杜绝断网不可用与供应链投毒。
2. **配置防脱敏**：采用 TOML 格式，源码模式原样写回，绝不丢失注释。
3. **无免密后门**：结构上严禁关闭认证，改密只能走登录态或 CLI。
4. **防目录穿越**：挂载点白名单 + 显式拒绝 `..` + `canonicalize` 包含性校验。
5. **文件操作防爆**：单次 copy/move/delete 超出 100 项或 2 GiB 自动拦截；跨挂载点移动采用流式复制临时文件防损坏。

## 📈 Star History

如果这个项目对你有帮助，请给一个 ⭐️ Star！

[![Star History Chart](https://api.star-history.com/svg?repos=Asmoday-lab/asmoday&type=Date)](https://star-history.com/#Asmoday-lab/asmoday&Date)

## 📚 附录：API 契约与详细功能

<details>
<summary><b>点击展开查看 API 一览表</b></summary>

### 文件与挂载点
| 方法 | 路径 | 说明 |
| --- | --- | --- |
| GET | `/api/files/mounts` | 获取挂载点列表（只读/离线状态） |
| GET | `/api/files/list` | 目录列表（支持排序、游标分页、隐藏项过滤） |
| POST | `/api/files/mkdir` | 新建目录（防静默覆盖） |
| POST | `/api/files/move` `/copy` | 批量移动/复制（支持三种冲突策略） |
| POST | `/api/files/delete` | 批量删除（需 confirm_name 或一次性 Token） |
| GET/PUT | `/api/files/raw` | 流式读写（支持 HTTP Range 与 ETag 缓存） |

### 媒体与终端
| 方法 | 路径 | 说明 |
| --- | --- | --- |
| GET | `/api/media/browse` | 浏览媒体（含 ThumbHash 占位与 LRC 字幕探测） |
| GET | `/api/media/cover` | 提取音频内嵌封面 (ID3 APIC) |
| POST | `/api/media/progress` | 上报播放进度（用于跨端续播） |
| WS | `/api/terminal/ws` | 终端 WebSocket 通信（PTY 数据流透传） |

### 系统与安全
| 方法 | 路径 | 说明 |
| --- | --- | --- |
| GET/POST | `/api/config` | 读写解析后的配置 |
| GET/POST | `/api/auth/totp` | 2FA 动态验证码设置 |
| any | `/proxy/{id}/*` | 外部服务反向代理透传 |
| any | `/dav/<mount_id>/*` | 标准 WebDAV 服务 (需独立 Token) |

</details>

<details>
<summary><b>点击展开查看历史更新日志</b></summary>

- **v3.15.0 (文档对齐)**：PTY 尺寸强制钳制（0/超大值不再击穿 ConPTY）、访问日志查询串按需脱敏、跨挂载移动「删源失败」独立错误码 `E_SOURCE_DELETE_FAILED`、垃圾文件补 `.crdownload/.part/.!ut/.tmp`、Termux `$PREFIX/bin/bash` 兼容、终端活动会话面板与强制关闭、编辑器语言矩阵扩至 30+ 语言、挂载点动态移除/离线兜底、缩略图下载中跳过、配置热重载四重防死循环、批量失败项防呆重试。
- **v3.5.9 (路径复制)**：右键菜单新增复制相对/绝对路径，修复分享页路径泄露漏洞。
- **v3.5.8 (日志系统)**：引入分级日志与脱敏，独立 `errors.log`。
- **v3.5.6 (错误码)**：Windows 权限拒绝(403)友好提示。
- **v3.5.5 (界面)**：目录点选器重构、面包屑点击回滚修复。
- **v3.5.3 (终端)**：历史记录强制隔离到 data 目录，解决受限环境红字报错。
- **v3.3.0 (双视图/路由)**：恢复照片库相册/时间线双视图，修复单挂载点直达逻辑。
- **v3.0.0 (重构)**：全局右键菜单、多选功能、主题入口重排、图片扁平化。
- **v2.10.0 (终端一致性)**：全量环境变量继承，跨平台垃圾文件过滤。
- **v2.8.2 (ThumbHash)**：将 ThumbHash 落盘改为存入 SQLite 防 Inode 耗尽。

</details>

---

<div align="center">
  <sub>Built with ❤️ by Asmoday-lab</sub>
</div>