# Asmoday 测试与调试流程

> 定稿 v1（2026-11-06）。README 的「提交前门禁」是本文件的摘要，冲突时以本文件为准。
> 目的：**构建做一次、数据只建一份、产物固定命名**——不再出现「每轮全量重建、
> 删一整棵目录树、脚本按版本号增殖」。

## 0. 四条硬规矩

1. **一轮只构建一次**。`web/dist` 被 rust-embed 内嵌进二进制，所以改了前端必须
   `npm run build` → `cargo build`，但这一串在一轮验证里只做一次，中间不重复。
2. **不删仓库里的任何既有文件**。探针数据只写 `<工作区>/.probe/<时间戳>`
   （`asmoday/` 的上一级，与仓库同级），跑完只删自己创建的那一个时间戳目录；
   缩略图缓存、`dist/assets` 之类的既有产物一律不动。
3. **产物固定命名，覆盖写**。截图 `probes/shots/login-code.png` 这种名字，同一状态
   永远同一张；不再出现 `shot-login-*-v4947.png` / `-v4948.png` 一路增殖。
4. **断言跟着 GWT 走**。新增/改动的验收条目先写进 `Design/agentreadme.md`，探针里的
   断言文案直接引用条目编号，避免「代码改了、测试还在锁旧行为」。

## 1. 门禁分层：按改动选层，不无脑全跑

| 层 | 触发条件 | 跑什么 | 大约耗时 |
| --- | --- | --- | --- |
| **L0** | 改一处 Rust 逻辑，边写边验 | `cargo test <模块名>`（单模块） | 秒级 |
| **L1** | 准备提交 | `cargo fmt --check` + `cargo clippy --all-targets -- -D warnings` + `cargo test`；**动过前端**再加 `vue-tsc -b` | 20~40s |
| **L2** | 动了认证 / 会话 / API 契约 / 配置默认值 / 下载引擎 | `python probes/run_login.py --case=http`（HTTP 冒烟），动了下载再加 `python probes/aria2_explorer.py` | 一次构建 + 5s（下载 +8s） |
| **L3** | 动了交互、样式、路由、空状态 | `python probes/run_login.py --case=ui-password --case=ui-code` | 一次构建 + 6s |

规矩：
- L2/L3 之前**一次性** `python probes/run_login.py --build`，之后再跑任何 case 都不再重建。
- 只改后端纯逻辑（比如缩略图缓存）不必重跑 L3；只改 CSS 不必重跑 L2。
- 全跑（`python probes/run_login.py`）只在动了大块认证相关时用，约 10s。

## 2. 探针套件

**这两个目录只在本机存在，不入库**（`.gitignore` 里 `/probes/`、`/scripts/`）：它们是调试脚手架，
改一行断言就要跟着改业务流程，放进仓库只会让提交历史被「探针适配」淹没，也让 clone 出来的人
误以为它们是产品的一部分。从仓库取到的树里没有这些文件，需要时按本节说明自建。

```
probes/
  run_login.py     统一入口：--case=http|ui-password|ui-code、--build、--keep-data
  lib_probe.py     公共件：Server（临时数据 + 随机空闲端口 + 独立日志）、Caller、Checks、totp
  login_http.py    L2：登录 API 29 条断言（GWT-AUTH-02/05/22/23/24/25）
  login_ui.cjs     L3：登录页两形态（password / code），playwright-core
  aria2_explorer.py L2：Aria2 Explorer 发包形态 11 条断言（GWT-DL-32/33）
  shots/           截图输出（固定文件名，本机产物）
```

常用命令：

```bash
python probes/run_login.py --build            # 一轮开头：构建 + 全跑
python probes/run_login.py --case=http        # 只验认证后端
python probes/run_login.py --case=ui-code     # 只看 2FA 切换后的动态码形态
python probes/run_login.py --case=http --keep-data   # 排障：保留数据目录并打印日志尾部
python probes/aria2_explorer.py               # 下载：插件请求头 / 探测回退 / 跨源跳转
python probes/aria2_explorer.py --build       # 同上，先 cargo build
```

