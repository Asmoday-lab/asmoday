# Asmoday

个人媒体与设备控制中枢 · Rust(Axum) + Vue 3(Vite) + TailwindCSS v4 · **单二进制发布**。

当前状态：**v3.5.3**（终端历史强制隔离 + 图片库口径回到 v2.8 + 面包屑点击回滚）。**终端（3.3.5）**：历史记录**一律**钉在 `<data>/terminal_history/`，不再探测、不再碰 `%APPDATA%\...\PSReadLine` 或 `~/.bash_history`——受限令牌（低完整性级别）、服务账户、只读主目录都不会再出现「访问被拒绝」红字；前端与终端里**没有任何**「权限受限/已重定向」横幅或 Toast（上一版的自愈提示已全部移除，启动日志的受限 WARN 一并删除）。Windows 载荷顺序改为 `chcp 65001` 优先 → `Set-PSReadLineOption -HistorySavePath`；PowerShell 保持 `-NoExit`（`$PROFILE` 照常加载，Conda/fnm 环境可用）+ `-Command`，纯净模式逃生舱仍是 `-NoProfile`（Unix `--noprofile --norc`）；父进程环境变量用 `std::env::vars_os()` 显式全量继承；profile 报错原样透传、不中断会话。**图片库（3.4.3）**：相册口径回到 v2.8——**有图片的目录就是一本相册，挂载目录本身也算**（根目录直接放的照片不再消失），卡片条数只数该目录第一层，点进去 `depth=0` 只列第一层，二者严格一致；嵌套目录同样列出；根相册显示「根目录」。侧边栏「照片相册」改名 **图片库**。**面包屑（3.2.1）**：回到 v2.8/v2.9 语义——`🏠` 回卡片页（单挂载点回根），点挂载点名**进入该挂载点根目录**，其余各级直接跳转（上一版把前两级都改成回卡片页，现已回滚）。118 条后端单测全绿；E2E 实测：`/api/media/albums` 返回 `根目录(2, is_root)` + `album-a(1)` + `album-b(1)` + `sub(1)`，根相册详情 2 张（与卡片一致），点挂载点面包屑 → `/files/m0`，终端内 `echo vfy-history-ok` 后历史落在 `data/terminal_history/ConsoleHost_history.txt`、页面无任何权限提示、`(base)` 提示符证明 profile 已加载、0 console error。

历史（v3.5.0，全量审计查漏补缺）。对照设计文档 v3.4.0 逐条复核后补齐一批从未落地的条款：**默认安全响应头**（nosniff / Referrer-Policy / Permissions-Policy / CSP（禁 unsafe-eval、放 blob: 与内联防白屏脚本）/ frame-ancestors，HTTPS 才发 HSTS；`/proxy/*` 透传除外）；**SSO“座上宾”纠正**（只认 `auth.proxy_trust.trusted_ips`、Header 用户不存在直接拒绝**不再自动建号**、2FA 默认不跳过）；**登录频控**（只有 socket 来源命中 `trusted_ips` 才采信 XFF；纯 IP 与 IP|用户名 双计数）；**Cookie 与会话**（`Path` 跟 `base_url`、HTTPS 才加 `Secure`、7 天滑动续期 + 30 天绝对上限）；**2FA 一次性恢复码**（开启即发 10 个只显示一次、库中仅存哈希、登录可代替 TOTP、关闭 2FA 必须密码+动态码/恢复码且恢复码随之作废）；**目录列表改游标分页**（3.2.4 禁 offset：`next_cursor` 固定排序游标，换排序/手改游标自动退回第一页，`page` 参数为老客户端保留）；`/api/files/raw` 补 **ETag / Last-Modified / 304 / HEAD**。终端组件补移动端软键盘 `visualViewport` 重排。前端 i18n 与设置页/登录页同步（恢复码一次性展示块、关闭 2FA 增加验证码输入框）。117 条后端单测全绿；E2E 实测：安全头/304/游标翻页（1011 项 0 重复）/频控双维度（6 次失败 429 且同 IP 正确密码也被挡）/SSO 拒绝建号/恢复码登录与禁用全流程（Playwright + curl）。

