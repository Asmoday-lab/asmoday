/**
 * 触发一次「存盘」下载。
 *
 * 真正让浏览器存盘的是服务端 `Content-Disposition: attachment`（`?download=1`），
 * 这里的 `<a download>` 只是兜底：部分 MIME（text/*）与重定向下它会被忽略，
 * 所以调用方给的 URL 必须已经带上 `download=1`。
 */
export function triggerDownload(url: string) {
  const link = document.createElement('a')
  link.href = url
  link.rel = 'noopener'
  link.style.display = 'none'
  document.body.appendChild(link)
  link.click()
  link.remove()
}
