#!/usr/bin/env python3
"""aria2 分段 + 断点续传冒烟（v4.9.26）。

依赖 `scripts/aria2-test-server.py`（Range + 限速），验证：
1. 多连接分段（tellStatus.connections > 1）
2. 暂停落 `.aria2` 控制文件，已完成字节保留
3. unpause 续传（completedLength 不回零），最终 sha256 与源一致

    python scripts/smoke-aria2-resume.py --base http://127.0.0.1:4977 \
        --secret <secret> --file-url http://127.0.0.1:4978/data.bin
"""

import argparse
import hashlib
import json
import sys
import time
import urllib.error
import urllib.request

PASS = 0
FAIL = 0


def check(name, ok, detail=""):
    global PASS, FAIL
    if ok:
        PASS += 1
        print(f"  ✓ {name}")
    else:
        FAIL += 1
        print(f"  ✗ {name}  {detail}")


class Rpc:
    def __init__(self, base, secret):
        self.base = base.rstrip("/")
        self.secret = secret

    def result(self, method, params=None):
        payload = json.dumps({
            "jsonrpc": "2.0",
            "id": "resume",
            "method": method,
            "params": [f"token:{self.secret}"] + (params or []),
        }).encode()
        req = urllib.request.Request(
            self.base + "/jsonrpc",
            data=payload,
            method="POST",
            headers={"Content-Type": "application/json"},
        )
        try:
            with urllib.request.urlopen(req, timeout=15) as res:
                body = json.loads(res.read() or b"{}")
                return body.get("result"), body.get("error")
        except urllib.error.HTTPError as err:
            return None, json.loads(err.read() or b"{}").get("error")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--base", required=True)
    parser.add_argument("--secret", required=True)
    parser.add_argument("--file-url", required=True)
    parser.add_argument("--expect-sha256", required=True)
    args = parser.parse_args()
    rpc = Rpc(args.base, args.secret)

    print("[1] 分段下载（限速下观测 connections）")
    gid, error = rpc.result(
        "aria2.addUri", [[args.file_url], {"out": "resume.bin", "max-connection-per-server": "4"}]
    )
    check("addUri 成功", bool(gid), str(error))
    max_conn = 0
    info = None
    for _ in range(60):
        info, _ = rpc.result("aria2.tellStatus", [gid])
        if not info:
            break
        max_conn = max(max_conn, info.get("connections", 0))
        if info.get("completedLength", 0) > 0:
            break
        time.sleep(0.1)
    check("多连接分段（connections > 1）", max_conn > 1, f"max={max_conn}")

    print("[2] 暂停 → 控制文件 + 进度保留")
    rpc.result("aria2.pause", [gid])
    paused = None
    for _ in range(40):
        paused, _ = rpc.result("aria2.tellStatus", [gid])
        if paused and paused.get("status") == "paused":
            break
        time.sleep(0.1)
    check("状态 paused", paused and paused["status"] == "paused", str(paused)[:120])
    frozen = (paused or {}).get("completedLength", 0)
    check("已下载部分保留", frozen > 0, str(frozen))
    wait_window, _ = rpc.result("aria2.tellWaiting", [0, 0])
    check("paused 任务出现在 tellWaiting", any(t.get("gid") == gid for t in (wait_window or [])))
    check("tellWaiting 状态为 paused",
          all(t.get("status") == "paused" for t in (wait_window or []) if t.get("gid") == gid))

    print("[3] unpause 续传 → 完成")
    rpc.result("aria2.unpause", [gid])
    final = None
    for _ in range(400):
        final, _ = rpc.result("aria2.tellStatus", [gid])
        if final and final.get("status") in ("complete", "error"):
            break
        time.sleep(0.1)
    check("最终 complete", final and final["status"] == "complete",
          json.dumps(final)[:160] if final else "timeout")
    check("长度一致", final and final["completedLength"] == final["totalLength"],
          f'{final.get("completedLength")}/{final.get("totalLength")}' if final else "")

    print("[4] 内容校验")
    file_path = None
    files = (final or {}).get("files") or [{}]
    file_path = files[0].get("path")
    try:
        data = open(file_path, "rb").read()
        check("sha256 与源一致", hashlib.sha256(data).hexdigest() == args.expect_sha256)
    except OSError as err:
        check("文件可读", False, f"{file_path}: {err}")

    print("[5] 收尾")
    rpc.result("aria2.removeDownloadResult", [gid])
    print(f"\n结果：{PASS} 通过 / {FAIL} 失败")
    sys.exit(1 if FAIL else 0)


if __name__ == "__main__":
    main()
