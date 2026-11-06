"""登录套件统一入口（替代 .asmoday-probe 里那一堆 smoke-login-v49xx / probe-login-ui-v49xx）。

用法：
  python probes/run_login.py                 # 跑全部用例
  python probes/run_login.py --case=http     # 只跑某个用例
  python probes/run_login.py --build         # 先构建一次（改前端后必须，dist 内嵌进二进制）

三个用例：
  http         —— 登录 API 全量冒烟（18 条，改认证逻辑时跑）
  ui-password  —— 未开 2FA 的登录页（只有密码框、无切换入口）
  ui-code      —— 开了 2FA：默认密码框 + 切换后同一框变动态码样式

约定（见 Design/testing.md）：
- **构建只做一次**：--build 在整轮开头跑 npm build + cargo build，用例里绝不再重建。
- **不删仓库里的东西**：每个用例的数据目录在系统临时目录，套件结束只删这一个临时根。
- **截图固定名**：写 probes/shots/*.png 直接覆盖，不按版本号增殖。
"""

from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from lib_probe import (  # noqa: E402
    EXE,
    ROOT,
    Caller,
    Server,
    prepare,
    run_node,
    temp_root,
)

PROBES = os.path.dirname(os.path.abspath(__file__))
SHOTS = os.path.join(PROBES, "shots")
CASES = ("http", "ui-password", "ui-code")


def ensure_playwright() -> str | None:
    """找一个装着 playwright-core 的 node_modules 目录，交给 NODE_PATH。"""
    for candidate in (
        os.path.join(PROBES, "node_modules"),
        os.path.join(ROOT, ".asmoday-probe", "node_modules"),
        os.path.join(ROOT, "web", "node_modules"),
    ):
        if os.path.isdir(os.path.join(candidate, "playwright-core")):
            return candidate
    return None


def build_once() -> None:
    """整轮只构建一次：dist 内嵌进二进制，所以前端先、Rust 后。"""
    npm = shutil.which("npm")
    if not npm:
        raise SystemExit("npm not found")
    print("[build] npm run build (web/)")
    subprocess.run([npm, "run", "build"], cwd=os.path.join(ROOT, "web"), check=True)
    print("[build] cargo build")
    subprocess.run(["cargo", "build"], cwd=ROOT, check=True)


def tail(log_path: str, lines: int = 15) -> str:
    try:
        with open(log_path, "r", encoding="utf-8", errors="replace") as fh:
            return "\n".join(fh.read().splitlines()[-lines:])
    except OSError:
        return "(no log)"


def case_http(root: str) -> int:
    import login_http

    with Server("http", root) as server:
        code = login_http.run(server)
        if code:
            print(f"\n--- server log tail ({server.log}) ---\n{tail(server.log)}")
        return code


def case_ui(mode: str, root: str, node_modules: str | None) -> int:
    if node_modules is None:
        raise SystemExit("playwright-core missing: npm i playwright-core --prefix .asmoday-probe")
    with Server(mode, root) as server:
        otp = server.read_otp()
        extra = {"BASE": server.base, "SHOTS_DIR": SHOTS, "NODE_PATH": node_modules}
        if mode == "ui-password":
            # 这个用例跑在「还没设正式密码」的实例上，只能用一次性密码登
            extra["LOGIN_PASSWORD"] = otp
        else:
            extra["TOTP_SECRET"] = prepare(server.base, Caller(server.base), otp, two_factor=True)
        code = run_node(os.path.join(PROBES, "login_ui.cjs"), [mode.split("-")[1]], extra)
        if code:
            print(f"\n--- server log tail ({server.log}) ---\n{tail(server.log)}")
        return code


def main() -> int:
    parser = argparse.ArgumentParser(description="asmoday login probe suite")
    parser.add_argument("--case", choices=CASES, action="append", help="只跑指定用例（可重复）")
    parser.add_argument("--build", action="store_true", help="整轮开头构建一次")
    parser.add_argument("--keep-data", action="store_true", help="保留临时数据目录（排障用）")
    args = parser.parse_args()

    if args.build:
        build_once()
    if not os.path.exists(EXE):
        raise SystemExit(f"missing {EXE}; run `cargo build` or pass --build")

    selected = args.case or list(CASES)
    node_modules = ensure_playwright() if any(name.startswith("ui-") for name in selected) else None

    root = temp_root()
    print(f"[data] {root}")
    started = time.time()
    results: dict[str, int] = {}
    try:
        for name in selected:
            print(f"\n=== {name} ===")
            results[name] = case_http(root) if name == "http" else case_ui(name, root, node_modules)
    finally:
        if args.keep_data:
            print(f"[data] kept at {root}")
        else:
            shutil.rmtree(root, ignore_errors=True)

    failed = [name for name, code in results.items() if code]
    print(f"\n=== {len(results) - len(failed)}/{len(results)} cases passed in {time.time() - started:.1f}s ===")
    for name, code in results.items():
        print(f"  {'FAIL' if code else 'PASS'} {name}")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
