/**
 * 密码 / 口令的字符集收口（v4.9.51）。
 *
 * 允许的只有**可见 ASCII**（`!`..=`~`），即后端 `core::auth::check_password_charset`
 * 那个集合：不含空格、不含控制字符、不含中文与全角符号。
 *
 * 为什么要在输入时就滤掉，而不是提交后报错：空格、全角符号、中文逗号在屏幕上和
 * ASCII 长得一样，用户根本看不出「哪里不合法」；这类字符一旦进了密码，下次在另一台
 * 设备上就再也打不出来。宁可当场吞掉，也不要留下一个「看起来对、实际对不上」的密码。
 */
const DISALLOWED = /[^!-~]/g

export function filterPasswordChars(value: string): string {
  return value.replace(DISALLOWED, '')
}
