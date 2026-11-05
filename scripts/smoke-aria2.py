#!/usr/bin/env python3
"""aria2 兼容 RPC 冒烟（v4.9.26）。

模拟 AriaNg 的典型调用序列：getVersion → addUri → 轮询 tellActive/tellStatus →
tellStopped → 文件校验 → remove/purge。另验 token 鉴权与 dir 白名单。

用法：
    python scripts/smoke-aria2.py --base http://127.0.0.1:4977 \
        --secret <rpc_secret> --file-url http://127.0.0.1:4978/data.bin
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


def check(name: str, ok: bool, detail: str = ""):
    global PASS, FAIL
    if ok:
        PASS += 1
        print(f"  ✓ {name}")
    else:
        FAIL += 1
        print(f"  ✗ {name}  {detail}")


class Rpc:
    def __init__(self, base: str, secret: str):
        self.base = base.rstrip("/")
        self.secret = secret

    def call(self, method: str, params=None, raw_secret=None):
        token = raw_secret if raw_secret is not None else f"token:{self.secret}"
        payload = [token] + (params or [])
        body = json.dumps(
            {"jsonrpc": "2.0", "id": "smoke", "method": method, "params": payload}
        ).encode()
        req = urllib.request.Request(
            self.base + "/jsonrpc",
            data=body,
            method="POST",
            headers={"Content-Type": "application/json"},
        )
        try:
            with urllib.request.urlopen(req, timeout=15) as res:
                return res.status, json.loads(res.read() or b"{}")
        except urllib.error.HTTPError as err:
            return err.code, json.loads(err.read() or b"{}")

    def result(self, method, params=None):
        status, payload = self.call(method, params)
        return payload.get("result"), status, payload


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--base", required=True)
    parser.add_argument("--secret", required=True)
    parser.add_argument("--file-url", required=True)
    parser.add_argument("--expect-sha256", default="")
    args = parser.parse_args()
    rpc = Rpc(args.base, args.secret)

    print("[1] getVersion（AriaNg 连接后第一个调用）")
    version, status, _ = rpc.result("aria2.getVersion")
    check("getVersion 200", status == 200 and version, str(status))
    features = (version or {}).get("enabledFeatures", [])
    check("enabledFeatures 不声明 BT（AriaNg 据此隐藏 BT 入口）",
          "BitTorrent" not in features and "Asmoday" in features, str(features))

    print("[2] system.multicall + listMethods（AriaNg 批量轮询依赖）")
    status, payload = rpc.call("system.multicall", [[
        {"methodName": "aria2.tellActive", "params": []},
        {"methodName": "aria2.getGlobalStat", "params": []},
    ]])
    result = payload.get("result", []) if isinstance(payload, dict) else []
    check("multicall 返回两个包裹结果",
          status == 200 and len(result) == 2 and isinstance(result[0], list),
          json.dumps(payload)[:160])
    methods, _, _ = rpc.result("system.listMethods")
    check("listMethods 含 addUri/tellStatus/multicall",
          "aria2.addUri" in (methods or []) and "system.multicall" in (methods or []),
          str(methods)[:120])

    print("[3] 鉴权")
    status, payload = rpc.call("aria2.getVersion", [], raw_secret="token:wrong-secret")
    error = payload.get("error", {}) if isinstance(payload, dict) else {}
    check("错误密钥 → error code 1 Unauthorized",
          error.get("code") == 1 and "Unauthorized" in error.get("message", ""),
          json.dumps(payload)[:120])

    print("[4] dir 白名单")
    _, payload = rpc.call("aria2.addUri", [[args.file_url], {"dir": "/tmp/escape"}])
    error = payload.get("error", {}) if isinstance(payload, dict) else {}
    check("scope 外 dir 被拒（error）", bool(error), json.dumps(payload)[:120])

    print("[5] addUri + 轮询（AriaNg 主循环）")
    gid, _, payload = rpc.result(
        "aria2.addUri", [[args.file_url], {"out": "downloaded.bin"}]
    )
    check("addUri 返回 16 位 gid",
          isinstance(gid, str) and len(gid) == 16, json.dumps(payload)[:120])

    seen_active = False
    final = None
    for _ in range(120):
        active, _, _ = rpc.result("aria2.tellActive")
        if any(t.get("gid") == gid for t in (active or [])):
            seen_active = True
        status_info, _, _ = rpc.result("aria2.tellStatus", [gid])
        if status_info and status_info.get("status") in ("complete", "error"):
            final = status_info
            break
        time.sleep(0.25)
    print(f"    (本地高速下载，active 观测: {seen_active} —— 错过属正常)")
    check("最终 complete", final and final["status"] == "complete",
          json.dumps(final)[:160] if final else "timeout")
    check("tellStatus 字段齐全（AriaNg 直接绑定）",
          all(k in (final or {}) for k in (
              "totalLength", "completedLength", "downloadSpeed", "connections",
              "dir", "files", "errorCode", "errorMessage")),
          str(sorted((final or {}).keys())))

    print("[6] 文件内容校验")
    file_dir = final["dir"]
    file_path = f"{file_dir}/downloaded.bin" if not file_dir.endswith("/") else file_dir + "downloaded.bin"
    try:
        with open(file_path, "rb") as handle:
            data = handle.read()
        if args.expect_sha256:
            check("sha256 与源一致",
                  hashlib.sha256(data).hexdigest() == args.expect_sha256)
        else:
            check("文件非空", len(data) > 0, str(len(data)))
    except OSError as err:
        check("文件落盘", False, str(err))

    print("[7] tellStopped / getGlobalStat / removeDownloadResult")
    stopped, _, _ = rpc.result("aria2.tellStopped", [0, 0])
    check("tellStopped 含该任务",
          any(t.get("gid") == gid for t in (stopped or [])), str(stopped)[:120])
    stat, _, _ = rpc.result("aria2.getGlobalStat")
    check("numStopped >= 1", (stat or {}).get("numStopped", 0) >= 1, str(stat))
    removed, _, _ = rpc.result("aria2.removeDownloadResult", [gid])
    check("removeDownloadResult OK", removed == "OK", str(removed))

    print(f"\n结果：{PASS} 通过 / {FAIL} 失败")
    sys.exit(1 if FAIL else 0)


if __name__ == "__main__":
    main()