下载类探针为什么要自备桩站：它验的正是「站点不配合」的三种形态（HEAD 405、
缺 Referer/Cookie 就 403、302 跳到另一个源），用真实外网站点既不稳定也没法断言。

依赖：`playwright-core` 装在 `asmoday/` 同级的 `.probe/node_modules`（仓库外面；`run_login.py`
会自动把它塞进 `NODE_PATH`）；浏览器优先用 `CHROME_PATH`，否则扫 `ms-playwright/chromium-*`。
缺了会直接报一句「npm i playwright-core --prefix ../.probe」，不会静默跳过。

用例与断言口径：

| case | 覆盖 | 对应条目 |
| --- | --- | --- |
| `http` | 一次性密码 → 改密 → 开 2FA → `{totp}` / `{password}` / 恢复码 / 严格双因子 / 空凭证 / 关 2FA 后失效 → 首尾空白宽容 → 密码字符集（空格/中文/全角/超长/纯数字被拒） | GWT-AUTH-02/05/22/23/24/25 |
| `ui-password` | 未开 2FA：只有一个密码框、无切换入口、无 2FA 字样、首尾空白自动消失、非法字符被滤掉、密码能登 | GWT-AUTH-02/24/25 |
| `ui-code` | 开 2FA：默认密码框 + 切换入口 → 同框变动态码样式（居中/明文/占位 000000）→ 切换清空 → 动态码模式填密码进不去 → 输满自动提交 → 密码模式仍能登 | GWT-AUTH-06 |

## 3. 数据与产物

- **数据根**：`<工作区>/.probe/<YYYYmmdd-HHMMSS>/<case>/`，即 `asmoday/` 的**同级**
  目录（`<工作区>\.probe\...`，例如 `C:\AimerFiles\deepseekworkspace\.probe\20261106-204500`）。套件结束 `rmtree` 这一个根，
  不碰仓库里其它任何东西；`--keep-data` 保留它并打印 `server.log` 尾部。
- **边界规则（唯一一条，别的都是它的推论）**：`asmoday/` 里只放前后端源码 + `target/`；
  运行期要写的东西一律放 `asmoday/` 同级，且**不得越出工作区**——`%TEMP%`、用户目录、
  `C://` 根、`C://AimerFiles` 根都不写。工具链自己的缓存（`~/.cargo`、`~/.cargo/bin`、
  playwright 的 `ms-playwright` 浏览器包）是环境级安装，不在此列。
- **为什么不用系统 `%TEMP%`**：Windows 上二进制在 `%TEMP%` 里建实例锁会拿到
  `os error 5`（`cannot open instance lock`），同一份 exe 在工作区盘上正常。数据根放
  工作区同级，既绕开这个坑，又不进版本库（在仓库外面，连 ignore 规则都不用写）。
- **为什么不放 `target/`**：`target/` 下只留 `debug` / `release` 两个 profile 目录，
  谁往里塞东西（探针、临时库、打包产物）就等于制造「`cargo clean` 也清不干净的杂物」。
  单测的临时库同理，落在 `target/<profile>/test-scratch/`（`core::test_scratch_dir()`）。
- **端口**：`free_port()` 让内核分配，不再写死 4299/4300（曾经撞过一次）。
- **代理**：本机探测必须 `ProxyHandler({})` / `NO_PROXY=127.0.0.1`，否则会被
  `http_proxy` 劫持成 502。
- **工作区里的删除会进回收站（2026-11-06 实测）**：在本机构建时，被执行环境接管的
  工作区把**任何**删除（含 `DeleteFileW` / `fs::remove_file` 这类底层 unlink）重定向到
  回收站——同一个 `[System.IO.File]::Delete()` 在 `%TEMP%` 里 `delta=0`，在
  `asmoday/target` 里 `delta=1`。后果：`cargo` 每次清理上一轮增量产物都会在回收站留下
  上千条目（实测一次 `cargo build` = **1767 条 / 539 MB**，几乎全是
  `target/debug/incremental/*.pre-lto.bc` 与 `target/debug/deps/*.o`）。
  这不是项目的 bug，也改不了；**别为此写脚本去「删回收站」**，能做的只是少触发重编
  （见第 4 节）。要释放空间时用 `Clear-RecycleBin` 或按 `$I` 记录里原路径精准挑，别整盘清。

