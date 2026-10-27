/**
 * 反代 WebSocket 透传自测（开发脚本）。
 * 用法：node scripts/proxy-ws-probe.mjs [host:port] [serviceId]
 */
const host = process.argv[2] ?? '127.0.0.1:4242'
const service = process.argv[3] ?? 'mock'

const socket = new WebSocket(`ws://${host}/proxy/${service}/ws`)
socket.binaryType = 'arraybuffer'

const result = await new Promise((resolve) => {
  const timer = setTimeout(() => resolve({ ok: false, reason: 'timeout' }), 6000)
  socket.addEventListener('open', () => socket.send('hello-asmoday'))
  socket.addEventListener('message', (event) => {
    clearTimeout(timer)
    resolve({ ok: true, data: typeof event.data === 'string' ? event.data : '<binary>' })
  })
  socket.addEventListener('error', () => {
    clearTimeout(timer)
    resolve({ ok: false, reason: 'error' })
  })
})

console.log('PROXY_WS:', JSON.stringify(result))
socket.close()
process.exit(result.ok && result.data === 'echo:hello-asmoday' ? 0 : 1)
