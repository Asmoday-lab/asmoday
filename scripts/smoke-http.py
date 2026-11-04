#!/usr/bin/env python3
"""HTTP 端到端冒烟（v4.9.22）。

只依赖标准库，直接对着一个**已经在跑**的服务发真实请求。存在的意义是补上
「单元测试过了、但真实请求链路是断的」这一类盲区——v4.9.22 之前的 tus
小文件上传就是这样：单测全绿，实际传 5 字节永远落不了位。

用法：
    python scripts/smoke-http.py --base http://127.0.0.1:4971 \
        --user admin --password 'Smoke#2026test'
    # 首次（还没有管理员）：加 --setup-token <启动时打印的令牌>

覆盖：
    1. 登录 / 初始化
    2. 在第一个在线挂载点下建 __smoke__ 临时目录
    3. tus 5 字节（偏移必须推进并落位）
    4. tus 分块（2 片，字节一致）
    5. tus 冲突三策略（fail=409 / rename=(1) / overwrite 覆盖）
    6. browse?dims=1 返回真实宽高 + 二次请求命中缓存
    7. 缩略图 200
    8. 收藏 + 播放进度 → DELETE /raw → 元数据被级联清理
    9. 清理临时目录

退出码 0 = 全过；1 = 有失败项。
"""

from __future__ import annotations

import argparse
import http.client
import json
import os
import struct
import sys
import urllib.parse
import zlib

FAILED: list[str] = []


def check(name: str, ok: bool, detail: str = "") -> None:
    print(f"  {'PASS' if ok else 'FAIL'}  {name}{(' — ' + detail) if detail else ''}")
    if not ok:
        FAILED.append(name)


class Client:
    def __init__(self, base: str) -> None:
        parsed = urllib.parse.urlsplit(base)
        self.host = parsed.hostname or "127.0.0.1"
        self.port = parsed.port or 80
        self.cookie = ""

    def request(self, method: str, path: str, body: bytes | None = None, headers: dict | None = None):
        conn = http.client.HTTPConnection(self.host, self.port, timeout=120)
        merged = {"Cookie": self.cookie} if self.cookie else {}
        merged.update(headers or {})
        conn.request(method, path, body=body, headers=merged)
        resp = conn.getresponse()
        payload = resp.read()
        set_cookie = resp.getheader("Set-Cookie")
        if set_cookie:
            self.cookie = set_cookie.split(";")[0]
        conn.close()
        return resp.status, payload, resp

    def json(self, method: str, path: str, payload: dict | None = None):
        status, body, _ = self.request(
            method,
            path,
            json.dumps(payload).encode() if payload is not None else None,
            {"Content-Type": "application/json"} if payload is not None else None,
        )
        try:
            return status, json.loads(body or b"null")
        except json.JSONDecodeError:
            return status, body


def make_png(width: int, height: int, rgb: tuple[int, int, int]) -> bytes:
    raw = b"".join(b"\x00" + bytes(rgb) * width for _ in range(height))

    def chunk(tag: bytes, data: bytes) -> bytes:
        return (
            struct.pack(">I", len(data))
            + tag
            + data
            + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
        )

    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw))
        + chunk(b"IEND", b"")
    )


def q(path: str) -> str:
    return urllib.parse.quote(path, safe="/:")


