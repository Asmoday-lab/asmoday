/**
 * 触发一次「存盘」下载。
 *
 * 真正让浏览器存盘的是服务端 `Content-Disposition: attachment`（`?download=1`）。
 *
 * 为什么用隐藏 iframe 而不是 `<a download>` + click()：
 * 点 `<a>` 是一次**顶层文档导航**，浏览器拿到 attachment 才把它转成下载。而本站的
 * 浮层（右键菜单、右键菜单所在的预览面板…）都通过 `useBackClose` 在历史里压了哨兵，
 * 关浮层时会 `history.back()` 退掉哨兵——这一次回退会把「还没转成下载」的导航一起
 * 中止（CDP 里看就是 `net::ERR_ABORTED`，且下载从未开始）。表现就是「右键下载没反应」。
 *
 * 把导航放进同源 iframe：顶层文档不动，`history.back()` 与它无关；attachment 响应
 * 照样触发下载，iframe 自身停在 about:blank，页面上什么也看不到。
 *
 * @param url 必须已带 `download=1`（或本身就是 `archive` 这种 attachment 端点）
 */
let sequence = 0

export function triggerDownload(url: string) {
  const frame = document.createElement('iframe')
  frame.name = `asmoday-download-${++sequence}`
  frame.setAttribute('aria-hidden', 'true')
  frame.tabIndex = -1
  // display:none 的 iframe 在部分浏览器里不会发起请求，用 0 尺寸 + 绝对定位代替
  frame.style.cssText =
    'position:absolute;width:0;height:0;border:0;left:-9999px;top:0;visibility:hidden'
  frame.src = url
  document.body.appendChild(frame)
  // 请求一旦交给浏览器下载器，iframe 就只剩一个空文档，及时回收避免越堆越多
  window.setTimeout(() => frame.remove(), 60_000)
}
