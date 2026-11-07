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
