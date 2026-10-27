/**
 * 反代网关自测用的上游服务（开发脚本，不参与构建）。
 *
 * - GET /            → HTML，带 X-Frame-Options: DENY，用于验证「自动剥离」
 * - GET /echo        → 回显收到的请求头（验证 inject_headers）
 * - GET /absolute    → 返回包含自身绝对地址的 HTML（验证 rewrite_html）
 * - WS  /ws          → 原样回显（验证 WebSocket Upgrade 透传）
 *
 * 用法：node scripts/mock-upstream.mjs [port]
 */
import { createServer } from 'node:http'
import { createHash } from 'node:crypto'

const port = Number(process.argv[2] ?? 9099)

const server = createServer((req, res) => {
  const url = new URL(req.url, `http://127.0.0.1:${port}`)

  if (url.pathname === '/echo') {
    res.writeHead(200, { 'Content-Type': 'application/json' })
    res.end(JSON.stringify({ path: url.pathname, query: url.search, headers: req.headers }))
    return
  }

  if (url.pathname === '/absolute') {
    res.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8' })
    res.end(`<html><body><a href="http://127.0.0.1:${port}/echo">absolute-link</a></body></html>`)
    return
  }

  if (url.pathname === '/ws') {
    res.writeHead(426, { 'Content-Type': 'text/plain' })
    res.end('WebSocket endpoint: use an Upgrade request')
    return
  }

  res.writeHead(200, {
    'Content-Type': 'text/html; charset=utf-8',
    'X-Frame-Options': 'DENY',
    'Content-Security-Policy': "frame-ancestors 'none'",
  })
  res.end(`<html><body><h1>Mock Upstream</h1><p>path=${url.pathname}</p></body></html>`)
})

server.listen(port, '127.0.0.1', () => {
  console.log(`mock upstream on http://127.0.0.1:${port}`)
})

/** 极简 WebSocket 回显：只支持单帧文本，够反代自测用 */
server.on('upgrade', (req, socket) => {
  if (!req.url.startsWith('/ws')) {
    socket.destroy()
    return
  }
  const key = req.headers['sec-websocket-key'] ?? ''
  const accept = createHash('sha1')
    .update(`${key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11`)
    .digest('base64')

  socket.write(
    [
      'HTTP/1.1 101 Switching Protocols',
      'Upgrade: websocket',
      'Connection: Upgrade',
      `Sec-WebSocket-Accept: ${accept}`,
      '',
      '',
    ].join('\r\n'),
  )

  socket.on('data', (buf) => {
    const opcode = buf[0] & 0x0f
    if (opcode === 0x8) {
      socket.end()
      return
    }

    const masked = (buf[1] & 0x80) !== 0
    let length = buf[1] & 0x7f
    let offset = 2
    if (length === 126) {
      length = buf.readUInt16BE(2)
      offset = 4
    }
    const mask = masked ? buf.subarray(offset, offset + 4) : null
    if (masked) offset += 4

    const payload = Buffer.from(buf.subarray(offset, offset + length))
    if (mask) {
      for (let i = 0; i < payload.length; i += 1) payload[i] ^= mask[i % 4]
    }

    const reply = Buffer.from(`echo:${payload.toString('utf8')}`, 'utf8')
    if (reply.length < 126) {
      socket.write(Buffer.concat([Buffer.from([0x81, reply.length]), reply]))
    } else {
      const header = Buffer.alloc(4)
      header[0] = 0x81
      header[1] = 126
      header.writeUInt16BE(reply.length, 2)
      socket.write(Buffer.concat([header, reply]))
    }
  })

  socket.on('error', () => socket.destroy())
})
