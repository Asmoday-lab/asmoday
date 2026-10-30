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

# 热备份数据库到指定文件（服务运行中也能执行，VACUUM INTO 保证一致）
asmoday --backup ./asmoday-backup.db
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
| GET | `/api/system/backup` | 下载数据库热备份 |
| GET/POST | `/api/auth/totp` | 2FA 动态验证码设置 |
| any | `/proxy/{id}/*` | 外部服务反向代理透传 |
| any | `/dav/<mount_id>/*` | 标准 WebDAV 服务 (独立 Token 或管理员账号 + 登录密码) |

</details>

<details>
<summary><b>点击展开查看历史更新日志</b></summary>

- **v4.4.2 (图片灯箱背景彻底干净)**：预览时不再有「图片背后一圈模糊大底」。原图加载完成后占位层直接淡出到透明，灯箱底色改为不透明纯色（不再让背后的照片墙透出来）；占位层与缩略图生成都改成**只缩不放**——之前 `object-contain` 加 `image.thumbnail` 会把 340×475 的小图放大成 510×712 的模糊块，比原图大一圈，原图淡入后仍留在后面，这就是「背景还有、发虚」的来源。

- **v4.4.1 (移除敏感操作 5 分钟二次验证闸门)**：开终端、建/删令牌、保存配置、下载备份不再要求「5 分钟内重新输过密码」，登录会话直接可用，`/api/auth/reauth` 与 `/api/auth/verify` 接口及前端弹窗一并移除。会话防线回到登录本身：7 天滑动续期 + 30 天绝对上限 + 2FA + CSRF 同源 + 防爆破限流不变。`sessions.verified_at` 列保留仅为存量库平滑升级，代码不再读写。

- **v4.4.0 (WebDAV 支持管理员账号直连，去掉「建令牌先输一遍密码」的冷启动摩擦)**：WebDAV Basic Auth 在令牌校验失败后回退校验**管理员账号（用户名 + 登录密码）**——首次部署还没有任何令牌时，直接拿登录账号就能挂载，不必先登录网页、过 5 分钟二次验证闸门、再创建令牌。安全边界不变：令牌的只读 / 挂载点范围 / 有效期语义原样保留（推荐继续用令牌，范围收口是它的价值）；密码直连 = 全量管理员权限，一次性密码未改密与已开启 2FA 的账号一律拒绝（Basic Auth 表达不了第二因子，这类账号请用令牌），防爆破仍走 WebDAV 独立池。Argon2 逐请求校验太贵（客户端一次同步几十个请求），进程内缓存验证结果：键是「用户名+密码」的 SHA-1 不落明文，值存验证时的哈希快照——改密后旧凭证即时失效，不依赖缓存过期。

- **v4.3.1 (媒体库也吃上目录快照：两万张图的库首屏 203 ms → 45 ms)**：搜索用的那套目录快照思路，现在媒体库也有了（`media_snapshots` 表，条目多带 `size` / `mtime`，约 35 字节/条）。以前每翻一页都从根目录重走整棵树、逐条 `stat`，现在只 `stat` 目录自己一次，名字、体积、时间全从库里取——命中时 **零 `readdir`、零逐条 `stat`**。判定依据仍是目录 mtime（创建/删除/改名都会推进它），并且只收留「mtime 至少 2 秒前」的目录。唯一漏网的是**原地覆盖写**（同名替换、编辑器保存不改变名字集合，目录 mtime 不动）：应用自己的覆盖上传与文本保存落盘后会主动废掉该目录的快照，其余情况由工具栏「重新扫描」按钮带 `?refresh=1` 强制穿透（穿透后照样写回，下一次又走缓存）。实测两万张图 / 101 个目录：浏览 203 ms → 45 ms、相册 232 ms → 38 ms、翻完 67 页每页 ~50 ms，**重启进程后首次也是 66 ms**（缓存是持久的）。顺带把查询串里的布尔参数统一成宽松解析（`1/0`、`true/false`、`yes/no`、`on/off`），`?refresh=1` 不会再让整条查询串 400。

