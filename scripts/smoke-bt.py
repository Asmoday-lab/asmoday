#!/usr/bin/env python3
"""BitTorrent 分支冒烟（v4.9.27，设计文档 3.11）。

只验协议面与生命周期，不做真实传输（真实传输由 `cargo test bt::tests::local_seeder`
覆盖：本地做种端 + 直连 peer + 字节比对）。这样脚本不依赖外网、不依赖公网种子。

用法：
    python scripts/smoke-bt.py --base http://127.0.0.1:4977 --secret <rpc_secret>
"""

import argparse
import base64
import json
import sys
import time
import urllib.error
import urllib.request

PASS = 0
FAIL = 0

# 任意一个合法的 BTv1 info hash（不要求能连上 peer）
INFOHASH = "cab507494d02ebb1178b38f2e9d7be299c86b862"
MAGNET = f"magnet:?xt=urn:btih:{INFOHASH}&dn=smoke"


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
        # 本机探针不进代理：环境里的 http_proxy 会把 127.0.0.1 也送出去
        self.opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

    def call(self, method: str, params=None):
        payload = [f"token:{self.secret}"] + (params or [])
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
            with self.opener.open(req, timeout=20) as res:
                return res.status, json.loads(res.read() or b"{}")
        except urllib.error.HTTPError as err:
            try:
                return err.code, json.loads(err.read() or b"{}")
            except json.JSONDecodeError:
                return err.code, {}

    def result(self, method: str, params=None):
        status, payload = self.call(method, params)
        return payload.get("result"), payload.get("error"), status


def get_json(url: str):
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    with opener.open(url, timeout=10) as res:
        return json.loads(res.read())


def wait_status(rpc: Rpc, gid: str, wanted: tuple, timeout: float = 10.0) -> str:
    deadline = time.time() + timeout
    last = "?"
    while time.time() < deadline:
        status, error, _ = rpc.result("aria2.tellStatus", [gid])
        if error is None and status:
            last = status.get("status", "?")
            if last in wanted:
                return last
        time.sleep(0.2)
    return last


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--base", default="http://127.0.0.1:4977")
    parser.add_argument("--secret", required=True)
    parser.add_argument("--bad-dir", default="/etc")
    args = parser.parse_args()
    rpc = Rpc(args.base, args.secret)

    print("1. 能力声明")
    version, error, _ = rpc.result("aria2.getVersion", [])
    check("getVersion 成功", error is None, str(error))
    features = (version or {}).get("enabledFeatures", [])
    check("enabledFeatures 含 BitTorrent", "BitTorrent" in features, str(features))

    print("2. 非法种子被明确拒绝")
    _, error, _ = rpc.result("aria2.addTorrent", ["not-base64@@@"])
    check("addTorrent 非 base64 → fault", error is not None, str(error))
    _, error, _ = rpc.result(
        "aria2.addTorrent", [base64.b64encode(b"not a torrent").decode()]
    )
    check("addTorrent 非 torrent 字节 → fault", error is not None, str(error))

    print("3. magnet 建任务")
    gid, error, _ = rpc.result("aria2.addUri", [[MAGNET]])
    check("addUri(magnet) 返回 GID", error is None and isinstance(gid, str) and len(gid) == 16, str(gid) + str(error))
    if not gid:
        print(f"\n结果：{PASS} passed, {FAIL} failed")
        return 1

    status = wait_status(rpc, gid, ("active", "waiting", "error"), 10)
    check("任务进入 active/waiting", status in ("active", "waiting"), status)

    info, error, _ = rpc.result("aria2.tellStatus", [gid])
    check("tellStatus.infoHash 正确", (info or {}).get("infoHash") == INFOHASH, str(info))
    check("tellStatus.dir 非空", bool((info or {}).get("dir")), str(info))
    check("tellStatus 带 files 数组", isinstance((info or {}).get("files"), list), str(info))
    check("totalLength 字段存在", "totalLength" in (info or {}), str(info))

    keys, error, _ = rpc.result("aria2.tellStatus", [gid, ["infoHash", "status"]])
    check(
        "keys 过滤只回 gid + 指定键",
        error is None and set((keys or {}).keys()) == {"gid", "infoHash", "status"},
        str(keys),
    )

    print("4. 暂停 / 恢复")
    _, error, _ = rpc.result("aria2.pause", [gid])
    check("pause 成功", error is None, str(error))
    check("状态转 paused", wait_status(rpc, gid, ("paused",), 10) == "paused")
    _, error, _ = rpc.result("aria2.unpause", [gid])
    check("unpause 成功", error is None, str(error))
    check("状态回 active", wait_status(rpc, gid, ("active", "waiting"), 10) in ("active", "waiting"))

    print("5. 空列表查询不炸")
    peers, error, _ = rpc.result("aria2.getPeers", [gid])
    check("getPeers 返回数组", error is None and isinstance(peers, list), str(peers))
    files, error, _ = rpc.result("aria2.getFiles", [gid])
    check("getFiles 返回数组", error is None and isinstance(files, list), str(files))

    print("6. 目录白名单对种子同样生效")
    # 用 magnet 指定越界 dir 最干净：不依赖 torrent 内容是否合法
    _, error, _ = rpc.result("aria2.addUri", [[MAGNET], {"dir": args.bad_dir}])
    check("越界 dir → fault", error is not None, str(error))

    print("7. 移除任务（文件不删）")
    _, error, _ = rpc.result("aria2.remove", [gid])
    check("remove 成功", error is None, str(error))
    stopped, error, _ = rpc.result("aria2.tellStopped", [0, 100])
    ids = {item.get("gid") for item in (stopped or [])}
    check("进入 stopped 清单", gid in ids, str(ids))

    print(f"\n结果：{PASS} passed, {FAIL} failed")
    return 0 if FAIL == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
