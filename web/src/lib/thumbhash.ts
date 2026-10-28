/**
 * ThumbHash 解码（设计文档 3.4.3 / 8.4.2：照片时间线必须用 ThumbHash 模糊占位防抖动）。
 *
 * 后端只把 25 字节左右的 ThumbHash 以 base64 随列表 JSON 返回（v2.8.2 起不落盘），
 * 前端在这里把它还原成 32px 级别的小图，铺在缩略图底下做「模糊占位」：
 * 真图没到之前就有正确的构图与配色，图片加载完成后无缝盖上去，不会整块跳色。
 *
 * 算法逐行对齐 Rust `thumbhash 0.1.0` 的 `thumb_hash_to_rgba`（后端算平均色用的是同一套），
 * 保证前端模糊图与后端 `color` 字段一致。base64 用标准字母表（`data_encoding::BASE64`，带 `=` 填充）。
 */

export interface ThumbHashBitmap {
  width: number
  height: number
  rgba: Uint8Array
}

function decodeBase64(input: string): Uint8Array {
  const binary = atob(input.trim())
  const out = new Uint8Array(binary.length)
  for (let i = 0; i < binary.length; i += 1) out[i] = binary.charCodeAt(i)
  return out
}

/** base64 ThumbHash → 32px 级别的 RGBA 位图；数据损坏时返回 null */
export function thumbHashToBitmap(hash: string): ThumbHashBitmap | null {
  try {
    return decodeThumbHash(decodeBase64(hash))
  } catch {
    return null
  }
}