历史（v3.3.1，终端受限环境自愈）：排查实机复现的「PSReadLine 历史文件访问被拒绝」：根因是服务进程运行在**低完整性级别（Low IL）令牌**下（受限环境启动：沙箱、部分服务/计划任务场景），子 shell 继承父令牌后对用户目录（Medium IL）一切写入被拒——不只历史报错，conda/npm 写配置同样会挂。修复：拉起会话前 `probe_shell_env` 探测默认历史路径可写性，不可写时把历史**重定向到数据目录**（Windows 注入 `Set-PSReadLineOption -HistorySavePath`、类 Unix 设 `HISTFILE`；`data_dir` 先展开为绝对路径，相对 `-d` 不再被 shell cwd 解析歪），同时给 ready 帧发 warning + 在终端打一次性黄色说明；正常环境探测即过、零干预，行为与本地完全一致。另：会话回收从直接 kill 改为**优雅退出**（先 `exit` 让 PSReadLine 收尾、700ms 兜底强杀），避免孤儿 shell。

历史（v3.3.0，双视图/路由/预加载纠偏）：照片库按 3.4.3 恢复**双视图**：新增 `GET /api/media/albums`（子文件夹卡片，递归统计图片数、封面取册内最新一张图并带 ThumbHash 打底，**无图文件夹后端直接剔除**），照片页顶部 `[相册] / [时间线]` 平级切换（选择记忆在 localStorage，相册详情递归展示、张数与卡片口径一致）；全局灯箱按 3.4.4 补齐**渐进式加载**（点击先用缩略图模糊打底铺满舞台、原图加载完 300ms 淡入，绝不黑屏）与**静默预加载**（停留即后台拉取前后各一张原图），原图 `z-index` 高于遮罩与打底层；文件管理按 3.2.1 纠偏路由：新增 `/files/:mountId/:path*`，单挂载点必须 `router.replace` 直达 `/files/m0`（跳转期间全屏 Loading 遮罩防闪烁），页内换目录同步 URL、支持深链接刷新还原，面包屑回卡片页时 URL 一并回 `/files`；终端按 3.3.5：类 Unix 默认以 **Login Shell** 启动（`bash -l` / `zsh -l`，Conda/fnm 初始化依赖 profile），纯净模式仍走 `--noprofile --norc`；安全与存储补漏：Cookie 认证的写操作新增 **CSRF 同源校验**（Origin/Referer 必须与 Host 一致，缺失放行、`null` 拒绝，Token/WebDAV 不受影响），SQLite 连接补 `busy_timeout=5000`、库文件 Unix 权限 0600，PTY 环形缓冲提到 1MB；细节：网格视图复选框移到左上角、Token 删除按钮 `hover:text-red-500`。

历史（v3.0.0）：（多选/右键/主题切换重构）：右键菜单抽成全局 `ContextMenu` 组件，文件管理、照片灯箱、影视库海报墙、音乐列表**共用同一组件**（媒体库不再"只能看不能操作"：打开/播放、下载、分享、复制直链、收藏就地可达）；分享表单同步抽成全局 `ShareDialog` 单例（`stores/share.ts` 唤起），各处分享行为完全一致；多选入口补齐规范 —— 网格视图悬停浮现复选框、列表视图左侧常驻复选框，点击即进多选并滑出底部操作栏（Ctrl/⌘ 点击与移动端长按 500ms 原有入口不变）；主题切换入口按 8.2 迁移：删除侧边栏底部孤立图标，收进右上角账号菜单（浅色/深色/跟随系统三项带勾选），与设置页"通用-外观"下拉双向同步；照片库按 3.4.3 扁平化：**只保留时间线**（年月分组吸顶标题），删除"相册"文件夹视图，子目录图片直接捞平；照片与影视卡片新增 ThumbHash **模糊占位层**（前端 `lib/thumbhash.ts` 逐行对齐后端 `thumbhash 0.1.0` 解码 DCT 还原 32px 小图，真图加载完成前就有正确构图与配色，经与后端平均色比对验证一致）。

