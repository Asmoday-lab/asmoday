"""登录 API 冒烟（L2 门禁）：改认证状态机时跑这一份。

断言口径见 Design/agentreadme.md 的 GWT-AUTH-02 / 05 / 22 / 23。
所有请求都打在真实二进制上，与网页走同一套 cookie / 防爆破 / 改密拦截。
"""

from __future__ import annotations

import sys

from lib_probe import PASSWORD, Caller, Checks, Server, totp


def run(server: Server) -> int:
    caller = Caller(server.base)
    checks = Checks("login http")

    status, payload = caller.call("GET", "/api/auth/status")
    checks("未登录 status 回 totp_enabled=false", payload.get("totp_enabled") is False, payload)

    otp = server.read_otp()
    status, payload = caller.call("POST", "/api/auth/login", {"password": otp})
    checks("一次性密码登录 200", status == 200 and payload.get("must_change_password") is True, (status, payload))

    status, payload = caller.call("POST", "/api/auth/password", {"next": PASSWORD})
    checks("待改密会话设置正式密码 200", status == 200, (status, payload))

    status, payload = caller.call("GET", "/api/auth/totp/setup")
    secret = payload.get("secret", "")
    checks("TOTP setup 拿到密钥", status == 200 and len(secret) >= 16, (status, list(payload)))

    status, payload = caller.call("POST", "/api/auth/totp/enable", {"code": totp(secret)})
    recovery_codes = payload.get("recovery_codes", [])
    checks("开启 2FA 成功并发放 10 个恢复码", status == 200 and len(recovery_codes) == 10, status)

    caller.call("POST", "/api/auth/logout")
    status, payload = caller.call("GET", "/api/auth/status")
    checks(
        "未登录 status 如实回 totp_enabled=true（登录页据此给切换入口）",
        status == 200 and payload.get("totp_enabled") is True and payload.get("authenticated") is False,
        payload,
    )

    # 动态码模式：页面切到「使用动态验证码登录」后只传 totp、password 为空
    status, payload = caller.call("POST", "/api/auth/login", {"totp": totp(secret)})
    checks("动态码模式：只传 totp 登录 200", status == 200 and payload.get("status") == "ok", (status, payload))
    caller.call("POST", "/api/auth/logout")

    status, payload = caller.call("POST", "/api/auth/login", {"password": PASSWORD})
    checks("密码模式：只传 password 登录 200", status == 200 and payload.get("status") == "ok", (status, payload))
    caller.call("POST", "/api/auth/logout")

    status, payload = caller.call("POST", "/api/auth/login", {"totp": recovery_codes[0]})
    checks("动态码模式：一次性恢复码也能登录 200", status == 200 and payload.get("status") == "ok", (status, payload))
    caller.call("POST", "/api/auth/logout")

    status, payload = caller.call("POST", "/api/auth/login", {"totp": recovery_codes[0]})
    checks("用过的恢复码不能再用 → 401", status == 401 and payload.get("code") == "AUTH_INVALID", (status, payload))

    # 宽容：有人还是把 6 位码填进密码框（旧单框习惯），仍算数
    status, payload = caller.call("POST", "/api/auth/login", {"password": totp(secret)})
    checks("兼容：动态码填在 password 字段也能登录 200", status == 200 and payload.get("status") == "ok", (status, payload))
    caller.call("POST", "/api/auth/logout")

    status, payload = caller.call("POST", "/api/auth/login", {"password": "", "totp": ""})
    checks("两字段都空 → 401（不是零凭证登录）", status == 401 and payload.get("code") == "AUTH_INVALID", (status, payload))

    status, payload = caller.call("POST", "/api/auth/login", {"totp": "000000"})
    checks("动态码模式：错的动态码 → 401 AUTH_INVALID", status == 401 and payload.get("code") == "AUTH_INVALID", (status, payload))

    # 显式两个字段都给：严格双因子
    status, payload = caller.call("POST", "/api/auth/login", {"password": PASSWORD, "totp": "000000"})
    checks("严格双因子：密码对但动态码错 → 401", status == 401, (status, payload))

    status, payload = caller.call("POST", "/api/auth/login", {"password": PASSWORD, "totp": totp(secret)})
    checks("严格双因子：密码 + 动态码 → 200", status == 200, (status, payload))
    caller.call("POST", "/api/auth/logout")

    # 首尾空白不算密码的一部分（v4.9.50）：粘贴多带空格、中文输入法留下的全角空格
    # 都该照常进站——这类字符看不见，不宽容就等于「密码对却一直 401」
    status, payload = caller.call("POST", "/api/auth/login", {"password": f"  {PASSWORD}\u3000 "})
    checks("密码带首尾空白（含全角空格）→ 200", status == 200 and payload.get("status") == "ok", (status, payload))
    caller.call("POST", "/api/auth/logout")

    status, payload = caller.call("POST", "/api/auth/login", {"password": "   "})
    checks("纯空白密码 → 401（不是零凭证放行）", status == 401 and payload.get("code") == "AUTH_INVALID", (status, payload))

    # 关掉 2FA 后只剩密码这一条路
    caller.call("POST", "/api/auth/login", {"password": PASSWORD})
    status, payload = caller.call("POST", "/api/auth/totp/disable", {"password": PASSWORD, "code": totp(secret)})
    checks("关闭 2FA 200", status == 200, (status, payload))
    caller.call("POST", "/api/auth/logout")

    status, payload = caller.call("GET", "/api/auth/status")
    checks("关闭后 status 回 totp_enabled=false", payload.get("totp_enabled") is False, payload)

    status, payload = caller.call("POST", "/api/auth/login", {"totp": totp(secret)})
    checks("没开 2FA 时动态码模式一律 401", status == 401, (status, payload))

    # 写入侧同样 trim：改密时多打的空格不会变成密码的一部分（v4.9.50）
    caller.call("POST", "/api/auth/login", {"password": PASSWORD})
    status, payload = caller.call(
        "POST",
        "/api/auth/password",
        {"current": f" {PASSWORD} ", "next": "  NewPass#2026  "},
    )
    checks("改密：当前密码带空格也认，新密码首尾空格被 trim", status == 200, (status, payload))
    caller.call("POST", "/api/auth/logout")

    status, payload = caller.call("POST", "/api/auth/login", {"password": "NewPass#2026"})
    checks("改密后用不带空格的密码登录 200", status == 200 and payload.get("status") == "ok", (status, payload))
    caller.call("POST", "/api/auth/logout")

    status, payload = caller.call("POST", "/api/auth/login", {"password": "  NewPass#2026  "})
    checks("改密后带空格也能登录（同一口径）", status == 200, (status, payload))
    caller.call("POST", "/api/auth/logout")

    return checks.summary()


if __name__ == "__main__":
    sys.exit("run via probes/run_login.py --case=http")
