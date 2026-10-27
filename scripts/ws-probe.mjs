/**
 * 终端 WebSocket 冒烟脚本（开发自测用，不参与构建）。
 *
 * 用法：node scripts/ws-probe.mjs [baseUrl] [path]
 * 验证：连上 → 收到 ready → 发命令 → 收到回显 → 断开 → 带 session_id 重连 → 仍能执行
 */
const base = process.argv[2] ?? '127.0.0.1:4242'
const targetPath = process.argv[3] ?? ''

const sleep = (ms) => new Promise((r) => setTimeout(r, ms))

function connect(sessionId) {
  const params = new URLSearchParams({ cols: '100', rows: '30' })
  if (targetPath) params.set('path', targetPath)
  if (sessionId) params.set('session_id', sessionId)
  return new WebSocket(`ws://${base}/api/terminal/ws?${params.toString()}`)
}

const output = []
let sessionId = null

function wire(socket) {
  socket.binaryType = 'arraybuffer'
  socket.addEventListener('message', (event) => {
    if (typeof event.data === 'string') {
      const control = JSON.parse(event.data)
      if (control.type === 'ready') {
        sessionId = control.session_id
        console.log('READY', JSON.stringify(control))
      } else {
        console.log('CONTROL', event.data)
      }
      return
    }
    output.push(Buffer.from(event.data).toString('utf8'))
  })
}

const first = connect(null)
wire(first)
await new Promise((resolve, reject) => {
  first.addEventListener('open', resolve)
  first.addEventListener('error', reject)
})

await sleep(900) // 等 shell 打印 banner
first.send(new TextEncoder().encode('echo ASMODAY_PROBE_OK\r'))
await sleep(1500)

const text = output.join('')
console.log('OUTPUT_HAS_ECHO:', text.includes('ASMODAY_PROBE_OK'))
console.log('OUTPUT_TAIL:', JSON.stringify(text.slice(-160)))

// 断开后立刻重连（模拟手机切后台再回来，后端 3 分钟内保留 PTY）
first.close()
await sleep(600)

const second = connect(sessionId)
wire(second)
await new Promise((resolve, reject) => {
  second.addEventListener('open', resolve)
  second.addEventListener('error', reject)
})

await sleep(900)
second.send(new TextEncoder().encode('echo ASMODAY_REATTACH_OK\r'))
await sleep(1500)

const after = output.join('')
console.log('REATTACH_OUTPUT_HAS_ECHO:', after.includes('ASMODAY_REATTACH_OK'))
console.log('BUFFER_REPLAYED:', after.length > text.length)

second.close()
await sleep(300)
process.exit(after.includes('ASMODAY_PROBE_OK') && after.includes('ASMODAY_REATTACH_OK') ? 0 : 1)