- **v4.3.0 (目录快照缓存：递归搜索五万条目从 406 ms 到 60 ms)**：搜索不再每次重读整棵树——`dir_snapshots` 表按目录存一份「有哪些名字」的快照（12 字节/条，五万条 600 KB），下次只 `stat` 目录本身、mtime 对得上就直接用快照，不再 `readdir`。判定依据是「目录内容的名字集合只在创建/删除/改名时变，而这三件事都会推进父目录 mtime」；**只收留 mtime 至少 2 秒前的目录**——时间戳分辨率是系统时钟粒度（Windows 约 15.6 ms），留 2 秒余量才能保证「入缓存之后发生的任何变化」都得到严格更大的 mtime，同一 tick 内的两次操作不会被误判成没变过，正在上传/解压的目录也天然不缓存。快照只存名字与类型，**大小一律等命中时现 `lstat`**；顺带把「每条都 stat」的去掉了——`readdir` 自带名字与类型，原来十万条的目录要打十万次 `stat` 只为显示一个 size。缓存是尽力而为的旁路数据：SQLite 出任何错都当 miss，库被删/表被改最多退回原来的慢路径，`MAX_DIRS=4096` 按使用时间做 LRU 裁剪。实测 5 万文件 / 201 目录 / 关键词只命中 10 条：首次 406 ms，之后 57–77 ms（**重启进程后首次也在这个量级**，缓存是持久的）。

- **v4.2.6 (递归搜索 + 栈游标分页，修无限滚动的「只续一页」)**：搜索不再只看当前目录第一层、也不再凑满 200 条就 `truncated`——工具栏搜索框里多一个范围开关（默认只搜当前目录，点一下搜整棵子树），两种模式都不设结果上限，翻到底才停。分页用的是**遍历栈游标**：游标记「每一层目录里下一个待访问的条目名」，续页只重开栈里那几个目录（深度通常 < 10），不用从头再走一遍树，目录中途增删最多重复一条而不是 offset 那样整体错位；单页还有 50k 条目的扫描预算，用完了交游标接着来，几十万条目的极窄关键词也能一路翻到底。搜索结果列表标出命中的所在子目录（网格显示在文件名下，列表挤在名字后面），底栏显示「搜索到 N 项 · 含子目录 · 已扫描 M 项」。顺带修掉一个一直存在的坑：`VirtualList` / `VirtualGrid` 自己就是滚动容器，塞在它们外面的 1px 哨兵永远「可见」，`IntersectionObserver` 只会在首屏响一次——1000 条以上的目录翻到第二页就再也不动了；现在触底判定搬进组件内部（`bottom` 事件 + 距底 240px 去重 + 追加后重新打量），一页填不满视口会自动补页。

- **v4.2.5 (媒体库游标分页，去掉 4000 / 20000 扫描上限)**：照片时间线、影视库、音乐库、相册卡片、册内图片全部改成游标分页（一页 300，游标 = 上一页最后一条的 `(mtime, 路径)`），和文件管理同一套 keyset 口径：翻页不重不漏，`total` / `counts` 是真实总数，头部直接显示「已加载 N / 共 M」。扫描拆成两段——第一段只 `readdir` + 分类，第二段只给**当前页**解析 ID3、探外挂字幕、查 ThumbHash，几千首歌的库不再首屏开几千个文件句柄。`SCAN_LIMIT` 与 `truncated` 字段彻底删除，再大的库也能翻到最后一条；前端每个列表末尾用 1px sentinel + `IntersectionObserver` 预取一屏，追加按路径去重。

- **v4.2.4 (去掉目录扫描上限 + 键位/拖拽对齐桌面管理器)**：文件管理不该有「看不到」的条目——删掉单目录 1 万条的硬截断，任何排序下都能翻到最后一条、`total` 与真实条数一致。性能换成两件正事：排序键 24 路并发补全（原来按 mtime/size 排序是串行两万次 `stat`，Windows 上切一次排序卡 1–3 秒）、条目容器只保留 `i64` 排序键（不再为每条常驻整个 `Metadata`，十万条目录内存降到四分之一）；同时补上 `F2` 重命名、`Delete` 删除、`Ctrl/⌘ + C/X/V` 内部剪贴板、`Ctrl/⌘ + A` 全选、`Shift` 连选、`Esc` 逐层关闭弹层，以及「把条目拖到文件夹 = 移动，按住 `Alt` = 复制」——全部复用已有弹窗与冲突三选，命中同名**绝不静默覆盖**，而是回退到移动/复制对话框。另修 `resolve_child` 的一处写穿：父目录在挂载点内、名字是**指向外部的悬空链接**时，`touch`/覆盖写会顺着链接写到挂载点外（现在按链接目标判归属，`..` 先折叠再比）。