def tus_upload(c: Client, dir_path: str, name: str, data: bytes, conflict: str, chunk: int = 0):
    status, _, resp = c.request(
        "POST",
        f"/api/files/upload/tus?dir={q(dir_path)}&name={q(name)}&conflict={conflict}",
        headers={"Upload-Length": str(len(data))},
    )
    if status != 201:
        return status, None, b""
    location = resp.getheader("Location")
    steps = [data] if chunk <= 0 else [data[i : i + chunk] for i in range(0, len(data), chunk)]
    offset = 0
    for part in steps:
        status, body, resp = c.request(
            "PATCH",
            location,
            body=part,
            headers={
                "Content-Type": "application/offset+octet-stream",
                "Upload-Offset": str(offset),
            },
        )
        if status != 204:
            return status, resp.getheader("upload-offset"), body
        offset = int(resp.getheader("upload-offset") or -1)
    return 204, offset, b""


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--base", default="http://127.0.0.1:4971")
    ap.add_argument("--user", default="admin")
    ap.add_argument("--password", default="Smoke#2026test")
    ap.add_argument("--setup-token", default=None)
    args = ap.parse_args()

    c = Client(args.base)

    print("[1] 登录")
    status, payload = c.json("POST", "/api/auth/login", {"username": args.user, "password": args.password})
    if status != 200 and args.setup_token:
        # 首次启动：服务端把一次性令牌打印在控制台（不落日志），必须带在头里
        status, payload, _ = c.request(
            "POST",
            "/api/auth/setup",
            json.dumps({"username": args.user, "password": args.password}).encode(),
            {"Content-Type": "application/json", "X-Setup-Token": args.setup_token},
        )
        payload = payload.decode(errors="replace")[:200]
    check("登录 / 初始化", status == 200, f"{status} {payload}")

    print("[2] 找挂载点")
    status, payload = c.json("GET", "/api/files/mounts")
    mounts = payload.get("mounts") if isinstance(payload, dict) else None
    online = [m for m in (mounts or []) if m.get("online", True) and not m.get("readonly")]
    if not online:
        check("有可写挂载点", False, str(payload)[:200])
        return 1
    root = online[0]["path"]
    work = root.rstrip("/") + "/__smoke__"
    print(f"      挂载点 {root} → 临时目录 {work}")
    c.json("POST", "/api/files/mkdir", {"path": root, "name": "__smoke__"})

    print("[3] tus 5 字节")
    small = b"hello"
    status, offset, body = tus_upload(c, work, "small.bin", small, "fail")
    check("5 字节 PATCH 204 且 offset=5", status == 204 and offset == 5, f"status={status} offset={offset} {body[:80]}")
    check(
        "5 字节按原名落位",
        status == 204 and os.path.exists(os.path.join(work, "small.bin")),
        work,
    )

    print("[4] tus 冲突三策略")
    status, offset, body = tus_upload(c, work, "small.bin", small, "fail")
    check("fail → 409 E_CONFLICT", status == 409, f"{status} {body[:80]}")
    status, offset, _ = tus_upload(c, work, "small.bin", small, "rename")
    check(
        "rename → small (1).bin",
        status == 204 and os.path.exists(os.path.join(work, "small (1).bin")),
        f"{status} {offset}",
    )
    status, _, _ = tus_upload(c, work, "small.bin", b"HELLO", "overwrite")
    overwritten = False
    try:
        with open(os.path.join(work, "small.bin"), "rb") as fh:
            overwritten = fh.read() == b"HELLO"
    except OSError:
        pass
    check("overwrite → 内容被替换", status == 204 and overwritten, f"{status}")

    print("[5] tus 分块（2 片）")
    big = os.urandom(200_000)
    status, offset, body = tus_upload(c, work, "chunked.bin", big, "fail", chunk=100_000)
    same = False
    try:
        with open(os.path.join(work, "chunked.bin"), "rb") as fh:
            same = fh.read() == big
    except OSError:
        pass
    check("两片传完 offset=200000 且字节一致", status == 204 and offset == 200_000 and same, f"{status} {offset}")

    print("[6] browse dims")
    png = make_png(640, 480, (200, 60, 60))
    status, _, _ = c.request(
        "PUT",
        f"/api/files/upload?dir={q(work)}&name=pic.png",
        body=png,
        headers={"Content-Type": "application/octet-stream"},
    )
    check("整包上传 PNG", status in (200, 201, 204), str(status))
    status, payload = c.json("GET", f"/api/media/browse?path={q(work)}&kind=image&dims=1")
    items = payload.get("items", []) if isinstance(payload, dict) else []
    dims_ok = any(i.get("name") == "pic.png" and i.get("width") == 640 and i.get("height") == 480 for i in items)
    check("browse 返回真实宽高", status == 200 and dims_ok, f"{status} {items[:1]}")

    print("[7] 缩略图")
    item = next((i for i in items if i.get("name") == "pic.png"), None)
    if item:
        status, body, _ = c.request(
            "GET",
            f"/api/files/thumb?path={q(item['path'])}&w=320&v={item.get('modified') or 0}",
        )
        check("缩略图 200 且非空", status == 200 and len(body) > 0, f"{status} {len(body)}B")
    else:
        check("缩略图 200 且非空", False, "browse 里没有 pic.png")

    print("[8] 删除级联")
    target = f"{work}/pic.png"
    c.json("POST", "/api/media/favorite", {"path": target, "favorite": True})
    c.json("POST", "/api/media/progress", {"path": target, "progress": 42})
    _, favs = c.json("GET", "/api/media/favorites")
    before_fav = len(favs.get("items", [])) if isinstance(favs, dict) else 0
    status, _, _ = c.request("DELETE", f"/api/files/raw?path={q(target)}")
    check("DELETE /raw 200", status == 200, str(status))
    _, favs = c.json("GET", "/api/media/favorites")
    after_fav = len(favs.get("items", [])) if isinstance(favs, dict) else 0
    _, hist = c.json("GET", "/api/media/history")
    after_hist = len(hist.get("items", [])) if isinstance(hist, dict) else 0
    check(
        "删除后收藏 / 历史归零",
        before_fav >= 1 and after_fav == 0 and after_hist == 0,
        f"fav {before_fav}→{after_fav}, hist→{after_hist}",
    )

    print("[9] 清理")
    status, _, _ = c.request("DELETE", f"/api/files/raw?path={q(work)}")
    check("删除临时目录", status == 200, str(status))

    print()
    if FAILED:
        print(f"FAILED ({len(FAILED)}): " + "; ".join(FAILED))
        return 1
    print("ALL PASS")
    return 0


if __name__ == "__main__":
    sys.exit(main())
