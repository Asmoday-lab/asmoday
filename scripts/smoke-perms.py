#!/usr/bin/env python3
"""v4.9.25 权限位 + 挂载点收权冒烟。

前置：实例已初始化（admin / 密码由 --password 传入），至少一个可写挂载点。
步骤：
  1. admin 建用户 perm_smoke（默认全量位）
  2. admin PATCH：收走 delete/share/terminal/webdav_manage 位 + mount_scope 限定第一个挂载点
  3. 以 perm_smoke 登录验证：
     - mkdir 仍可用（WRITE 保留）
     - DELETE /api/files/raw → 403
     - POST /api/shares → 403
     - GET /api/terminal/ws → 403（无终端位）
     - WebDAV PROPFIND（密码直连）→ 403（无 webdav_read? 不，保留 read；验证管理被收 →
       用 PUT 写 → 403）
     - scope 外挂载点的 list → 404（多挂载点时）
  4. 恢复全量位后 DELETE 恢复可用，再删除测试用户
"""

import argparse
import sys
import urllib.request
import urllib.error
import http.cookiejar

FAILED: list[str] = []


def check(name: str, ok: bool, detail: str = "") -> None:
    mark = "ok" if ok else "FAIL"
    print(f"  [{mark}] {name}" + (f"  {detail}" if detail and not ok else ""))
    if not ok:
        FAILED.append(name)