- **v4.2.3 (缩略图缓存重构，修指数增殖)**：`data/thumbnails/` 里的图片被当普通图片反复缩略图、产物又写回同一目录，导致缓存按宽度倍数指数增长（默认配置「挂载 cwd + 数据目录 cwd/data」必现）。按 FileBrowser 的 diskcache 重构：缓存键只由路径决定（改图原地覆盖，不再每次多一个文件）、文件名不带扩展名（`320-jpg`）、`<sha1[..2]>/<sha1>/` 两级扇出、同目录临时文件 + rename 原子落盘、同一源文件 per-key 串行；删除 / 改名 / 移动自动清掉该文件的缓存单元与 ThumbHash 行。同时把整个应用数据目录登记为「保留目录」：列表不显示，`/api/files/*`、WebDAV、媒体扫描、搜索一律不碰，顺手堵住「通过文件管理删掉 `asmoday.db`」这个更大隐患。

- **v4.2.2 (软链接逃逸可配置)**：新增 `[system].follow_symlinks`（默认 `true`，与 FileBrowser 行为一致）——挂载点内指向其它位置（含跨盘）的符号链接 / junction 现在能点进去浏览、预览、上传、改名，**寻址始终以链接路径为前缀**；直接访问挂载点外的绝对路径依旧 403（放开的只有「链接解析」，判定条件是「字面上在挂载点内 + 沿途至少一段是链接」）。改为 `false` 恢复严格边界（逃逸按断链处理，跨盘链接不可用）。设置页「高级 → 挂载点」提供开关，配置渲染保持幂等：等于默认值的键不写盘，改成非默认值才落文件。Windows 上 `mklink /J` 建的 junction 与目录符号链接同等对待。

- **v4.2.1 (可用性修复与工程收口)**：**修 WebDAV 客户端「全不可用」**——浏览器/客户端打开目录页是空响应（`ERR_EMPTY_RESPONSE`）、服务端日志却记 200/207，根因是全局压缩层把 `dav-server` 的流式 body 压坏了（带 `Accept-Encoding: gzip/br` 才触发，curl 不带所以回归测不出来），DAV 出口改标 `Cache-Control: no-transform` 跳过压缩；**2FA 开启页补上二维码**（后端用 `qrcode` 直接下发 SVG，深色主题固定白底，附解码往返测试）；修 `apply_shell_args` 两个平台签名不一致导致的 Linux 编译失败（本机 Windows 全绿、CI 必炸）。工程上：**GitHub Actions 只保留发版**——删掉每次 push 都跑一遍的 `ci.yml` 与每周开依赖 PR 的 Dependabot，`fmt`/`clippy`/`test`/前端构建改回提交前在本机做（本地已经编译过，云端重复装依赖没有收益）。

- **v4.2.0 (WebDAV 客户端兼容修复 + 全面拥抱成熟方案)**：**WebDAV 协议栈换成 `dav-server`**——自研实现里客户端实测加不上（`/dav`、`/dav/` 虚拟根直接 404，`PROPFIND` 缺 `getetag`/`creationdate`/`supportedlock`，目录 href 无尾斜杠，全是客户端判断「能不能用」的判据）；现在协议全套交给社区实现（多段 Range、ETag 条件请求、真锁 `MemLs`、`PROPPATCH`、autoindex），`/dav/` 成为虚拟根（`PROPFIND` 列出全部挂载点，客户端只需填这一个地址），令牌限定挂载点时只列可见项；安全边界（软链接逃逸、`Destination` 头、系统垃圾、只读、防爆破池）全部保留在入口层；Windows 适配：`MKCOL` 尾斜杠与 `ERROR_ALREADY_EXISTS`→405 两处 dav-server 的坑已修。同时：数据库迁移换成 `rusqlite_migration`（`PRAGMA user_version` 版本化，每步只跑一次，以后加列只追加 `M::up`）、全局分配器换 `mimalloc`（musl 目标除外）、前端状态管理收口到 `pinia`（5 个全局 store：播放器 / 灯箱 / 二次验证 / 分享 / 终端标签）并用 `@vueuse/core` 替掉手写的 `addEventListener` 与偏好持久化。