## 3.5 容器与监听（手工，不进自动链）

本机没有 Docker 时，监听相关的改动至少用二进制验一遍：

```bash
ASMODAY_HOST=auto ./target/debug/asmoday -d ../.probe/bind -p 45999
# 期望：listening on [::]:45999 (dual stack: IPv4 + IPv6)，/api/health 200
```

再验「不降级」：先占住同一端口，`-H ::` 必须直接报
`failed to bind [::]:<port>: address already in use (os error 10048)`，不能悄悄换到 `0.0.0.0`。

有 Docker 的环境补一遍镜像层（首次构建十几分钟）：

```bash
docker build -t asmoday .
docker run --rm -d --name asmoday-check -p 4242:4242 asmoday
curl -s http://127.0.0.1:4242/api/health
docker logs asmoday-check | grep -E "listening on|cannot listen"
```

判据：Docker 默认 bridge（无 IPv6）里出现
`cannot listen on [::]:4242 (AddrNotAvailable); falling back to 0.0.0.0`，随后 healthcheck 变 healthy。
镜像不在 CI 的 L0~L3 里跑，靠 `.github/workflows/docker.yml` 的构建 job 兜底（PR 只构建不推送）。

## 4. 明确不做的事

| 反模式 | 为什么不行 |
| --- | --- |
| `touch src/main.rs` 只为看 warning | 白等一轮编译；`cargo build` 本来就会打。而且一次重编 = 1000+ 个增量产物进回收站（第 3 节），代价比它省下的几秒大得多 |
| 每个 bug 新建一套 `probe-vXXXX.py` + 一套 `data-vXXXX` | 90% 重复，还会在源码目录堆一堆一次性目录 |
| `rmtree` 源码目录下的 `data-*` / `dist` | 删一大片再重建，正是「频繁删除」的来源 |
| `git add -A` | 容易把临时产物带进提交，显式列路径 |
| 手删 `dist/assets` 里的旧 hash 文件 | `vite build` 的 `emptyOutDir` 已经做，这一步是多余动作 |
| 一次改动里反复 `npm run build` + `cargo build` | 一次构建支撑整轮 L2/L3 |

## 5. 排障顺序

1. `--keep-data` 重跑，看 `server.log` 尾部（套件失败时会自动打 15 行）。
2. HTTP 层面先用 `--case=http`：它不走浏览器，能把「后端错」和「前端错」分开。
3. 浏览器层面看 `probes/shots/*.png`（固定名，永远是最新一次）。
4. 涉及 TOTP 的用例先对时：`totp()` 用系统时间，机器时钟偏移 >30s 会全红。
5. 端口占用 / 上一实例没退：`tasklist | grep asmoday` 后 `taskkill /IM asmoday.exe /F`。

## 6. 变更记录

- **v20（v4.9.70）**：`probes/services_form.cjs` D 段按可视化图标选择器重写，断言数 27→31。
  覆盖：D0 点触发按钮宫格展开且格子数 ≥30（每格是渲染出的图标而不是文字选项）；
  D1 点格子选中后宫格收起；D2/D3 图标写进 POST 与 GET；**D4 跳到门户 `/` 页断言
  `[data-service]` 卡片的 svg path 真是 folder**（v4.9.70 前模板写死 `grid`，icon 配了
  也从不显示）；D5 默认档把 icon 从 POST 里整个省略（不是写空串）。A5 从
  `inputValue()` 改为断言触发按钮的 `data-value` 属性（按钮不是 input 了）。
  **坑**：D 段跑在 C 段之后，服务已在 C5 改名为 `probe-renamed`，门户断言的 id 必须从
  当前落盘配置 `cfg.services[0].id` 读，不能写死探针初始 id。
  门禁 `vue-tsc -b` + `vite build --emptyOutDir=false` + `services_form.py` 31/31 +
  既有 `config_save.py` 15/15；Rust 零改动不重跑 `cargo build`。