历史主题（v2.10.0，3.3.6/3.3.7 终端一致性 + 跨平台垃圾过滤）：PTY 全量继承进程环境变量（`PATH`/`HOME` 原样带下去，绝不 `env_clear`）、stderr 零过滤透传、Windows 启动参数强制 `chcp 65001`（放在 profile 之后执行；被 profile 改回去的情况用工具栏「UTF-8」按钮一键修复）、工具栏新增「纯净模式」开关（Windows 加 `-NoProfile`、类 Unix 切 `bash --noprofile --norc`，配置文件死循环救急）。`desktop.ini`/`Thumbs.db`/`.DS_Store`/`._*`/`~$*`/`lost+found`/`.Trashes`/`$RECYCLE.BIN`/`System Volume Information` 等系统垃圾在文件列表、搜索、媒体库扫描、WebDAV PROPFIND 中**无条件过滤**（优先于 `show_hidden`，`.env`/`.git` 等普通隐藏项不受影响），WebDAV 还直接拒绝这些名字的 PUT/MKCOL，防止 macOS 客户端挂载后无限回写污染。v2.9.0（3.2.2.1 编辑器换行符与编码规范）：新增 `GET /api/files/text` 返回正文 + `encoding` / `eol_type` / `has_bom` / `mixed_eol` 元信息，`PUT /api/files/raw` 支持 `encoding` / `eol` / `bom` 参数；读取时 UTF-8 优先、失败探测 GBK/GB18030、UTF-16 只认 BOM，解不出干净文本直接报错绝不回乱码；换行符按文件原样保留（传输统一 LF、写盘前还原，混合按多数派归一），BOM 有就剥、有就加回、没有绝不添加；目标编码表示不了的字符（GBK 文件粘 emoji）在保存前拒绝并 422，不落乱码；老客户端不带参数时按磁盘现状推断风格。编辑器底部新增状态栏（编码 · 换行符 · BOM 标记），点击可手动转换 LF/CRLF 或 GBK→UTF-8，转换计入未保存状态；只读挂载点禁止写回（此前漏检）。v2.8.2（ThumbHash 改存 SQLite）：ThumbHash 只有 20–30 字节，原先每张缩略图旁边落一个 `.th` 旁路文件，几十万张图会让低功耗设备耗尽 inode、拖慢目录扫描 —— 现改为 Base64 文本存进 SQLite `thumb_hashes` 表（键 = 路径 + mtime + 大小的摘要，与宽度无关，LRU 保留最新 4 万行），生成时随 `get()` 带出由处理器入库，媒体扫描一次连接批量查库；存量部署的 `.th` 文件在下次浏览时惰性搬进库并自动删除，无需手工迁移；「清理媒体缓存」同时清表。v2.8.1 主题（交互细节打磨）：影视库视频卡片出后端首帧封面、影视库/音乐库一律展示挂载点内相对路径、终端「新建」按钮紧贴最后一个标签页、属性弹窗改版（`/api/files/stat` 新增 `width`/`height`，图片读文件头、视频经外部 FFmpeg 探测；「真实路径」改为相对「所在目录」；Windows 拿不到的权限/所有者/inode 整行隐藏；大小旁浅色小字附精确字节数）。v2.7→v2.8 主题：8.2 主题入口规范（三态 `light/dark/system` 默认跟随系统 + `matchMedia` 实时跟随；侧边栏底部**仅图标**循环切换，设置页「通用-外观」下拉框）+ 一批交互修复：媒体灯箱翻页按钮贴边固定、缩略图条可滚轮横滚并自动定位、视频瓦片/灯箱缩略图全部出后端首帧封面、播放器传 poster、右键菜单按真实尺寸定位（矮窗口不再挤出视口）、源码编辑器 14px + 视口自适应高度、全局字号上调一档、浅色主题语义色对比度修复；并修掉「属性/删除统计弹窗永远停在加载中」的响应式身份比较 bug（`ref.value` 是 proxy，与原始对象恒不相等，改用请求序号判定）。v2.6→v2.7 新增：监听地址策略（`[server].host` + `-H`/`ASMODAY_HOST`）、0.0.0.0 无 SSL 启动警告与设置页自检警告条（`GET /api/config/safety`）、未初始化时 WebDAV 一并 503、文件列表排序（`sort=name|mtime|size&order=asc|desc`）、工具栏排序菜单与无限滚动、网格缩略图 ≥100x100 与列表表头、终端强制深色 + 外壳跟随主题、设置页 `max-w-5xl` 布局规范与 Token 列表表头。除认证、配置双模编辑与热重载、上传与搜索、分享/图床、收藏与续播外，本版新增**文件变更操作全链路**（新建/重命名/移动/复制/删除 + 多选批量 + 一次性令牌）、挂载点模型与单挂载点直达、统一错误码与日志规范、软链接与并发安全、危险配置自检、缩略图批量重建与三类清理入口。v2.9.0（3.2.2.1 编辑器换行符与编码规范）：新增 `GET /api/files/text` 返回正文 + `encoding` / `eol_type` / `has_bom` / `mixed_eol` 元信息，`PUT /api/files/raw` 支持 `encoding` / `eol` / `bom` 参数；读取时 UTF-8 优先、失败探测 GBK/GB18030、UTF-16 只认 BOM，解不出干净文本直接报错绝不回乱码；换行符按文件原样保留（传输统一 LF、写盘前还原，混合按多数派归一），BOM 有就剥、有就加回、没有绝不添加；目标编码表示不了的字符（GBK 文件粘 emoji）在保存前拒绝并 422，不落乱码；老客户端不带参数时按磁盘现状推断风格。编辑器底部新增状态栏（编码 · 换行符 · BOM 标记），点击可手动转换 LF/CRLF 或 GBK→UTF-8，转换计入未保存状态；只读挂载点禁止写回（此前漏检）。v2.8.2（ThumbHash 改存 SQLite）：ThumbHash 只有 20–30 字节，原先每张缩略图旁边落一个 `.th` 旁路文件，几十万张图会让低功耗设备耗尽 inode、拖慢目录扫描 —— 现改为 Base64 文本存进 SQLite `thumb_hashes` 表（键 = 路径 + mtime + 大小的摘要，与宽度无关，LRU 保留最新 4 万行），生成时随 `get()` 带出由处理器入库，媒体扫描一次连接批量查库；存量部署的 `.th` 文件在下次浏览时惰性搬进库并自动删除，无需手工迁移；「清理媒体缓存」同时清表。v2.8.1 主题（交互细节打磨）：影视库视频卡片出后端首帧封面、影视库/音乐库一律展示挂载点内相对路径、终端「新建」按钮紧贴最后一个标签页、属性弹窗改版（`/api/files/stat` 新增 `width`/`height`，图片读文件头、视频经外部 FFmpeg 探测；「真实路径」改为相对「所在目录」；Windows 拿不到的权限/所有者/inode 整行隐藏；大小旁浅色小字附精确字节数）。v2.7→v2.8 主题：8.2 主题入口规范（三态 `light/dark/system` 默认跟随系统 + `matchMedia` 实时跟随；侧边栏底部**仅图标**循环切换，设置页「通用-外观」下拉框）+ 一批交互修复：媒体灯箱翻页按钮贴边固定、缩略图条可滚轮横滚并自动定位、视频瓦片/灯箱缩略图全部出后端首帧封面、播放器传 poster、右键菜单按真实尺寸定位（矮窗口不再挤出视口）、源码编辑器 14px + 视口自适应高度、全局字号上调一档、浅色主题语义色对比度修复；并修掉「属性/删除统计弹窗永远停在加载中」的响应式身份比较 bug（`ref.value` 是 proxy，与原始对象恒不相等，改用请求序号判定）。v2.6→v2.7 新增：监听地址策略（`[server].host` + `-H`/`ASMODAY_HOST`）、0.0.0.0 无 SSL 启动警告与设置页自检警告条（`GET /api/config/safety`）、未初始化时 WebDAV 一并 503、文件列表排序（`sort=name|mtime|size&order=asc|desc`）、工具栏排序菜单与无限滚动、网格缩略图 ≥100x100 与列表表头、终端强制深色 + 外壳跟随主题、设置页 `max-w-5xl` 布局规范与 Token 列表表头。除认证、配置双模编辑与热重载、上传与搜索、分享/图床、收藏与续播外，本版新增**文件变更操作全链路**（新建/重命名/移动/复制/删除 + 多选批量 + 一次性令牌）、挂载点模型与单挂载点直达、统一错误码与日志规范、软链接与并发安全、危险配置自检、缩略图批量重建与三类清理入口。

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
| 文件管理 | **无目录树**：挂载点卡片入口 + 面包屑（Home/根名/子段，不暴露真实绝对路径）、网格/列表双虚拟化、1000 条强制分页、流式读取 + HTTP Range、拖拽上传（XHR 进度）、当前目录模糊搜索、**隐藏文件开关**（默认过滤 `.` 开头项，工具栏一键切换，`show_hidden` 参数）+ **文件预览矩阵**（双击分流：图片/音视频→全局 MediaLightbox 左右切换；Markdown→`marked`+`DOMPurify` 渲染可切源码；CSV/TSV→只读表格，>5MB 或 >1000 行强制截断并提示下载；PDF→原生 iframe；Office/CAD/压缩包→**不引解析库**只提示下载；纯文本→CodeMirror）+ 缩略图失败自动回退图标 + 右键菜单（打开/终端（仅目录）/复制路径/下载/分享/图床直链/**重命名**/**移动**/**复制**/**属性**/**删除**）+ **文件变更全链路**（`mkdir`/`touch`/`rename`/`move`/`copy`/`delete`，逐项结果 + `summary`，部分成功不回滚）：挂载点归属校验、只读挂载点前置拦截、per-path 内存锁（按字典序加锁，冲突立即 `E_BUSY`）、**原子 no-replace 落盘**（`MoveFileExW` / `renameat2` / 存在性降级）、`.asmoday-part.<8位随机>` 临时文件（启动无差别清理 + 运行期 24h 清理）、自动命名 `名称 (n).扩展名`（批次共享已占用名集合）、跨挂载点复制→校验 size/mtime→再删源、空间预检、单目录删除需 `confirm_name`、批量删除需 60 秒一次性 `delete-token`、`measure` 预统计项数与总大小、**多选批量**（Ctrl/⌘ 点击或移动端长按 500ms，底部安全区操作栏，>10 个下载先确认） |
| 在线编辑 | CodeMirror 6 按需加载，js/ts/rust/python/toml/nginx 语言包各自独立 chunk，`Ctrl+S` 原子写回 |
| 终端 | portable-pty 真实 shell（Windows 显式 `powershell.exe` 绝对路径，Unix 读 `$SHELL` 回退 `bash`）、PTY 直通全屏应用、**最多 10 个会话**（超限先回收空闲）、多标签（同目录复用不叠加）、WebSocket 双向桥接、断线 3 分钟保活 + 缓冲回放、**前端指数退避自动重连**（服务重启后自愈，无需关标签）、重试用尽后状态栏「重新连接」、`dirs::home_dir()` 解析主目录（前端不传 `~`/`%USERPROFILE%`，后端对占位符兜底展开并提示）、POSIX 风格路径统一转原生分隔符后再交给 PTY、目录已删则退回默认目录并提示、文件右键「在终端中打开」、移动端 Termux 两行键盘（粘性 Ctrl/Alt） |
| 媒体 | 缩略图**WebP 优先**（有 libwebp 编码器的 FFmpeg 时输出 WebP，无则降级 JPEG/PNG；按实际格式回 Content-Type）+ 信号量（并发 2）+ 磁盘缓存 + `[media]` 可配置淘汰（`thumb_cache_max_size_mb` 默认 1024 MiB LRU + `thumb_cache_max_age_days` 默认 90 天过期，每 30 分钟后台清理，设置页可查看用量并手动清空）+ **ThumbHash 占位**（扫描时返回平均色，图未加载前不白屏）；相册/时间线（虚拟列表）；影视墙（**外部可选 FFmpeg 截首帧作封面**，WebP 优先 → JPEG → PNG 逐级降级，与缩略图共用 LRU 缓存；**路径可填可执行文件或安装目录**）+ Video.js 10（`@videojs/html` 自定义元素，neutral 皮肤）+ **外挂字幕**（同目录同名 `.srt/.vtt`，后端统一转 WebVTT）+ 原生播放器降级开关 + 播放进度续播；音乐 **ID3 标签与内嵌封面**（`APIC/PIC`，`GET /api/media/cover`）+ 全局悬浮播放条（循环/单曲/随机 + 进度拖拽 + **音量记忆**（默认 35%、`asm_volume` 持久化、静音恢复静音前音量、移动端隐藏滑块）+ **播放列表抽屉**（PC 贴条上方/移动端占下半屏，切歌/清空/高亮））+ `.lrc` 歌词滚动；收藏与「只看收藏」 |
| 分享与图床 | 文件右键「分享…」（口令/有效期/允许下载）与「复制图床直链」；公开页 `/s/{token}`（口令解锁、内联预览、目录浏览与子文件下载）；独立「分享与图床」管理页；`[shares].referer_allowlist` 防盗链 |
| 反代网关 | `[[services]]` 驱动的 `/proxy/<id>/…` 透传：剥离 X-Frame-Options/CSP、WebSocket 升级、Header 注入、绝对链接改写、忽略证书 |
| WebDAV | `/dav/<mount_id>/*` 挂载（设计文档 v2.5.0 3.10）：Basic Auth（用户名=Token 名，密码=`asm_` 明文）+ SHA-1 存库 + 过期校验；OPTIONS/PROPFIND/GET/HEAD/PUT/DELETE/MKCOL/MOVE/COPY/LOCK/UNLOCK；只读 Token 与只读挂载点双重 403；PUT 流式落盘（`.asmoday-part` 临时文件 + 原子重命名，绝不 to_bytes）；GET/HEAD 支持 Range；LOCK/UNLOCK 为伪锁；失败计入独立防爆破池；初始化完成前一律 503 |
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
| GET | `/api/config/safety` | 危险配置自检结果（`{ issues: [{ level, code }] }`，前端渲染设置页警告条，i18n key `settings.safety.<code>`） |
| GET | `/api/files/roots` | 挂载点（未配置时为工作目录） |
| GET | `/api/files/mounts` | 挂载点 `{id,name,path,readonly,online,implicit}`（只读置灰、单挂载点直达的依据） |
| POST | `/api/files/mkdir` `/touch` `/rename` | 新建目录/空文件、重命名；目标已存在一律 `E_CONFLICT`，绝不静默覆盖 |
| POST | `/api/files/move` `/copy` | 批量移动/复制 `{paths[], dest_path, on_conflict}`，`on_conflict` 只接受 overwrite/rename/skip |
| POST | `/api/files/measure` | 删除前预统计 `{items, bytes, truncated}`（数到 1000 项即停，不阻塞确认弹窗） |
| POST | `/api/files/delete-token` | 批量删除的一次性令牌（60 秒，绑定 paths 集合哈希，用后即删） |
| POST | `/api/files/delete` | 删除 `{paths[], confirm_name? \| confirm_token?}`；逐项结果，不回滚 |
| GET | `/api/files/stat?path=` | 属性（lstat 不跟随软链接；含 mode/inode/owner/real_path/symlink_target） |
| POST | `/api/files/hash` | 按需哈希（<64 MiB，并发 2，超出 `E_BUSY`） |
| GET | `/api/files/list?path=&page=&show_hidden=&sort=&order=` | 目录列表，单页 1000，`has_more`；`show_hidden=true` 返回 `.` 开头隐藏项；`sort=name|mtime|size` + `order=asc|desc`（目录恒排前，同键并列按名称兜底，mtime/size 服务端全量 lstat 后排序） |
| GET/PUT/DELETE | `/api/files/raw?path=` | 流式读取（Range）/ 编辑器写回 / 删除（目录递归，挂载点根 403） |
| PUT | `/api/files/upload?dir=&name=` | 单文件上传（≤512 MiB） |
| GET | `/api/files/search?path=&q=` | 当前目录模糊搜索 |
| GET | `/api/files/thumb?path=&w=` | 缩略图（限流 + 缓存；**WebP 优先**；视频走外部 FFmpeg 截首帧） |
| POST | `/api/system/cleanup/thumbnails` `/cleanup/parts` `/cleanup/shares` | 三类清理：缩略图缓存 / 遗留 `.asmoday-part`（24h 阈值）/ 30 天前分享墓碑 |
| POST/GET | `/api/system/thumbnails/rebuild` `…/rebuild/status` | 缩略图批量重建（单线程队列，扫描上限 5000，`yield` 让路不抢 CPU） |
| GET | `/api/system/health` | `{status, version, uptime, build}`（`build` = git hash + 提交时间） |
| GET/POST | `/api/system/ffmpeg-status` `/api/system/test-ffmpeg` | FFmpeg 探测（含 `webp_support`）/ 指定路径连通性测试 |
| GET | `/api/files/subtitle?path=` | 外挂字幕（`.srt` 现场转 WebVTT；UTF-8 失败按 GBK/GB18030 重解） |
| GET | `/api/media/browse?kind=&depth=` | 媒体索引（含 ThumbHash `hash`/`color`、同名字幕 `subtitle` 与音频 `title/artist/album/cover`） |
| GET | `/api/media/cover?path=` | 音频内嵌封面（ID3 `APIC/PIC`，无封面 404 + `COVER_NOT_FOUND`） |
| GET/POST | `/api/media/cache` `/api/media/cache/clear` | 缓存统计 / 手动清空 |
| POST/GET | `/api/media/progress` `/api/media/history` | 播放进度上报与续播（play_history） |
| POST/GET | `/api/media/favorite` `/api/media/favorites` | 收藏切换与清单（favorites） |
| GET/POST | `/api/shares`，DELETE `/api/shares/{token}` | 分享创建 / 管理（「分享与图床」页） |
| POST | `/api/public/share/{token}/verify` | 公开页口令预校验（口令错误计入**独立**分享防爆破池，不与登录池互相牵连） |
| GET | `/api/public/share/{token}` `…/raw` `…/download?sub=` `…/list?sub=` | 公开分享：元信息 / 图床直链 / 下载 / 目录浏览（校验令牌、口令、有效期与 Referer 防盗链） |
| WS | `/api/terminal/ws?path=&cols=&rows=&session_id=` | PTY 字节流 |
| GET/POST/DELETE | `/api/tokens` | WebDAV 令牌 |
| any | `/proxy/{id}/*` | 外部服务反代 |
| any | `/dav/<mount_id>/*` | WebDAV 服务端（Basic：用户名=Token 名 + `asm_` 令牌；写方法被只读双重拦截；禁止跨挂载点 Destination；`/dav/*` 不参与 SPA Fallback） |

## 五、安全模型

**文件路径三道闸**：挂载点白名单 → 显式拒绝 `..` 组件 → `canonicalize` 后做 `starts_with` 包含性校验。

**WebDAV**（设计文档 v2.5.0 3.10）：Basic 用户名必须是 Token 名、密码是 `asm_` 明文（不是主密码）；令牌只存摘要、可设只读与有效期；认证失败计入独立防爆破池（5 次封 15 分钟，与登录/分享互不影响）；未初始化实例一律 503（设计文档 v2.7.0 3.1.1）。

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
│   ├── build.rs          # 构建期注入 ASMODAY_BUILD_ID（git hash + 提交时间）
│   ├── api/              # auth / config / error / files / files_mutate / media / proxy / static_files / system / terminal / tokens / webdav
│   └── core/             # auth / config / ffmpeg / fs / fs_ops（变更内核）/ locks（per-path 锁）/ logging / path_guard（挂载点与路径闸门）/ terminal / thumbnail / totp
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
- **TOCTOU（已知限制）**：单管理员场景下，本地攻击者在「校验后、操作前」把路径换成 symlink 的行为**不额外防护**；威胁模型里没有本地低权用户。
- **不做转码兜底**：能不能播由浏览器解码能力决定，服务端只做容器/编码探测，不引 libav*。
- **`allow_download=false` 不是 DRM**：它只隐藏入口、拦住 `?download=1`，不防 F12 抓包、缓存嗅探与录屏。
- **无回收站**：删除是物理硬删，没有 undo；确认弹窗的「输入目录名 / 一次性令牌」防的是误操作与脚本漏确认，不是安全边界。
- **无断点续传 / 无异步任务队列**：上传中断需重传；单次 copy/move/delete 上限 100 项或 2 GiB，超限请去终端 `cp`/`mv`。
- **WebDAV 的 LOCK 是伪锁**：只返回随机 `Lock-Token`，不维护超时表；并发安全依托 per-path 文件互斥锁，不提供严格分布式锁语义。
- **WebDAV MOVE/COPY 仅限同挂载点**：Destination 指向其他 mount_id 直接 403；跨挂载点搬运请走前端 move（复制→校验→删源）。

## 八、开发自测

```powershell
cd asmoday
C:\AimerFiles\dev-env\rust\cargo\bin\cargo.exe test        # 单元测试
node scripts/ws-probe.mjs 127.0.0.1:4242 .                  # PTY + 断线重连
node scripts/mock-upstream.mjs 9099                          # 反代上游桩
node scripts/shot.mjs http://127.0.0.1:4242/ .shots/x.png dark 1440 900
```
