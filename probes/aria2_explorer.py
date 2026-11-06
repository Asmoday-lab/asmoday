#!/usr/bin/env python3
"""Aria2 Explorer 兼容探针（L2）。

为什么单开一个：浏览器插件（alexhua/Aria2-Explorer，v2.7.x）发出的东西和「手写
aria2 RPC」不是一回事，它把：

    addUri(uris, {
        dir: "<插件里配的下载位置>",
        out: "<文件名>",
        referer: "<页面地址>",                 # 独立选项，不在 header 里
        header: ["Cookie: a=b; c=d", "User-Agent: Mozilla/5.0 …"],   # 数组
    })
    → 紧接着 changeGlobalOption(<AriaNg 里的全局选项>)

而我们最容易翻车的地方恰好都在这里：`referer` 被静默丢掉、`header` 只在第一跳
生效、HEAD 探测被站点 405 就直接判死、跨源 302 之后 Cookie 被剥掉。

桩站复刻这两类站点的真实行为（都是常见拦路虎）：
  * `HEAD /file.bin` 一律 405（对象存储 / 防盗链的常态）；
  * `GET /redirect` 302 跳到**另一个端口**（= 跨源），两跳都必须带
    `Referer: https://site.example/page` 与 `Cookie: sid=abc`，否则 403。

覆盖：
  1. addUri 返回 16 位 gid
  2. 任务跑到 complete（HEAD 405 → GET+Range 探测生效）
  3. 落盘字节与源一致（8 段并发，每段都带 Cookie/Referer）
  4. getOption 回显 referer 与 header 数组（插件详情页依赖）
  5. `pause: "true"` → 任务停在 paused；changeOption 补头后 unpause → 照样下完
  6. dir 越界时报错里列出允许的目录（插件「下载位置」配错时的唯一线索）

用法：
    cargo build && python probes/aria2_explorer.py
    python probes/aria2_explorer.py --build     # 需要时先重建二进制
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import lib_probe as probe  # noqa: E402

REFERER = "https://site.example/page"
COOKIE = "sid=abc; theme=dark"
UA = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) Chrome/140.0.0.0 Safari/537.36"
BODY = bytes((i * 37 + 11) % 251 for i in range(96 * 1024))


class ContentSite(BaseHTTPRequestHandler):
    """要 Referer + Cookie 才给下的站点，且不认 HEAD。"""

    protocol_version = "HTTP/1.1"

    def log_message(self, *args):  # 静音
        pass

    def _authorized(self) -> bool:
        return self.headers.get("Referer") == REFERER and self.headers.get("Cookie") == COOKIE

    def _deny(self) -> None:
        self.send_response(403)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def do_HEAD(self):  # noqa: N802
        if not self._authorized():
            return self._deny()
        self.send_response(405)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def do_GET(self):  # noqa: N802
        if not self._authorized():
            return self._deny()
        raw = self.headers.get("Range")
        if raw and raw.startswith("bytes="):
            first, _, last = raw[len("bytes="):].partition("-")
            start = int(first or 0)
            end = int(last) if last else len(BODY) - 1
            end = min(end, len(BODY) - 1)
            chunk = BODY[start:end + 1]
            self.send_response(206)
            self.send_header("Content-Range", f"bytes {start}-{end}/{len(BODY)}")
        else:
            chunk = BODY
            self.send_response(200)
        self.send_header("Content-Type", "application/octet-stream")
        self.send_header("Accept-Ranges", "bytes")
        self.send_header("ETag", '"probe-v1"')
        self.send_header("Content-Length", str(len(chunk)))
        self.end_headers()
        self.wfile.write(chunk)


class RedirectSite(BaseHTTPRequestHandler):
    """短链 / 图片站的形态：302 跳到另一个主机（这里用另一个端口）。"""

    protocol_version = "HTTP/1.1"
    target = ""

    def log_message(self, *args):
        pass

    def _redirect(self):
        self.send_response(302)
        self.send_header("Location", self.target)
        self.send_header("Content-Length", "0")
        self.end_headers()

    do_GET = _redirect  # noqa: N815
    do_HEAD = _redirect  # noqa: N815


class QuietServer(ThreadingHTTPServer):
    """探测时客户端会主动掐断 GET（只读响应头），socketserver 默认会打一堆
    ConnectionReset 堆栈——那不是失败，静音掉。"""

    def handle_error(self, request, client_address):
        pass


def same_path(left, right) -> bool:
    """比较两个路径是否指向同一处：分隔符方向、大小写、冗余 .. 都不算差异。"""
    def norm(value):
        return os.path.normcase(os.path.normpath(str(value))).replace("\\", "/")

    return norm(left) == norm(right)


def serve(handler, port: int) -> ThreadingHTTPServer:
    server = QuietServer(("127.0.0.1", port), handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


class Rpc:
    def __init__(self, base: str, secret: str) -> None:
        self.caller = probe.Caller(base)
        self.secret = secret
        self.counter = 0

    def call(self, method: str, *params):
        self.counter += 1
        body = {
            "jsonrpc": "2.0",
            "id": str(self.counter),
            "method": method,
            "params": [f"token:{self.secret}", *params],
        }
        status, payload = self.caller.call("POST", "/jsonrpc", body)
        if status != 200 or "error" in payload:
            raise RuntimeError(f"{method} failed: {status} {payload}")
        return payload.get("result")

    def wait(self, gid: str, want: str, timeout: float = 25) -> dict:
        deadline = time.time() + timeout
        last = {}
        while time.time() < deadline:
            last = self.call("aria2.tellStatus", gid) or {}
            if last.get("status") == want:
                return last
            if last.get("status") in ("error", "removed"):
                return last
            time.sleep(0.2)
        return last


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--build", action="store_true", help="先 cargo build")
    args = parser.parse_args()
    if args.build:
        import subprocess

        subprocess.run(["cargo", "build"], cwd=probe.ROOT, check=True)

    root = probe.temp_root()
    mount = os.path.join(root, "mnt")
    os.makedirs(mount, exist_ok=True)
    config = os.path.join(root, "config.toml")
    with open(config, "w", encoding="utf-8") as fh:
        fh.write("[system]\n[[system.mounts]]\nname = \"probe\"\n")
        fh.write(f"path = {json.dumps(mount.replace(os.sep, '/'))}\n")

    content_port = probe.free_port()
    jump_port = probe.free_port()
    handles = [
        serve(ContentSite, content_port),
        serve(type("Redirect", (RedirectSite,), {"target": f"http://127.0.0.1:{content_port}/file.bin"}), jump_port),
    ]

    checks = probe.Checks("aria2-explorer")
    try:
        with probe.Server("explorer", root, config=config) as server:
            caller = probe.Caller(server.base)
            probe.prepare(server.base, caller, server.read_otp(), two_factor=False)
            caller.call("POST", "/api/auth/login", {"password": probe.PASSWORD})
            status, info = caller.call("GET", "/api/downloads/engine")
            if status != 200 or not info.get("enabled"):
                raise SystemExit(f"downloads disabled: {status} {info}")
            roots = info.get("roots") or []
            checks("实例有可用的下载根", bool(roots), info)
            allowed_dir = roots[0]["path"]
            rpc = Rpc(server.base, info["rpc_secret"])

            # -- 1. 插件默认形态：header 数组 + referer + 302 跨源 --------------
            gid = rpc.call(
                "aria2.addUri",
                [f"http://127.0.0.1:{jump_port}/redirect?id=7"],
                {
                    "dir": allowed_dir,
                    "out": "explorer.bin",
                    "referer": REFERER,
                    "header": [f"Cookie: {COOKIE}", f"User-Agent: {UA}"],
                    # 插件会带 AriaNg 里的任务选项，认不出的键必须被安静吞掉
                    "min-split-size": "1M",
                    "continue": "true",
                },
            )
            checks("addUri 返回 16 位 gid", isinstance(gid, str) and len(gid) == 16, gid)

            state = rpc.wait(gid, "complete")
            checks("HEAD 405 的站点能下完（探测退回 GET+Range）", state.get("status") == "complete", state)
            target = os.path.join(allowed_dir, "explorer.bin")
            got = b""
            if os.path.exists(target):
                with open(target, "rb") as fh:
                    got = fh.read()
            checks("跨源 302 后 Cookie/Referer 仍在，字节一致", got == BODY,
                   f"{len(got)} vs {len(BODY)}")

            options = rpc.call("aria2.getOption", gid) or {}
            checks("getOption 回显 referer", options.get("referer") == REFERER, options.get("referer"))
            header = options.get("header")
            checks(
                "getOption 回显 header 数组（含 Cookie 与 UA）",
                isinstance(header, list)
                and any("Cookie:" in line for line in header)
                and any("User-Agent:" in line for line in header),
                header,
            )
            checks(
                "getOption 回显落盘目录",
                same_path(options.get("dir"), allowed_dir),
                f"{options.get('dir')} vs {allowed_dir}",
            )

            # -- 2. pause + changeOption 两步法 ---------------------------------
            gid2 = rpc.call(
                "aria2.addUri",
                [f"http://127.0.0.1:{jump_port}/redirect?id=8"],
                {"dir": allowed_dir, "out": "explorer-2.bin", "pause": "true"},
            )
            paused = rpc.call("aria2.tellStatus", gid2) or {}
            checks("pause=true 的任务停在 paused", paused.get("status") == "paused", paused.get("status"))
            rpc.call(
                "aria2.changeOption",
                gid2,
                {"referer": REFERER, "header": [f"Cookie: {COOKIE}"]},
            )
            rpc.call("aria2.unpause", gid2)
            state2 = rpc.wait(gid2, "complete")
            checks("changeOption 补的头对后续请求生效", state2.get("status") == "complete", state2)
            target2 = os.path.join(allowed_dir, "explorer-2.bin")
            got2 = b""
            if os.path.exists(target2):
                with open(target2, "rb") as fh:
                    got2 = fh.read()
            checks("两步法下完的文件字节一致", got2 == BODY, f"{len(got2)} vs {len(BODY)}")

            # -- 3. dir 越界：报错要指出可用目录 --------------------------------
            status, payload = caller.call("POST", "/api/downloads", {"uri": "http://127.0.0.1:9/x.bin", "dir": root})
            detail = json.dumps(payload, ensure_ascii=False)
            checks(
                "目录越界被拒且提示可用根",
                status >= 400 and "allowed" in detail.lower(),
                f"{status} {detail[:160]}",
            )
    finally:
        for handle in handles:
            handle.shutdown()
    return checks.summary()


if __name__ == "__main__":
    sys.exit(main())
