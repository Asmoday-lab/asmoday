/**
 * 登录页交互探针（L3 门禁）：一个框 + 可切换的登录方式。
 *
 * 用法：node login_ui.cjs password|code
 *   password —— 没开 2FA：只有密码框，页面无 2FA 字样、也没有切换入口
 *   code     —— 开了 2FA：默认仍是密码框，下方一行「使用动态验证码登录」；
 *               点一下同一个框变成 6 位动态码样式（居中、字距拉开、输满自动提交），
 *               再点一行换回密码框
 *
 * 环境变量：BASE（默认 http://127.0.0.1:4300）、SHOTS_DIR、LOGIN_PASSWORD、
 *          TOTP_SECRET、CHROME_PATH（默认自动在 ms-playwright 目录里找）
 */
const fs = require('fs')
const os = require('os')
const path = require('path')
const { chromium } = require('playwright-core')

const CASE = process.argv[2]
const BASE = process.env.BASE || 'http://127.0.0.1:4300'
const PASSWORD = process.env.LOGIN_PASSWORD || 'SecretPass2026'
const SHOTS = process.env.SHOTS_DIR || path.join(__dirname, 'shots')
const TOGGLE = '[data-testid="login-mode-toggle"]'

let failed = 0
function check(name, ok, detail) {
  if (!ok) failed++
  console.log(`${ok ? 'PASS' : 'FAIL'} ${name}${ok ? '' : ' -> ' + JSON.stringify(detail)}`)
}

/** 找浏览器：环境变量优先，其次扫 playwright 的 ms-playwright 缓存，最后交给默认解析 */
function chromePath() {
  if (process.env.CHROME_PATH) return process.env.CHROME_PATH
  const roots = [
    path.join(os.homedir(), 'AppData', 'Local', 'ms-playwright'),
    path.join(os.homedir(), '.cache', 'ms-playwright'),
  ]
  for (const root of roots) {
    if (!fs.existsSync(root)) continue
    const builds = fs
      .readdirSync(root)
      .filter((name) => name.startsWith('chromium-'))
      .sort((a, b) => Number(b.split('-')[1]) - Number(a.split('-')[1]))
    for (const build of builds) {
      const candidates = [
        path.join(root, build, 'chrome-win', 'chrome.exe'),
        path.join(root, build, 'chrome-linux', 'chrome'),
        path.join(root, build, 'chrome-mac', 'Chromium.app', 'Contents', 'MacOS', 'Chromium'),
      ]
      for (const candidate of candidates) if (fs.existsSync(candidate)) return candidate
    }
  }
  return undefined
}

async function totpNow(page, secret) {
  return page.evaluate(async (value) => {
    // 浏览器没有 b32decode：自己把 base32 文本还原成字节
    const alphabet = 'ABCDEFGHIJKLMNOPQRSTUVWXYZ234567'
    let bits = 0
    let acc = 0
    const bytes = []
    for (const ch of value.toUpperCase().replace(/=+$/, '')) {
      acc = (acc << 5) | alphabet.indexOf(ch)
      bits += 5
      if (bits >= 8) {
        bits -= 8
        bytes.push((acc >> bits) & 0xff)
      }
    }
    const key = await crypto.subtle.importKey(
      'raw',
      new Uint8Array(bytes),
      { name: 'HMAC', hash: 'SHA-1' },
      false,
      ['sign'],
    )
    const counter = Math.floor(Date.now() / 1000 / 30)
    const msg = new Uint8Array(8)
    new DataView(msg.buffer).setUint32(4, counter)
    const mac = new Uint8Array(await crypto.subtle.sign('HMAC', key, msg))
    const offset = mac[mac.length - 1] & 0x0f
    const number =
      ((mac[offset] & 0x7f) << 24) |
      (mac[offset + 1] << 16) |
      (mac[offset + 2] << 8) |
      mac[offset + 3]
    return String(number % 1000000).padStart(6, '0')
  }, secret)
}

async function openLogin(browser) {
  const context = await browser.newContext({ viewport: { width: 1100, height: 820 } })
  const page = await context.newPage()
  const errors = []
  page.on('pageerror', (err) => errors.push(String(err)))
  await page.goto(`${BASE}/login`, { waitUntil: 'networkidle' })
  await page.waitForSelector('form')
  return { context, page, errors }
}

async function waitLeave(page) {
  let ok = true
  try {
    await page.waitForURL((url) => !url.pathname.startsWith('/login'), { timeout: 15000 })
  } catch {
    ok = false
  }
  const formError = ok ? '' : ((await page.textContent('form')) || '').trim()
  return { ok, formError }
}

async function inputInfo(page) {
  return page.$eval('form input', (el) => ({
    type: el.getAttribute('type'),
    placeholder: el.getAttribute('placeholder'),
    value: el.value,
    align: getComputedStyle(el).textAlign,
    autocomplete: el.getAttribute('autocomplete'),
  }))
}

async function shot(page, name) {
  fs.mkdirSync(SHOTS, { recursive: true })
  // 固定文件名：同一状态永远覆盖同一张，不按版本号增殖
  await page.screenshot({ path: path.join(SHOTS, `${name}.png`), fullPage: true })
}

