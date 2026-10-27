/**
 * 开发期截图工具（不参与生产构建）。
 * 用本机 Edge 的 headless 模式 + CDP 给页面拍照，用于人工核对布局与主题。
 *
 * 用法：node scripts/shot.mjs <url> <out.png> [dark|light] [width] [height] [mobile]
 */
import { spawn } from 'node:child_process'
import { mkdtempSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

const EDGE = 'C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe'
const [, , url, out, theme = 'dark', widthArg = '1440', heightArg = '900', mobileArg] = process.argv
const width = Number(widthArg)
const height = Number(heightArg)
const isMobile = mobileArg === 'mobile'

const port = 9333
const profile = mkdtempSync(join(tmpdir(), 'asmoday-cdp-'))
const child = spawn(
  EDGE,
  [
    '--headless=new',
    `--remote-debugging-port=${port}`,
    `--user-data-dir=${profile}`,
    '--no-first-run',
    '--no-default-browser-check',
    '--disable-gpu',
    '--hide-scrollbars',
    'about:blank',
  ],
  { stdio: 'ignore' },
)

const sleep = (ms) => new Promise((r) => setTimeout(r, ms))

let targets = []
for (let i = 0; i < 80; i++) {
  try {
    targets = await (await fetch(`http://127.0.0.1:${port}/json/list`)).json()
    if (targets.some((t) => t.type === 'page')) break
  } catch {
    /* 端口还没起来 */
  }
  await sleep(250)
}

const page = targets.find((t) => t.type === 'page')
if (!page) {
  child.kill()
  throw new Error('未能连上 Edge 调试端口')
}

const ws = new WebSocket(page.webSocketDebuggerUrl)
await new Promise((resolve, reject) => {
  ws.onopen = resolve
  ws.onerror = reject
})

let seq = 0
const pending = new Map()
ws.onmessage = (event) => {
  const message = JSON.parse(event.data)
  const slot = pending.get(message.id)
  if (!slot) return
  pending.delete(message.id)
  if (message.error) slot.reject(new Error(JSON.stringify(message.error)))
  else slot.resolve(message.result)
}
const send = (method, params = {}) =>
  new Promise((resolve, reject) => {
    const id = ++seq
    pending.set(id, { resolve, reject })
    ws.send(JSON.stringify({ id, method, params }))
  })

await send('Page.enable')
await send('Runtime.enable')
await send('Emulation.setDeviceMetricsOverride', {
  width,
  height,
  deviceScaleFactor: 2,
  mobile: isMobile,
})
await send('Page.navigate', { url })
await sleep(1800)

if (theme === 'light') {
  await send('Runtime.evaluate', {
    expression:
      "localStorage.setItem('asmoday.theme','light');document.documentElement.setAttribute('data-theme','light');document.documentElement.style.colorScheme='light';",
  })
  await sleep(500)
}

// 可选：真实点击顶栏主题按钮，验证切换逻辑（而非直接改 DOM）
if (process.env.CLICK_THEME === '1') {
  const clicked = await send('Runtime.evaluate', {
    returnByValue: true,
    expression:
      "(() => { const btn = document.querySelector('header button[title]'); if (!btn) return 'no-button'; btn.click(); return btn.getAttribute('title'); })()",
  })
  await sleep(500)
  const after = await send('Runtime.evaluate', {
    returnByValue: true,
    expression: `(() => ({ title: document.querySelector('header button[title]')?.getAttribute('title'), label: document.querySelector('header button[title] span')?.textContent, theme: document.documentElement.getAttribute('data-theme'), bg: getComputedStyle(document.body).backgroundColor, stored: localStorage.getItem('asmoday.theme') }))()`,
  })
  console.log('CLICK_THEME:', clicked.result.value, JSON.stringify(after.result.value))
}

// 可选：先在左侧目录树中逐级展开目录（支持 "scratch/pagination" 形式）
if (process.env.TREE_OPEN) {
  const segments = process.env.TREE_OPEN.split('/').filter(Boolean)
  for (let i = 0; i < segments.length; i++) {
    const suffix = JSON.stringify(`/${segments.slice(0, i + 1).join('/')}`.toLowerCase())
    const res = await send('Runtime.evaluate', {
      returnByValue: true,
      expression: `(() => {
        const btn = [...document.querySelectorAll('aside button[title]')].find((b) => (b.getAttribute('title') || '').toLowerCase().endsWith(${suffix}))
        if (!btn) return 'not-found'
        btn.click()
        return 'ok'
      })()`,
    })
    console.log('TREE_OPEN:', segments.slice(0, i + 1).join('/'), res.result.value)
    await sleep(1000)
  }
}

// 可选：切换到列表视图
if (process.env.CLICK_LIST === '1') {
  await send('Runtime.evaluate', {
    expression: "[...document.querySelectorAll('button[title=\"列表视图\"]')][0]?.click()",
  })
  await sleep(600)
}

// 可选：双击打开指定文件（验证编辑器与语言包按需加载）
if (process.env.OPEN_FILE) {
  const target = JSON.stringify(process.env.OPEN_FILE)
  await send('Runtime.evaluate', {
    expression: `(() => { const btn = [...document.querySelectorAll('button[title]')].find(b => b.getAttribute('title') === ${target}); if (!btn) return 'not-found'; btn.dispatchEvent(new MouseEvent('dblclick', { bubbles: true })); return 'ok' })()`,
  })
  await sleep(2500)
}
// 可选：移动端展开抽屉
if (process.env.OPEN_DRAWER === '1') {
  await send('Runtime.evaluate', {
    expression: "[...document.querySelectorAll('button')].find(b=>b.getAttribute('aria-label')==='打开菜单')?.click()",
  })
  await sleep(600)
}

// 可选：在编辑器内模拟 Ctrl+S，验证快捷键保存链路
if (process.env.SAVE_KEY === '1') {
  const sent = await send('Runtime.evaluate', {
    returnByValue: true,
    expression: `(() => {
      const el = document.querySelector('.cm-content')
      if (!el) return 'no-editor'
      el.dispatchEvent(new KeyboardEvent('keydown', { key: 's', code: 'KeyS', ctrlKey: true, bubbles: true }))
      return 'sent'
    })()`,
  })
  await sleep(900)
  console.log('SAVE_KEY:', sent.result.value)
}

// 可选：在打开文件之后再切主题，验证编辑器是否跟随
if (process.env.CLICK_THEME_LAST === '1') {
  const res = await send('Runtime.evaluate', {
    returnByValue: true,
    expression:
      "(() => { const btn = document.querySelector('header button[title]'); if (!btn) return 'no-button'; btn.click(); return document.documentElement.getAttribute('data-theme') ?? 'dark' })()",
  })
  await sleep(700)
  console.log('CLICK_THEME_LAST:', res.result.value)
}

// 可选：反复点击「加载更多」，验证分页拼接
if (process.env.CLICK_MORE) {
  const times = Number(process.env.CLICK_MORE)
  for (let i = 0; i < times; i++) {
    const res = await send('Runtime.evaluate', {
      returnByValue: true,
      expression: `(() => {
        const btn = [...document.querySelectorAll('footer button')].find((b) => (b.textContent || '').includes('加载更多'))
        if (!btn) return 'no-button'
        btn.click()
        return 'clicked'
      })()`,
    })
    await sleep(900)
    if (res.result.value !== 'clicked') break
  }
  const footer = await send('Runtime.evaluate', {
    returnByValue: true,
    expression: `(() => ({ footer: document.querySelector('footer')?.textContent?.replace(/\\s+/g, ' ').trim(), items: document.querySelectorAll('[data-entry]').length }))()`,
  })
  console.log('CLICK_MORE:', JSON.stringify(footer.result.value))
}

// 可选：在终端里真实敲一条命令（CDP 键盘事件，走 xterm → WS → PTY）
if (process.env.TERM_CMD) {
  await send('Runtime.evaluate', {
    expression: "document.querySelector('.xterm-screen')?.dispatchEvent(new MouseEvent('mousedown', { bubbles: true }))",
  })
  await sleep(600)
  for (const ch of process.env.TERM_CMD) {
    await send('Input.dispatchKeyEvent', { type: 'char', text: ch })
    await sleep(25)
  }
  await send('Input.dispatchKeyEvent', {
    type: 'keyDown',
    key: 'Enter',
    code: 'Enter',
    windowsVirtualKeyCode: 13,
    nativeVirtualKeyCode: 13,
    text: '\r',
  })
  await sleep(1500)
}

// 可选：在文件列表上触发右键菜单（CTX_ENTRY=目录名），CTX_OPEN=1 时点击「在终端中打开」
if (process.env.CTX_ENTRY) {
  const name = JSON.stringify(process.env.CTX_ENTRY)
  const res = await send('Runtime.evaluate', {
    returnByValue: true,
    expression: `(() => {
      const el = [...document.querySelectorAll('[data-entry]')].find((e) => e.getAttribute('data-entry') === ${name})
      if (!el) return 'not-found'
      const r = el.getBoundingClientRect()
      el.dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, clientX: r.left + 24, clientY: r.top + 24 }))
      return 'ok'
    })()`,
  })
  console.log('CTX_ENTRY:', res.result.value)
  await sleep(500)

  if (process.env.CTX_OPEN === '1') {
    const clicked = await send('Runtime.evaluate', {
      returnByValue: true,
      expression: `(() => {
        const btn = [...document.querySelectorAll('button')].find((b) => (b.textContent || '').trim() === '在终端中打开')
        if (!btn) return 'not-found'
        btn.click()
        return 'ok'
      })()`,
    })
    console.log('CTX_OPEN:', clicked.result.value)
    await sleep(2800)
  }
}

// 可选：自动登录（LOGIN="用户名:密码"）
if (process.env.LOGIN) {
  const [user, pass] = process.env.LOGIN.split(':')
  const res = await send('Runtime.evaluate', {
    returnByValue: true,
    expression: `(() => {
      const inputs = [...document.querySelectorAll('input')]
      if (inputs.length < 2) return 'no-form'
      const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set
      const fill = (el, value) => { setter.call(el, value); el.dispatchEvent(new Event('input', { bubbles: true })) }
      fill(inputs[0], ${JSON.stringify(user ?? '')})
      fill(inputs[1], ${JSON.stringify(pass ?? '')})
      const submit = document.querySelector('button[type="submit"]')
      if (!submit) return 'no-submit'
      setTimeout(() => submit.click(), 120)
      return 'ok'
    })()`,
  })
  console.log('LOGIN:', res.result.value)
  await sleep(2200)
}

// 可选：按可见文本点击按钮（通用交互，如切换 Tab）
if (process.env.CLICK_TEXT) {
  const target = JSON.stringify(process.env.CLICK_TEXT)
  const res = await send('Runtime.evaluate', {
    returnByValue: true,
    expression: `(() => {
      const el = [...document.querySelectorAll('button, a')].find((n) => (n.textContent || '').trim() === ${target})
      if (!el) return 'not-found'
      el.click()
      return 'ok'
    })()`,
  })
  console.log('CLICK_TEXT:', process.env.CLICK_TEXT, res.result.value)
  await sleep(1200)
}

const shot = await send('Page.captureScreenshot', { format: 'png' })
writeFileSync(out, Buffer.from(shot.data, 'base64'))

if (process.env.STYLE_PROBE === '1') {
  const probe = await send('Runtime.evaluate', {
    returnByValue: true,
    expression: `(() => {
      const pick = (sel) => {
        const el = document.querySelector(sel)
        if (!el) return null
        const cs = getComputedStyle(el)
        return { sel, color: cs.color, fontSize: cs.fontSize, bg: cs.backgroundColor }
      }
      return {
        bodyBg: getComputedStyle(document.body).backgroundColor,
        asideWidth: document.querySelector('aside')?.getBoundingClientRect().width,
        cardTitle: pick('a[href="/files"] span span'),
        cardDesc: pick('a[href="/files"] span span + span'),
        heading: pick('h1'),
        muted: pick('p.text-muted'),
        names: [...document.querySelectorAll('[data-entry]')].slice(0, 4).map((b) => ({
          title: b.getAttribute('title'),
          text: (b.textContent || '').trim(),
        })),
        editor: (() => {
          const cm = document.querySelector('.cm-editor')
          if (!cm) return null
          return {
            lines: cm.querySelectorAll('.cm-line').length,
            tokens: cm.querySelectorAll('.cm-line span').length,
          }
        })(),
        toast: document.querySelector('.fixed.bottom-6')?.textContent?.trim() ?? null,
        status: document.querySelector('footer')?.textContent?.replace(/\\s+/g, ' ').trim() ?? null,
      }
    })()`,
  })
  console.log(JSON.stringify(probe.result.value, null, 2))
}

ws.close()
child.kill()
console.log(`saved ${out}`)