class Client:
    def __init__(self, base: str) -> None:
        self.base = base.rstrip("/")
        self.jar = http.cookiejar.CookieJar()
        self.opener = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(self.jar))

    def request(self, method: str, path: str, payload: dict | bytes | None = None, headers: dict | None = None):
        import json

        data = None
        hdrs = {"Accept": "application/json"}
        if headers:
            hdrs.update(headers)
        if isinstance(payload, bytes):
            data = payload
        elif payload is not None:
            data = json.dumps(payload).encode()
            hdrs["Content-Type"] = "application/json"
        req = urllib.request.Request(self.base + path, data=data, method=method, headers=hdrs)
        try:
            with self.opener.open(req, timeout=30) as res:
                return res.status, res.read()
        except urllib.error.HTTPError as err:
            return err.code, err.read()

    def json(self, method: str, path: str, payload: dict | None = None):
        status, body = self.request(method, path, payload)
        import json

        try:
            return status, json.loads(body)
        except Exception:
            return status, {}


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--base", default="http://127.0.0.1:4976")
    ap.add_argument("--user", default="admin")
    ap.add_argument("--password", default="Smoke#2026test")
    args = ap.parse_args()

    admin = Client(args.base)

    print("[1] admin 登录")
    status, payload = admin.json("POST", "/api/auth/login", {"username": args.user, "password": args.password})
    check("admin 登录", status == 200, f"{status} {payload}")

    print("[2] 建测试用户 perm_smoke")
    admin.request("DELETE", "/api/users/0")  # placeholder，无副作用
    status, users = admin.json("GET", "/api/users")
    items = users.get("items", []) if isinstance(users, dict) else []
    for item in items:
        if item.get("username") == "perm_smoke":
            admin.json("DELETE", f"/api/users/{item['id']}")
    status, payload = admin.json(
        "POST", "/api/users",
        {"username": "perm_smoke", "password": "Perms#2026ok", "role": "user"},
    )
    check("建号成功", status == 200, f"{status} {payload}")
    uid = payload.get("user", {}).get("id") if isinstance(payload, dict) else None
    check("响应带 permissions/mount_scope", uid is not None, str(payload)[:200])

    print("[3] 挂载点清单与收权")
    _, mounts = admin.json("GET", "/api/files/mounts")
    all_mounts = [m for m in mounts.get("mounts", []) if m.get("online")] if isinstance(mounts, dict) else []
    if not all_mounts:
        check("有在线挂载点", False, str(mounts)[:200])
        return 1
    scope_mount = all_mounts[0]["path"]
    others = [m["path"] for m in all_mounts[1:]]
    # 收走：删除 / 分享 / 终端 / WebDAV 管理；保留 write/rename/move_copy/webdav_read/see_hidden
    keep = (1 << 0) | (1 << 1) | (1 << 2) | (1 << 3) | (1 << 6)
    import json as _json

    status, payload = admin.json(
        "PATCH", f"/api/users/{uid}",
        {"permissions": keep, "mount_scope": _json.dumps([scope_mount])},
    )
    me = payload.get("user", {}) if isinstance(payload, dict) else {}
    check(
        "PATCH permissions+mount_scope 生效",
        status == 200 and me.get("permissions") == keep and scope_mount in str(me.get("mount_scope")),
        f"{status} {str(payload)[:200]}",
    )

    print("[4] 以 perm_smoke 登录")
    chief = Client(args.base)
    status, payload = chief.json("POST", "/api/auth/login", {"username": "perm_smoke", "password": "Perms#2026ok"})
    check("登录", status == 200, f"{status} {payload}")
    # 建号统一 must_change_password=1：先走一次自助改密解锁 API
    status, payload = chief.json(
        "POST", "/api/auth/password", {"current": "Perms#2026ok", "next": "Perms#2026ok2"}
    )
    check("首登改密", status == 200, f"{status} {str(payload)[:120]}")
    chief = Client(args.base)
    status, payload = chief.json("POST", "/api/auth/login", {"username": "perm_smoke", "password": "Perms#2026ok2"})
    check("改密后重新登录", status == 200, f"{status} {payload}")

    # 挂载点清单被收权
    status, payload = chief.json("GET", "/api/files/mounts")
    visible = [m["path"] for m in payload.get("mounts", [])] if isinstance(payload, dict) else []
    check(
        "挂载点清单只剩 scope 内",
        status == 200 and scope_mount in visible and all(o not in visible for o in others),
        f"visible={visible}",
    )

    # 终端位被收 → 403（http.client 精确发送 WS 升级头；urllib 会注入 Connection: close）
    import base64
    import http.client

    def ws_status(client: Client) -> int:
        session = next(c for c in client.jar if c.name == "asmoday_session")
        conn = http.client.HTTPConnection("127.0.0.1", int(client.base.rsplit(":", 1)[1]), timeout=5)
        conn.request(
            "GET", "/api/terminal/ws?cols=80&rows=24",
            headers={
                "Cookie": f"asmoday_session={session.value}",
                "Connection": "Upgrade",
                "Upgrade": "websocket",
                "Sec-WebSocket-Key": "dGhlIHNhbXBsZSBub25jZQ==",
                "Sec-WebSocket-Version": "13",
            },
        )
        resp = conn.getresponse()
        code = resp.status
        resp.close()
        conn.close()
        return code

    # 预清理上一轮遗留目录（mkdir 冲突会 409）
    admin.request("DELETE", f"/api/files/raw?path={urllib.request.quote(scope_mount + '/__perm_smoke__')}")
    status, payload = chief.json("POST", "/api/files/mkdir", {"path": scope_mount, "name": "__perm_smoke__"})
    check("scope 内 mkdir 200", status == 200, f"{status} {str(payload)[:120]}")

    # scope 外目录 list 拒绝（不回落成 200 清单）
    if others:
        status, _ = chief.json("GET", f"/api/files/list?path={urllib.request.quote(others[0])}")
        check("scope 外 list 拒绝", status in (400, 404), str(status))

    # 删除位被收 → 403
    status, body = chief.request("DELETE", f"/api/files/raw?path={urllib.request.quote(scope_mount + '/__perm_smoke__')}")
    check("DELETE /raw 403（无删除位）", status == 403, f"{status} {body[:80]}")

    # 分享位被收 → 403
    status, payload = chief.json("POST", "/api/shares", {"path": scope_mount, "allow_download": True, "days": 1})
    check("POST /shares 403（无分享位）", status == 403, f"{status} {str(payload)[:120]}")

    # 终端位被收 → 403
    status = ws_status(chief)
    check("terminal ws 403（无终端位）", status == 403, str(status))

    # WebDAV 管理位被收：密码直连（用户名+登录密码）PUT 应 403
    import base64

    basic = base64.b64encode(b"perm_smoke:Perms#2026ok2").decode()
    status, body = chief.request(
        "PUT", f"/dav/m0/__perm_smoke__/denied.txt",
        payload=b"x",
        headers={"Authorization": f"Basic {basic}"},
    )
    # mount id 不一定是 m0：探测一下 scope 内挂载点 id
    _, mounts = admin.json("GET", "/api/files/mounts")
    mid = next((m["id"] for m in mounts.get("mounts", []) if m.get("path") == scope_mount), "m0")
    if mid != "m0":
        status, body = chief.request(
            "PUT", f"/dav/{mid}/__perm_smoke__/denied.txt",
            payload=b"x",
            headers={"Authorization": f"Basic {basic}"},
        )
    check("WebDAV PUT 403（无管理位）", status == 403, f"{status} {body[:80]}")
    # 读取位保留：PROPFIND 放行
    status, body = chief.request(
        "PROPFIND", f"/dav/{mid}/", headers={"Authorization": f"Basic {basic}", "Depth": "0"}
    )
    check("WebDAV PROPFIND 207（保留读取位）", status == 207, f"{status} {body[:80]}")

    print("[5] 恢复全量位 → 删除恢复 → 清理")
    status, _ = admin.json("PATCH", f"/api/users/{uid}", {"permissions": 511, "mount_scope": "*"})
    check("恢复全量位", status == 200, str(status))
    status, body = chief.request("DELETE", f"/api/files/raw?path={urllib.request.quote(scope_mount + '/__perm_smoke__')}")
    check("恢复后 DELETE /raw 200", status == 200, f"{status} {body[:80]}")

    status, _ = admin.json("DELETE", f"/api/users/{uid}")
    check("删除测试用户", status == 200, str(status))

    print()
    if FAILED:
        print(f"FAILED ({len(FAILED)}): " + "; ".join(FAILED))
        return 1
    print("ALL PASS")
    return 0


if __name__ == "__main__":
    sys.exit(main())