- **v19（v4.9.69）**：新增 `probes/services_form.py` + `services_form.cjs`（27 条断言）。
  初始 `config.toml` 故意带 `strip_prefix = true` 与两个注入头，同时钉住「弃用 UI 字段后
  存量配置仍可解析、原样回传」。覆盖：A 表单无死开关、常驻 `/proxy/<id>/` 预览、两个头
  各占一行、图标下拉回显；B 改 / 增 / 删注入头都完整落盘、POST 不带 `headerRows` 等前端
  字段、`strip_prefix` 原样回传；C 新建 id 为 `service-1`、复制完整地址、改名出现断链
  警告且保存后消失、重复 / 非法字符 / 空 id 均不发 POST、恢复后可保存；D 图标改动回写；
  Z 无未捕获异常。
  **坑**：后端 `inject_headers` 是 `BTreeMap`，每次保存后 GET 会按头名字母序重排，断言必须
  按头名定位而不是 `nth(1)` / `last()`；本机 playwright-core 较旧，没有
  `locator.allInputValues()`，改用 `evaluateAll(els => els.map(el => el.value))`。
  门禁 `vue-tsc -b` + `vite build --emptyOutDir=false` + 新探针 27/27 +
  既有 `config_save.py` 15/15；Rust 零改动不重跑 `cargo build`。
- **v18（v4.9.68）**：新增 `probes/gallery_toolbar.py` + `gallery_toolbar.cjs`（34 条断言）。
  样本为 2 张 1×1 PNG + 2 个无效 mp4 + 1 个无效 mp3，起服务后先由 `Caller` 预热索引并
  把 `a.png` 置为收藏，浏览器里筛选结果才是确定的。覆盖：A 三分区都不再有 `gallery-more`
  下拉、都有显式 `gallery-favorites` chip；B chip 的 `aria-pressed` / 强调色激活态 / 时间线
  4→1→4 / 筛选态隐藏分页进度 / 目录墙禁用且给出原因提示；C 影视与音乐库筛选同样生效，
  空态文案指向筛选而不是「媒体库是空的」；D 360px 窄屏下音乐行三点常驻可见且能开单条菜单、
  Esc 只收菜单、竖三点描边为 3、影视卡片长按（合成 `TouchEvent`）能开同一菜单且补发 click
  不误开灯箱；E 视频灯箱信息层的兼容模式开关勾选即时切原生 `<video controls>`、持久化到
  `asmoday.cinema.native`、取消即回、图片灯箱不显示该项；Z 无未捕获异常。
  **坑**：B8 把照片库视图切到「文件夹」并写进 `asmoday.photos.view.v2`，后续再进 `/photos`
  等 `[data-photo]` 会超时——须先写回 `timeline`；合成 `TouchEvent` 需要 `Touch` 构造并
  带上 `touches[0]`，`useLongPress` 才会启动计时；`vite build` 仍用 `--emptyOutDir=false`
  绕开执行环境的批量删除闸门。门禁 `vue-tsc -b` + `vite build --emptyOutDir=false` +
  新探针 34/34 + 既有 `media_viewer.py` 21/21；Rust 零改动不重跑 `cargo build`。
- **v17（v4.9.67）**：新增 `probes/media_viewer.py` + `media_viewer.cjs`（21 条断言）。
  一次性实例 + 挂载点里 2 张 1×1 PNG 与 2 个无效 mp4（只断言 DOM / 导航 / 键位，
  不依赖解码），真实浏览器走：A 影视库点卡片开的是全局灯箱且序列为 1/2、视频也有
  缩略图条与左右箭头、顶栏工具齐全；B `←/→` 在视频上翻页、`Shift+→` 不翻页、
  信息层给出键位提示；C Esc 分层收信息层 → 再按关灯箱、顶栏隐去 3.2s 后左上 ✕
  仍可见可点且能关；D 图片库序列 1/4 且缩略图条有 2 个 `.mp4`、点它直接跳到视频
  并渲染播放器、相册详情确实发 `kind=media&depth=0`；  E 分享动作图标 5 段三点两线、
  侧边栏分享管理改用 2 段 `link`；F 置 `asmoday.cinema.native=1` 后灯箱仍走原生
  `<video controls autoplay>`（收敛查看器后降级开关不丢）；Z 无未捕获异常。
  **坑**：`playwright-core` 不在仓库里，必须给 `run_node()` 传
  `NODE_PATH=<工作区>/.probe/node_modules`（与 `config_save.py` 同口径）；
  视频文件故意无效，`MediaPlayer` 的 error 事件不是 `pageerror`，不影响断言。
  门禁 `vue-tsc -b` + `vite build --emptyOutDir=false` + 探针全绿；Rust 零改动，
  不重复跑 `cargo build`（避免增量产物被重定向到回收站）。