function decodeThumbHash(bytes: Uint8Array): ThumbHashBitmap {
  if (bytes.length < 5) throw new Error('thumbhash too short')

  let offset = 0
  const readByte = () => {
    if (offset >= bytes.length) throw new Error('thumbhash truncated')
    return bytes[offset++]
  }

  // 前 3 字节：亮度/黄蓝/红绿 的直流分量 + 亮度尺度 + 是否有 alpha
  const header24 = readByte() | (readByte() << 8) | (readByte() << 16)
  const header16 = readByte() | (readByte() << 8)

  const lDc = (header24 & 63) / 63
  const pDc = ((header24 >> 6) & 63) / 31.5 - 1
  const qDc = ((header24 >> 12) & 63) / 31.5 - 1
  const lScale = ((header24 >> 18) & 31) / 31
  const hasAlpha = header24 >> 23 !== 0
  const pScale = ((header16 >> 3) & 63) / 63
  const qScale = ((header16 >> 9) & 63) / 63
  const isLandscape = header16 >> 15 !== 0
  const lMax = hasAlpha ? 5 : 7
  const lx = Math.max(3, isLandscape ? lMax : header16 & 7)
  const ly = Math.max(3, isLandscape ? (header16 & 7) : lMax)

  let aDc = 1
  let aScale = 1
  if (hasAlpha) {
    const header8 = readByte()
    aDc = (header8 & 15) / 15
    aScale = (header8 >> 4) / 15
  }

  // AC 系数按 4 bit 量化，两两共用一个字节（低半字节在前）
  let pending: number | null = null
  const decodeChannel = (nx: number, ny: number, scale: number): number[] => {
    const ac: number[] = []
    for (let cy = 0; cy < ny; cy += 1) {
      let cx = cy > 0 ? 0 : 1
      while (cx * ny < nx * (ny - cy)) {
        let bits: number
        if (pending !== null) {
          bits = pending
          pending = null
        } else {
          const byte = readByte()
          pending = byte >> 4
          bits = byte & 15
        }
        ac.push((bits / 7.5 - 1) * scale)
        cx += 1
      }
    }
    return ac
  }

  const lAc = decodeChannel(lx, ly, lScale)
  const pAc = decodeChannel(3, 3, pScale * 1.25)
  const qAc = decodeChannel(3, 3, qScale * 1.25)
  const aAc = hasAlpha ? decodeChannel(5, 5, aScale) : []

  // 纵横比决定小图尺寸（长边固定 32）。与 Rust `thumb_hash_to_approximate_aspect_ratio`
  // 一致：用未 clamp 的原始 lx/ly（clamp 只影响系数个数，不影响比例）。
  const lMinRaw = header16 & 7
  const ratio = (isLandscape ? lMax : lMinRaw) / (isLandscape ? lMinRaw : lMax)
  const width = Math.max(1, ratio > 1 ? 32 : Math.round(32 * ratio))
  const height = Math.max(1, ratio > 1 ? Math.round(32 / ratio) : 32)

  const rgba = new Uint8Array(width * height * 4)
  const fx = new Float64Array(8)
  const fy = new Float64Array(8)

  for (let y = 0; y < height; y += 1) {
    for (let x = 0; x < width; x += 1) {
      let l = lDc
      let p = pDc
      let q = qDc
      let a = aDc

      const fxCount = Math.max(lx, hasAlpha ? 5 : 3)
      for (let cx = 0; cx < fxCount; cx += 1) {
        fx[cx] = Math.cos((Math.PI / width) * (x + 0.5) * cx)
      }
      const fyCount = Math.max(ly, hasAlpha ? 5 : 3)
      for (let cy = 0; cy < fyCount; cy += 1) {
        fy[cy] = Math.cos((Math.PI / height) * (y + 0.5) * cy)
      }

      let j = 0
      for (let cy = 0; cy < ly; cy += 1) {
        let cx = cy > 0 ? 0 : 1
        const fy2 = fy[cy] * 2
        while (cx * ly < lx * (ly - cy)) {
          l += lAc[j] * fx[cx] * fy2
          j += 1
          cx += 1
        }
      }

      j = 0
      for (let cy = 0; cy < 3; cy += 1) {
        let cx = cy > 0 ? 0 : 1
        const fy2 = fy[cy] * 2
        while (cx < 3 - cy) {
          const f = fx[cx] * fy2
          p += pAc[j] * f
          q += qAc[j] * f
          j += 1
          cx += 1
        }
      }

      if (hasAlpha) {
        j = 0
        for (let cy = 0; cy < 5; cy += 1) {
          let cx = cy > 0 ? 0 : 1
          const fy2 = fy[cy] * 2
          while (cx < 5 - cy) {
            a += aAc[j] * fx[cx] * fy2
            j += 1
            cx += 1
          }
        }
      }

      const b = l - (2 / 3) * p
      const r = (3 * l - b + q) / 2
      const g = r - q

      const index = (x + y * width) * 4
      rgba[index] = clampChannel(r)
      rgba[index + 1] = clampChannel(g)
      rgba[index + 2] = clampChannel(b)
      rgba[index + 3] = clampChannel(a)
    }
  }

  return { width, height, rgba }
}

function clampChannel(value: number): number {
  return Math.max(0, Math.min(255, Math.round(value * 255)))
}

/** 解码结果缓存：列表滚动时同一张图会被反复取，避免重复做 DCT */
const urlCache = new Map<string, string>()
const MAX_CACHE = 400

/**
 * base64 ThumbHash → 可直接塞进 `<img :src>` 的 dataURL。
 * 解码失败（脏数据 / 老文件）返回 null，调用方回退纯色占位。
 */
export function thumbHashToDataUrl(hash: string | null | undefined): string | null {
  if (!hash) return null
  const cached = urlCache.get(hash)
  if (cached !== undefined) return cached || null

  const bitmap = thumbHashToBitmap(hash)
  let url = ''
  if (bitmap && typeof document !== 'undefined') {
    const canvas = document.createElement('canvas')
    canvas.width = bitmap.width
    canvas.height = bitmap.height
    const ctx = canvas.getContext('2d')
    if (ctx) {
      const image = ctx.createImageData(bitmap.width, bitmap.height)
      image.data.set(bitmap.rgba)
      ctx.putImageData(image, 0, 0)
      url = canvas.toDataURL('image/png')
    }
  }

  if (urlCache.size >= MAX_CACHE) urlCache.clear()
  urlCache.set(hash, url)
  return url || null
}
