"""探针公共件：起停服务、HTTP 调用、TOTP、一次性密码读取。

约定（见 Design/testing.md）：
- **数据目录一律建在 `target/probe/<时间戳>`**（已 ignore、与仓库同卷），不进版本库、
  也不在源码目录里制造「删一整个目录树再重建」；一次套件只清理自己创建的那一个根
  （系统 `%TEMP%` 不能用：Windows 上实例锁在那里会拿 os error 5）。
- 一个用例一份数据目录、一个空闲端口，用例之间互不污染。
- 服务本体就是 `target/debug/asmoday.exe`，与发布形态同一份二进制（dist 已内嵌）。
"""

from __future__ import annotations

import base64
import hashlib
import hmac
import http.cookiejar
import json
import os
import re
import socket
import struct
import subprocess
import sys
import time
import urllib.error
import urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
EXE = os.path.join(ROOT, "target", "debug", "asmoday.exe" if os.name == "nt" else "asmoday")
PASSWORD = "SecretPass2026"


def free_port() -> int:
    """让内核挑一个空闲端口，避免写死 4299/4300 撞车。"""
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def totp(secret: str, at: float | None = None) -> str:
    pad = "=" * ((8 - len(secret) % 8) % 8)
    key = base64.b32decode(secret.upper() + pad)
    counter = int((at or time.time()) // 30)
    digest = hmac.new(key, struct.pack(">Q", counter), hashlib.sha1).digest()
    offset = digest[-1] & 0x0F
    return "%06d" % ((struct.unpack(">I", digest[offset:offset + 4])[0] & 0x7FFFFFFF) % 1000000)


class Checks:
    """断言收集器：只打印，不抛，最后统一给退出码。"""

    def __init__(self, title: str) -> None:
        self.title = title
        self.total = 0
        self.failed = 0

    def __call__(self, name: str, ok: bool, detail: object = None) -> bool:
        self.total += 1
        if not ok:
            self.failed += 1
        suffix = "" if ok else " -> " + json.dumps(detail, ensure_ascii=False, default=str)
        print(f"{'PASS' if ok else 'FAIL'} {name}{suffix}")
        return ok

    def summary(self) -> int:
        print(f"\n[{self.title}] {self.total - self.failed}/{self.total} passed")
        return 0 if self.failed == 0 else 1


class Caller:
    """带 Cookie 罐、绕开系统代理的 HTTP 调用方（本机探针必须 --noproxy）。"""

    def __init__(self, base: str) -> None:
        self.base = base
        self.opener = urllib.request.build_opener(
            urllib.request.ProxyHandler({}),
            urllib.request.HTTPCookieProcessor(http.cookiejar.CookieJar()),
        )

    def call(self, method: str, path: str, body: dict | None = None) -> tuple[int, dict]:
        data = None if body is None else json.dumps(body).encode()
        req = urllib.request.Request(self.base + path, data=data, method=method)
        if data is not None:
            req.add_header("Content-Type", "application/json")
        req.add_header("Accept", "application/json")
        try:
            with self.opener.open(req, timeout=10) as res:
                return res.status, json.loads(res.read() or b"{}")
        except urllib.error.HTTPError as err:
            return err.code, json.loads(err.read() or b"{}")


class Server:
    """一次性服务实例：临时数据目录 + 空闲端口 + 独立日志。

    `config` 给定时用 `-c` 指定配置文件（不指定就落到进程工作目录的
    `config.toml`）——下载类探针要在配置里写 `[[system.mounts]]`，
    不能让探针去改仓库根的那份。
    """

    def __init__(self, tag: str, data_root: str, *, config: str | None = None) -> None:
        self.tag = tag
        self.data = os.path.join(data_root, tag)
        os.makedirs(self.data, exist_ok=True)
        self.log = os.path.join(self.data, "server.log")
        self.port = free_port()
        self.base = f"http://127.0.0.1:{self.port}"
        self.config = config
        self.proc: subprocess.Popen | None = None

    def __enter__(self) -> "Server":
        if not os.path.exists(EXE):
            raise SystemExit(f"missing binary: {EXE} (run `cargo build` first)")
        env = dict(os.environ)
        for key in ("HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"):
            env.pop(key, None)
        env["NO_PROXY"] = "127.0.0.1,localhost"
        self._out = open(self.log, "w", encoding="utf-8")
        command = [EXE, "-d", self.data, "-p", str(self.port), "-H", "127.0.0.1"]
        if self.config:
            command += ["-c", self.config]
        self.proc = subprocess.Popen(
            command,
            stdout=self._out,
            stderr=subprocess.STDOUT,
            cwd=ROOT,
            env=env,
        )
        self._wait_ready()
        return self

    def __exit__(self, *exc_info) -> None:
        if self.proc is not None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.proc.kill()
        self._out.close()

    def _wait_ready(self, timeout: float = 30) -> None:
        caller = Caller(self.base)
        deadline = time.time() + timeout
        while time.time() < deadline:
            try:
                if caller.call("GET", "/api/auth/status")[0] == 200:
                    return
            except Exception:
                pass
            time.sleep(0.3)
        raise SystemExit(f"server on {self.base} did not become ready; log: {self.log}")

    def read_otp(self) -> str:
        """从启动横幅里抠一次性密码（只有首启 / 补发时才打印）。"""
        with open(self.log, "r", encoding="utf-8", errors="replace") as fh:
            lines = fh.read().splitlines()
        for index, line in enumerate(lines):
            if "One-time password" in line:
                for nxt in lines[index + 1:]:
                    found = re.match(r"^\s{6}(\S+)\s*$", nxt)
                    if found:
                        return found.group(1)
        raise SystemExit(f"one-time password not found in {self.log}")


def temp_root() -> str:
    """套件级数据根：`target/probe/<时间戳>`。

    为什么不用系统临时目录：Windows 上在 `%TEMP%` 里建实例锁会拿到 os error 5
    （`cannot open instance lock`，同一份二进制在仓库盘上正常），所以统一放在
    `target/` 下——已被 .gitignore 忽略、与仓库同卷、`cargo clean` 顺手带走。
    套件只删自己创建的那一个时间戳目录，绝不碰 `target/` 里的其它东西。
    """
    root = os.path.join(ROOT, "target", "probe", time.strftime("%Y%m%d-%H%M%S"))
    os.makedirs(root, exist_ok=True)
    return root


def prepare(base: str, caller: Caller, otp: str, *, two_factor: bool) -> str:
    """把一个全新实例推到「已设正式密码（可选已开 2FA）」的状态，返回 TOTP 密钥。"""
    caller.call("POST", "/api/auth/login", {"password": otp})
    caller.call("POST", "/api/auth/password", {"next": PASSWORD})
    secret = ""
    if two_factor:
        secret = caller.call("GET", "/api/auth/totp/setup")[1]["secret"]
        caller.call("POST", "/api/auth/totp/enable", {"code": totp(secret)})
    caller.call("POST", "/api/auth/logout")
    return secret


def run_node(script: str, args: list[str], env_extra: dict[str, str]) -> int:
    env = dict(os.environ)
    for key in ("HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy", "LOGIN_PASSWORD", "TOTP_SECRET"):
        env.pop(key, None)
    env["NO_PROXY"] = "127.0.0.1,localhost"
    env.update(env_extra)
    return subprocess.run(["node", script, *args], cwd=ROOT, env=env).returncode


if __name__ == "__main__":
    sys.exit("this module is imported by probes/run_login.py")