- **v16（v4.9.66）**：新增 `probes/remove_with_files.py`（14 条断言，本地 HTTP 站点 +
  真实 exe + `/jsonrpc` 造任务 + 页面同款 REST 删除）：A 普通删除 / `delete_files=0`
  保留文件且 `deletedFiles=0`；B `delete_files=1` / `=true` 删掉目标文件并清掉任务记录；
  C pause 任务手工补半成品与 `.aria2` 后删除，两个文件都没了且 worker 收尾不重建
  控制文件；D 下载后把保存目录热替换到别处再删，白名单外文件不删且进 `failedFiles`。
  Rust 单测补 `delete_output_files_stays_inside_roots`（越界 / 目录 / 软链接不删）。
  门禁 `fmt` + `clippy -D warnings` + `cargo test`（346）+ `cargo build` +
  `vue-tsc -b` + `vite build` 全绿。**坑**：本地 `vite build` 默认清空 `dist/assets`
  会被执行环境的批量删除闸门拦下（109 个文件 > 50），验证时用
  `vite build --emptyOutDir=false`；产物哈希已更新，仅残留旧 asset，不影响 debug 运行。
- **v15（v4.9.65）**：新增 `probes/filename_policy.py`（23 条断言，本地 HTTP 站点 +
  真实 exe + `/jsonrpc`）：A 无 CD 用 URL 末段且 `getOption` 不回显自动 `out`；B CD 的
  UTF-8 中文名 / `filename*` 优先 / 显式 `out` 覆盖；C 危险 CD 名整段忽略、URL 里的
  `%2F`、`..%2F` 不越出保存目录；D 同名二次下载改 `name.1.ext` 且不覆盖旧文件；
  E `allow-overwrite=true` 恢复覆盖；F `auto-file-renaming=false` 报 aria2 的 13；
  G 两个 pause 任务同名排队也要分到两个文件；H `filename=` 的 ISO-8859-1 / UTF-8
  两种解释。门禁 `fmt` + `clippy -D warnings` + `cargo test`（345）+ `cargo build`
  全绿，既有 `aria2_explorer` 15/15、`ariang_list` 18/18、`init_flow` /
  `open_removed` 无回归。**坑**：活跃任务同名预检查若把 complete 的历史任务也算进去，
  重复下载会凭空多一个 `.N` —— 只看 active / waiting / paused。
- **v14（v4.9.64）**：新增 `probes/open_removed.py`（纯 CLI 探针，不起浏览器）：
  A `-h` 不含 `--open` / `ASMODAY_OPEN` 且字符全 ASCII；B 传 `--open` 启动 →
  clap `unexpected argument` 非零退出、不建库不监听；C 只设 `ASMODAY_OPEN=1`
  正常启动 → 服务可访问、日志无 `opened the default browser` /
  `could not open a browser`、横幅 `Open:` 地址与端口仍正确（OSC 8 链路未受影响）。
  门禁：`cargo fmt --check` → `clippy -D warnings` → `cargo test` → `cargo build`
  → `init_flow.py` 回归。
  **防回归**：别再把「由服务端进程拉起浏览器」以任何形式加回来（flag / 环境变量 /
  首启 + conhost 隐式触发）——v4.9.45 / v4.9.49 的提权混权弹窗是实机教训；
  经典 conhost 的出路只有「复制地址」这一条纯提示。

