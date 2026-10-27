# Asmoday

个人媒体与设备控制中枢 · Rust(Axum) + Vue 3(Vite) + TailwindCSS v4 · **单二进制发布**。

当前状态：设计文档（v1.5.0）功能全部落地，另含认证、配置双模编辑与热重载、WebDAV、上传与搜索、分享/图床、收藏与续播。

## 一、启动

```powershell
cd asmoday
C:\AimerFiles\dev-env\rust\cargo\bin\cargo.exe run
```

打开 <http://127.0.0.1:4242> —— 首次访问会引导你设置管理员账号（也可用 `--init` 生成一次性密码）。

```powershell
# 一次性生成管理员与密码（脚本化部署）
asmoday --init
# 忘记密码：终端重置，配置里没有任何免密开关
asmoday --reset-password
# Docker/脚本场景：环境变量直接重置管理员密码（用完记得删）
ASMODAY_RESET_PASSWORD=新密码 asmoday -d ./data
# 覆盖项
asmoday -c ./config.toml -d ./data -p 4242
```

前端开发模式（改样式时用；产物最终由 rust-embed 内嵌）：

```powershell
cd asmoday/web
npm run dev     # http://localhost:5173，/api 与 WS 自动代理到 4242
```

> 本机没有 npm，依赖用 pnpm 安装（`pnpm-lock.yaml`）；装官方 Node LTS 后 `npm install` 等价。

## 二、已实现的功能