- **v4.1.0 (依赖与工程基线：换成社区成熟方案)**：新增 HTTP 压缩（`tower-http`，br 优先 gzip 兜底，只压文本类、206 不压；实测主包 89 KB → 30 KB）、单实例锁改用标准库文件锁（`File::try_lock`，1.89+，替掉会漏锁的 SQLite 排他锁写法）、音频标签改用 `lofty`（FLAC / M4A / OGG 也有标签与封面了，扫描时关掉 `read_properties` 不再为算时长把整首歌读一遍）、补 CI（fmt + `clippy -D warnings` + test + 前端构建）与 Dependabot，全仓 rustfmt 化并清零 clippy 警告。

- **v4.0.0 (安全与运维补强)**：文件操作语义纠偏（删除不再为拒绝先扫全树、条目数只数选中项、同挂载点移动零统计、阈值可配 `[server.limits]`）、默认不信任 `X-Forwarded-Proto`（改由 `trusted_ips` 白名单）、单实例锁（SQLite 排他锁，防双开互删 `.part`）、`.part` 启动清理留 10 分钟宽限期、WebDAV 真锁（30 分钟内存锁 + `If` 头校验，`MS-Author-Via`/`PROPPATCH` 补齐 Windows 映射可写）、令牌可限定挂载点（`mount_scope`）、敏感操作二次验证（终端/令牌/配置写入/备份下载，5 分钟窗口）、数据库热备（设置页一键下载 + `--backup`）、`X-Forwarded-Prefix` 支持（反代带前缀但没配 `base_url` 也能跑）、收藏/播放历史上复合主键 `(mount_id, file_path)`、终端保活与并发可配、缩略图并发按 CPU 自适应、字体瘦身为两个 woff2（省约 5MB）。

- **v3.15.0 (文档对齐)**：PTY 尺寸强制钳制（0/超大值不再击穿 ConPTY）、访问日志查询串按需脱敏、跨挂载移动「删源失败」独立错误码 `E_SOURCE_DELETE_FAILED`、垃圾文件补 `.crdownload/.part/.!ut/.tmp`、Termux `$PREFIX/bin/bash` 兼容、终端活动会话面板与强制关闭、编辑器语言矩阵扩至 30+ 语言、挂载点动态移除/离线兜底、缩略图下载中跳过、配置热重载四重防死循环、批量失败项防呆重试、配置表单保存改为 AST 级局部改写（`toml_edit`，用户手写的注释/空行/键序原样保留）、反代不再把 asmoday 会话 Cookie 透给上游（上游同名 `Set-Cookie` 同样丢弃）、终端 WebSocket 强制 Origin 同源（防 CSWSH）、一次性密码会话的后端强制改密拦截、**IPv6 支持**（`host = "::"` 双栈监听，显式关 `IPV6_V6ONLY`，Windows 默认 on 会挡掉 IPv4；`trusted_ips` 支持 IPv6 CIDR；`::ffff:x.x.x.x` 归一回 IPv4，防限流/白名单键分裂；CSRF/CSWSH 同源判定理解 `[::1]` 写法）、**终端体验对齐 ttyd**（`[terminal].shell/args` 覆盖与 tmux 接入、zsh 探测回退 `$SHELL → zsh → bash`、WS 支持 `mount_id` 定位 CWD、打包 JetBrainsMono Nerd Font 修图标豆腐块、VS Code Dark+ 16 色板、Tab 键拦截进 PTY、`nextTick` 后 fit 防列错位）、**软链接全链路修复**（目录列表返回 `symlink_target`/`target_is_dir`，目标逃出挂载点或断链标记为断链；删除/改名/移动改走 nofollow——删链接只删链接本身，绝不递归删目标实体；前端软链接带快捷方式箭头角标、悬停显示目标、断链置灰并提示）、**WebDAV 路由容错与设置页联动**（`GET /dav/<mount_id>` 无尾斜杠 301 到带斜杠形式，认证仍 401 优先；设置页动态拉取挂载点下拉、URL 完整拼接 `origin + base_url + /dav/<id>/` 强制尾斜杠、无挂载点置灰提示）。
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