- **v13（v4.9.63）**：新增 `probes/init_flow.py`（纯 CLI 端到端探针，不起浏览器）：
  A 全新目录跑 `--init` → stdout 只有 `default config created` + `config ready; ...`，
  **没有** `One-time password` / `Open:` / `starting` / `database ready`；
  且数据目录、`asmoday.db`、`data/logs` 都不存在。B 紧接着不带 `--init` 正常启动
  → 首启引导才打印 OTP 横幅与 `Open:`，地址端口可连。C 再跑一次 `--init`
  → 只提示 `config already exists`，不改配置、不碰库。
  `--init` 与 `--reset-password` 都不起服务，横幅一律不带地址。
  **防回归**：别再把首启引导塞回 `--init` —— 那边发的密码在下次正常启动时会被
  `reissue_pending_password()` 直接顶掉作废，且 `Open:` 地址根本没服务监听。

- **v12（v4.9.62）**：CLI 帮助文案纯文本改动，无新增浏览器探针。验收口径：
  `cargo fmt --check` → `cargo build` → `./target/debug/asmoday.exe -h` 全量读一遍
  （检查语言一致、选项对齐、描述不超长折行）→ `-V` 输出 `asmoday 0.1.0`
  → 管道 `python -c` 断言 help 里没有 `ord(c) > 127` 的字符。
  **防回归**：clap derive 直接拿 `Cli` 字段的 `///` 当 help 文本，别再把中文写回
  `src/main.rs:114-152`；控制台输出全英文是 v4.9.42 定下的编码约定，不是风格偏好。
  Windows 下 `Usage: asmoday.exe [OPTIONS]` 属 clap 读 `argv[0]` 的正常行为，非缺陷。

- **v11（v4.9.61）**：新增 `probes/config_save.py` + `config_save.cjs`（14 条浏览器断言），
  补上此前所有探针都没覆盖的「真的点保存」路径：进设置页 → 改 `segments` → 点
  `download-settings-save` → 抓 POST 请求体与响应 → 断言 200、只改一个字段、
  `system.mounts` 原样回传、GET 读到新值、未保存角标消失、无 `structuredClone` 错误条，
  再无改动保存一次仍 200。用 `[[system.mounts]]` 临时配置，POST 回写不碰仓库配置。
  **探针缺口教训**：`unsaved_guard` 的 C 段只「改草稿 → 放弃」，所以 v4.9.40 漏修的
  `saveSettings()` 一直没被发现；验证「保存」类 bug 必须真的发一次请求。
  **实测纠正**：v10 写的「不重建 exe 浏览器跑旧页面」只在 release 成立；debug 下
  rust-embed 运行时读盘，本次 `cargo build` 因用户手动启动的裸实例（PID 23800，
  CommandLine 无参数）占用 exe 报 os error 5，三套探针仍全部跑在新 dist 上；实例退出后
  补跑 `cargo build` 成功（23.22s），用新 exe 复验 `config_save` 仍 14/14。
  遇 `failed to remove file ... asmoday.exe` 先用 `Get-CimInstance Win32_Process`
  看命令行再决定是否 taskkill：带 `-d/-p/-H` 的是探针遗留可清，无参数的可能是用户在用的服务，
  不要盲杀；debug 验证也不必为它强行重建。

- **v10（v4.9.60）**：新增 `probes/dir_picker.py` + `dir_picker.cjs`（13 条浏览器断言）：
  设置页无手输路径输入框、面包屑逐级进入预置子目录、新建文件夹后自动进入并回填草稿、
  新建下载弹窗内选择器受保存目录白名单约束、选中路径回填且所属下拉不空白、无 JS 报错。
  挂载点用 `[[system.mounts]]` 指向 `.probe/<时间戳>/mnt`，新建文件夹不碰真实数据。
  `unsaved_guard.cjs` 的 C 段不再依赖已删除的 `download-new-root-rel`，改填引擎参数。
  改前端后门禁顺序固定：`vue-tsc -b` → `vite build` → `cargo build`（dist 内嵌，不重建 exe
  浏览器跑的还是旧页面）。**[v11 更正]** 括号里的判断只对 release / 发版成立：rust-embed 8
  未启用 `debug-embed`，debug 构建运行时从磁盘读 `dist/`，实测不重建 exe 浏览器也是新页面。