| 模块 | 内容 |
| --- | --- |
| 认证与安全 | Argon2 密码哈希、HttpOnly 会话 Cookie、全量 API 鉴权中间件、5 次失败封 IP 15 分钟、可选 TOTP 双因素、CLI 密码重置、一次性密码首登强制改密 |
| 配置 | `config.toml`（TOML，禁 YAML），带注释默认模板，`GET/POST /api/config` 与 `GET/POST /api/config/raw`（源码模式保留注释），`notify` 监听文件变更自动热重载；设置页分 5 个 Tab（通用/媒体/安全/高级/生态推荐），**分区块恢复默认**（通用=主题+语言，媒体=`[media]` 三项），挂载点与外部服务属高级配置绝不被重置覆盖；FFmpeg 路径三层优先级（设置页 > `ASMODAY_FFMPEG_PATH` > 系统 PATH，**可填可执行文件或安装目录**）+ 状态检测 + 测试连通性 |
| 文件管理 | **无目录树**：挂载点卡片入口 + 面包屑（Home/根名/子段，不暴露真实绝对路径）、网格/列表双虚拟化、1000 条强制分页、流式读取 + HTTP Range、拖拽上传（XHR 进度）、当前目录模糊搜索、**隐藏文件开关**（默认过滤 `.` 开头项，工具栏一键切换，`show_hidden` 参数）+ **文件预览矩阵**（双击分流：图片/音视频→全局 MediaLightbox 左右切换；Markdown→`marked`+`DOMPurify` 渲染可切源码；CSV/TSV→只读表格，>5MB 或 >1000 行强制截断并提示下载；PDF→原生 iframe；Office/CAD/压缩包→**不引解析库**只提示下载；纯文本→CodeMirror）+ 缩略图失败自动回退图标 + 右键菜单（打开/终端/复制路径/下载/分享/图床直链/**删除**，删除有二次确认弹窗，挂载点根禁止删） |
| 在线编辑 | CodeMirror 6 按需加载，js/ts/rust/python/toml/nginx 语言包各自独立 chunk，`Ctrl+S` 原子写回 |
| 终端 | portable-pty 真实 shell（Windows 显式 `powershell.exe` 绝对路径，Unix 读 `$SHELL` 回退 `bash`）、PTY 直通全屏应用、**最多 10 个会话**（超限先回收空闲）、多标签（同目录复用不叠加）、WebSocket 双向桥接、断线 3 分钟保活 + 缓冲回放、**前端指数退避自动重连**（服务重启后自愈，无需关标签）、重试用尽后状态栏「重新连接」、`dirs::home_dir()` 解析主目录（前端不传 `~`/`%USERPROFILE%`，后端对占位符兜底展开并提示）、POSIX 风格路径统一转原生分隔符后再交给 PTY、目录已删则退回默认目录并提示、文件右键「在终端中打开」、移动端 Termux 两行键盘（粘性 Ctrl/Alt） |
| 媒体 | 缩略图**WebP 优先**（有 libwebp 编码器的 FFmpeg 时输出 WebP，无则降级 JPEG/PNG；按实际格式回 Content-Type）+ 信号量（并发 2）+ 磁盘缓存 + `[media]` 可配置淘汰（`thumb_cache_max_size_mb` 默认 1024 MiB LRU + `thumb_cache_max_age_days` 默认 90 天过期，每 30 分钟后台清理，设置页可查看用量并手动清空）+ **ThumbHash 占位**（扫描时返回平均色，图未加载前不白屏）；相册/时间线（虚拟列表）；影视墙（**外部可选 FFmpeg 截首帧作封面**，WebP 优先 → JPEG → PNG 逐级降级，与缩略图共用 LRU 缓存；**路径可填可执行文件或安装目录**）+ Video.js 10（`@videojs/html` 自定义元素，neutral 皮肤）+ **外挂字幕**（同目录同名 `.srt/.vtt`，后端统一转 WebVTT）+ 原生播放器降级开关 + 播放进度续播；音乐 **ID3 标签与内嵌封面**（`APIC/PIC`，`GET /api/media/cover`）+ 全局悬浮播放条（循环/单曲/随机 + 进度拖拽 + **音量记忆**（默认 35%、`asm_volume` 持久化、静音恢复静音前音量、移动端隐藏滑块）+ **播放列表抽屉**（PC 贴条上方/移动端占下半屏，切歌/清空/高亮））+ `.lrc` 歌词滚动；收藏与「只看收藏」 |
| 分享与图床 | 文件右键「分享…」（口令/有效期/允许下载）与「复制图床直链」；公开页 `/s/{token}`（口令解锁、内联预览、目录浏览与子文件下载）；独立「分享与图床」管理页；`[shares].referer_allowlist` 防盗链 |
| 反代网关 | `[[services]]` 驱动的 `/proxy/<id>/…` 透传：剥离 X-Frame-Options/CSP、WebSocket 升级、Header 注入、绝对链接改写、忽略证书 |
| WebDAV | `/dav` 挂载，App Token 认证（`asm_` 前缀、SHA-1 存库、只读/读写、有效期），OPTIONS/PROPFIND/GET/PUT/MKCOL/DELETE/MOVE |
| 单二进制 | `rust-embed` 内嵌 dist，非 `/api`、`/proxy`、`/dav` 路径回落 `index.html`（History 路由），`/assets/*` 强缓存；**子路径部署**：`[server.proxy].base_url` 非空时整站挂到该前缀（`/` 重定向过去，index.html 运行时注入 `window.__ASMODAY_BASE__`，JS/CSS 资源引用构建期即为相对路径，任意前缀下都成立） |
| 运行日志 | `data/logs/asmoday.log`（INFO/WARN/ERROR，UTC ISO8601），单文件 10 MiB、保留 5 份轮转，防撑爆 |

## 三、红线落地

1. **零外部 CDN**：图标是内联 SVG 路径表；Tailwind/CodeMirror/xterm/Video.js 全部走 npm 本地包并打进二进制；Web 字体使用系统栈。
2. **配置只用 TOML**：`toml` 库解析，`deny_unknown_fields`；源码模式原样写回，不引入 YAML。
3. **不做服务端转码**：`image` 只处理静态图片缩略图（有 FFmpeg 时借 `libwebp` 编码 WebP，FFmpeg 不可用则内置 JPEG）；视频封面仅靠**外部可选** FFmpeg 截首帧（`-frames:v 1`），不链接 libav*、不转码；没有 FFmpeg 时统一占位图。
4. **没有免密后门**：结构上不存在关闭认证的字段；校验层再拦一次「本地登录 + 代理信任同时关闭」；改密/重置只能走登录态或 CLI。

## 四、API 一览

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| GET | `/api/health` | 运行状态（公开） |
| GET | `/api/auth/status` | 初始化/登录态（公开） |
| POST | `/api/auth/setup` | 首次初始化（仅一次） |
| POST | `/api/auth/login` | 登录（限流） |
| POST | `/api/auth/logout` | 登出 |
| GET | `/api/auth/me` | 当前用户 |
| POST | `/api/auth/password` | 修改密码（旧会话全失效） |
| GET/POST | `/api/auth/totp/setup` `/enable` `/disable` | 双因素 |
| GET/POST | `/api/config` | 解析后的配置 |
| GET/POST | `/api/config/raw` | config.toml 原文 |
| GET | `/api/files/roots` | 挂载点（未配置时为工作目录） |
| GET | `/api/files/list?path=&page=&show_hidden=false` | 目录列表，单页 1000，`has_more`；`show_hidden=true` 返回 `.` 开头隐藏项 |
| GET/PUT/DELETE | `/api/files/raw?path=` | 流式读取（Range）/ 编辑器写回 / 删除（目录递归，挂载点根 403） |
| PUT | `/api/files/upload?dir=&name=` | 单文件上传（≤512 MiB） |
| GET | `/api/files/search?path=&q=` | 当前目录模糊搜索 |
| GET | `/api/files/thumb?path=&w=` | 缩略图（限流 + 缓存；**WebP 优先**；视频走外部 FFmpeg 截首帧） |
| GET/POST | `/api/system/ffmpeg-status` `/api/system/test-ffmpeg` | FFmpeg 探测（含 `webp_support`）/ 指定路径连通性测试 |
| GET | `/api/files/subtitle?path=` | 外挂字幕（`.srt` 现场转 WebVTT，`text/vtt`） |
| GET | `/api/media/browse?kind=&depth=` | 媒体索引（含 ThumbHash `hash`/`color`、同名字幕 `subtitle` 与音频 `title/artist/album/cover`） |
| GET | `/api/media/cover?path=` | 音频内嵌封面（ID3 `APIC/PIC`，无封面 404 + `COVER_NOT_FOUND`） |
| GET/POST | `/api/media/cache` `/api/media/cache/clear` | 缓存统计 / 手动清空 |
| POST/GET | `/api/media/progress` `/api/media/history` | 播放进度上报与续播（play_history） |
| POST/GET | `/api/media/favorite` `/api/media/favorites` | 收藏切换与清单（favorites） |
| GET/POST | `/api/shares`，DELETE `/api/shares/{token}` | 分享创建 / 管理（「分享与图床」页） |
| GET | `/api/public/share/{token}` `…/raw` `…/download?sub=` `…/list?sub=` | 公开分享：元信息 / 图床直链 / 下载 / 目录浏览（校验令牌、口令、有效期与 Referer 防盗链） |
| WS | `/api/terminal/ws?path=&cols=&rows=&session_id=` | PTY 字节流 |
| GET/POST/DELETE | `/api/tokens` | WebDAV 令牌 |
| any | `/proxy/{id}/*` | 外部服务反代 |
| any | `/dav/*` | WebDAV（Basic + App Token） |

## 五、安全模型

**文件路径三道闸**：挂载点白名单 → 显式拒绝 `..` 组件 → `canonicalize` 后做 `starts_with` 包含性校验。

**WebDAV**：Basic 密码必须是 `asm_` 令牌（不是主密码）；令牌只存摘要、可设只读与有效期。

**代理**：只允许配置里声明的 `[[services]]`，路径不可拼接到任意主机。

**上传**：文件名只允许单层（拒绝分隔符与 `..`），落盘前再次校验父目录仍在授权根内。

## 六、目录结构

```text
asmoday/
├── Cargo.toml            # axum / tokio / portable-pty / rusqlite / rust-embed / image / thumbhash / reqwest ...
├── config.toml           # 首次启动生成（TOML，带注释，含 [media] 缓存策略）
├── data/                 # asmoday.db + thumbnails/ + logs/（运行时创建）
├── src/
│   ├── main.rs           # CLI、状态装配、路由合并、base_url 子路径挂载、优雅退出
│   ├── api/              # auth / config / error / files / media / proxy / static_files / system / terminal / tokens / webdav
│   └── core/             # auth / config / ffmpeg / fs / logging / terminal / thumbnail / totp
├── web/                  # Vue 3 + Vite + TS + Tailwind v4 + vue-i18n
│   └── src/{api,components,composables,constants,lib,locales,router,stores,views}
├── scripts/              # 开发自测：shot.mjs（CDP 截图/交互）、ws-probe、mock-upstream
└── dist/                 # 前端产物（构建时被 rust-embed 吸入二进制）
```

## 七、已知边界（后续可做）

- **端口热变更**：配置已支持 `notify` 文件监听热重载，但改端口仍需重启进程（listener 不会 rebind）。
- **缩略图**：视频封面依赖外部可选 FFmpeg（未配置或截帧失败回占位色）；照片缩略图走信号量 + 容量/过期双策略缓存，WebP 优先、按需降级。
- **全局媒体组件**：灯箱 `MediaLightbox` 全站单实例（Teleport 到 body），文件管理与相册共用；关闭播放器时对 `<video>` 执行 `pause(); src=''; load()` 彻底销毁，防止后台继续缓冲（8.4.9）。
- **base_url 热变更**：前缀热重载生效于新请求，浏览器已打开的页面需刷新。
- **分享目录层级**：目录分享支持浏览与下载，但不提供子目录打包 zip 下载。

## 八、开发自测

```powershell
cd asmoday
C:\AimerFiles\dev-env\rust\cargo\bin\cargo.exe test        # 单元测试
node scripts/ws-probe.mjs 127.0.0.1:4242 .                  # PTY + 断线重连
node scripts/mock-upstream.mjs 9099                          # 反代上游桩
node scripts/shot.mjs http://127.0.0.1:4242/ .shots/x.png dark 1440 900
```