;(async () => {
  const browser = await chromium.launch({ executablePath: chromePath() })

  if (CASE === 'password') {
    const { context, page, errors } = await openLogin(browser)
    const inputs = await page.$$('form input')
    const bodyText = (await page.textContent('form')) || ''
    const info = await inputInfo(page)

    check('表单只有一个输入框', inputs.length === 1, inputs.length)
    check('该输入框是密码框', info.type === 'password', info)
    check('没有任何切换入口', (await page.$(TOGGLE)) === null, 'toggle found')
    check('页面无「动态验证码 / 恢复码」字样', !/动态验证码|恢复码/.test(bodyText), bodyText)
    check('标签就是「密码」', /密码/.test(bodyText), bodyText)
    check('页面无 JS 报错', errors.length === 0, errors)
    await shot(page, 'login-password')

    // 首尾空白（v4.9.50）：粘贴时多带的空格看不见，失焦后就该被抹掉，
    // 而不是带着它去撞一次 401
    await page.fill('form input', `  ${PASSWORD}  `)
    await page.evaluate(() => document.querySelector('form input').blur())
    const trimmed = (await inputInfo(page)).value
    check('密码框失焦自动去掉首尾空白', trimmed === PASSWORD, JSON.stringify(trimmed))

    // 字符集（v4.9.51）：空格、中文、全角符号在输入当场就该消失——屏幕上看不出来的
    // 差异留在密码里，换个设备就再也打不出来了
    await page.fill('form input', `ab c${PASSWORD}密码！　`)
    const filtered = (await inputInfo(page)).value
    check(
      '密码框只留可见 ASCII（空格、中文、全角符号被滤掉）',
      filtered === `abc${PASSWORD}`,
      JSON.stringify(filtered),
    )

    await page.fill('form input', PASSWORD)
    await page.click('form button[type="submit"]')
    const login = await waitLeave(page)
    check('密码模式能登录', login.ok, login)
    await context.close()
  }

  if (CASE === 'code') {
    const { context, page, errors } = await openLogin(browser)
    const before = await page.$$('form input')
    const beforeInfo = await inputInfo(page)
    const beforeText = (await page.textContent('form')) || ''

    check('默认仍然是密码框（方式可选，不预先锁死）', beforeInfo.type === 'password', beforeInfo)
    check('默认只有一个输入框', before.length === 1, before.length)
    check('框下方给出「使用动态验证码登录」', /使用动态验证码登录/.test(beforeText), beforeText)
    check('切到动态码之前不展示恢复码提示', !/恢复码/.test(beforeText), beforeText)
    await shot(page, 'login-2fa-default')

    // 先填一段密码：验证切换会清空、不会把密码当动态码提交出去
    await page.fill('form input', PASSWORD)
    await page.click(TOGGLE)
    const codeInfo = await inputInfo(page)
    const codeText = (await page.textContent('form')) || ''
    const afterInputs = await page.$$('form input')

    check('切换后仍然是同一个框（不是多出第二个）', afterInputs.length === 1, afterInputs.length)
    check('切换后输入框变成明文动态码样式', codeInfo.type === 'text', codeInfo)
    check('动态码框居中 + 大字距', codeInfo.align === 'center', codeInfo)
    check('占位是 6 位数字提示', codeInfo.placeholder === '000000', codeInfo)
    check('切换清空了原来的密码', codeInfo.value === '', codeInfo)
    check('标签变成「动态验证码」', /动态验证码/.test(codeText), codeText)
    check('下方提示写清恢复码这条出路', /恢复码/.test(codeText), codeText)
    check('切换后入口变成「使用密码登录」', /使用密码登录/.test(codeText), codeText)
    check('页面无 JS 报错', errors.length === 0, errors)
    await shot(page, 'login-code')

    // 动态码模式下填密码不该进站（两种模式是分开的）
    await page.fill('form input', PASSWORD)
    await page.click('form button[type="submit"]')
    await page.waitForTimeout(1200)
    const stillLogin = new URL(page.url()).pathname.startsWith('/login')
    const errText = ((await page.textContent('form')) || '').trim()
    check('动态码模式下填密码进不去（模式确实分开了）', stillLogin, page.url())
    check('失败时行内提示「密码或动态验证码不正确」', /密码或动态验证码不正确/.test(errText), errText)

    await page.click(TOGGLE)
    const backInfo = await inputInfo(page)
    check('再点一次换回密码框', backInfo.type === 'password', backInfo)

    await page.click(TOGGLE)
    if (process.env.TOTP_SECRET) {
      const code = await totpNow(page, process.env.TOTP_SECRET)
      await page.fill('form input', code)
      const auto = await waitLeave(page)
      check('6 位动态码输满自动提交并进站', auto.ok, auto)
    }
    await context.close()

    // 另开一个上下文：密码模式单独也能进站
    const second = await openLogin(browser)
    await second.page.fill('form input', PASSWORD)
    await second.page.click('form button[type="submit"]')
    const byPassword = await waitLeave(second.page)
    check('开 2FA 后密码依然能单独登录', byPassword.ok, byPassword)
    await second.context.close()
  }

  await browser.close()
  console.log(`\n[login ui:${CASE}] ${failed === 0 ? 'all passed' : failed + ' failed'}`)
  process.exit(failed === 0 ? 0 : 1)
})()