- **v9（v4.9.59）**：发版 CI 的 docker merge 失败（`-amd64 is a manifest list`）
  暴露了一条只靠本地门禁测不出的链路：本机无 docker，多架构 manifest 合并只能在
  tag 触发的真实 runner 上验证。查 CI 用匿名 REST
  `GET /repos/Asmoday-lab/asmoday/actions/runs/{id}/jobs` 看失败 step，
  修 workflow 后必须**重打 tag**（re-run 会用旧 commit 的 workflow 定义，改了不生效）。

- **v8（v4.9.58）**：新增 `probes/ariang_list.py`（18 条断言），按 AriaNg 打包源码
  的真实形态发 `system.multicall` —— **外层不带 token、每个子调用带 token、
  `tellStopped` 的 offset 是 -1**。这是「左侧有数量、右侧列表空」的唯一复现方式，
  手写 `tellStopped(0,1000)` 永远测不出来。断言同时锁住伪造 token / 缺 token
  整批拒绝与旧式外层 token 兼容。改 `/jsonrpc` 分派后照旧 `cargo build` 再跑探针。

- **v7（v4.9.57）**：拒绝响应分类改为可断言的行为——单测新增
  `reject_details_classify_cloudflare_and_plain_text` 与
  `cloudflare_challenge_page_reports_actionable_hint`，后者复刻真实 CF 403 页并
  断言 errorMessage 里**没有** `<!DOCTYPE` / `<html`。排查真实站点 403 时，
  先看 errorMessage 的分类结论，再看控制台 WARN 的 `proxy:` 字段核对出口 IP。

- **v6（v4.9.56）**：`probes/aria2_explorer.py` 扩到 **15 条断言**，新增一次性签名链接
  （只允许第一个 GET、之后 403）与 403 页面原文两组桩站；关键断言是
  「整条任务只发 1 个 HTTP 请求」——旧版四发探测请求在这里必然失败。断言名
  「HEAD 405 → GET+Range 探测」改为「首请求即下载」。改后端后照旧 `cargo build`
  再跑探针；前端未动时不必重建 dist。

- **v5（v4.9.55）**：新增 `probes/unsaved_guard.py` + `unsaved_guard.cjs`（浏览器回归，
  10 条断言），专测「下载页没改动却弹离开确认」：断言前提是探针实例的
  `/api/downloads/engine` 至少有一条保存目录（空数组复现不出这个 bug）。
  改前端后照旧先 `npm run build` 再 `cargo build`——dist 内嵌，只改 Vue 不重编 exe
  探针跑的还是旧页面。

- **v4（v4.9.53）**：新增 §3.5「容器与监听」——Docker 不在 L0~L3 自动链里，本机无 docker 时
  用二进制验 `-H auto` 双栈与「端口被占不降级」；有 docker 时补一遍镜像与 healthcheck。
  判据写死为日志里的 `cannot listen on [::]:… falling back to 0.0.0.0`。
- **v2（v4.9.49 后）**：记下「工作区里的删除会被重定向到回收站」这一环境事实与实测代价
  （一次重编 1767 条 / 539 MB），`touch` 反模式补上这条后果；口径改为「少触发重编」而不是
  去清回收站。
- **v3（v4.9.52 前后）**：`target/` 只留 `debug` / `release`——探针数据根从
  `target/probe/<时间戳>` 迁到 `.probe/<时间戳>`，单测临时库从 `target/test-scratch/`
  迁到 `target/<profile>/test-scratch/`（`core::test_scratch_dir()`）；
  **`.probe/` 再挪到 `asmoday/` 的同级**（仓库里连运行期目录都不留），并写死边界规则：
  运行期产物只在工作区内、`asmoday/` 之外。playwright 的 node_modules 同样移到
  `../.probe/node_modules`。
- **v1（v4.9.48）**：从 `.asmoday-probe` 里四份「按版本号」的脚本收敛为
  `probes/run_login.py` 一套；数据根从「源码目录 + 每次 rmtree」改为
  `target/probe/<时间戳>`（v3 又迁出 `target/`）；截图改固定名；明确 L0~L3 分层与六条反模式。
