/**
 * 极轻量 CSV 解析（设计文档 3.2 / 8.4.8）。
 *
 * 不引第三方解析器：只处理 RFC 4180 的核心子集（引号包裹、`""` 转义、引号内换行、CRLF），
 * 并对行数与单元格长度做**强制截断**，防止几十万行 / 单格超长的文件把浏览器卡死。
 */

/** 预览最多渲染多少行（设计文档 8.4.8 的硬性上限） */
export const CSV_MAX_ROWS = 1000
/** 单个单元格最多保留多少字符（表格渲染的自我保护，不影响下载原文件） */
export const CSV_MAX_CELL = 2000

export interface CsvTable {
  /** 已解析的行（首行当表头，全部按数据展示） */
  rows: string[][]
  /** 是否因为行数上限被截断 */
  truncated: boolean
  /** 粗略行数（引号内换行会略有偏差，仅用于提示「还有更多」） */
  totalRows: number
}

/** 把 BOM 去掉：Excel 导出的 CSV 常带 UTF-8 BOM，留着会把首个表头变成 `\ufeffname` */
function stripBom(text: string): string {
  return text.charCodeAt(0) === 0xfeff ? text.slice(1) : text
}

export function parseCsv(text: string, maxRows = CSV_MAX_ROWS, delimiter = ','): CsvTable {
  const rows: string[][] = []
  let row: string[] = []
  let cell = ''
  let quoted = false
  let lineBreaks = 1
  let index = 0

  const source = stripBom(text)

  const pushCell = () => {
    row.push(cell.length > CSV_MAX_CELL ? `${cell.slice(0, CSV_MAX_CELL)}…` : cell)
    cell = ''
  }

  const pushRow = () => {
    pushCell()
    rows.push(row)
    row = []
  }

  while (index < source.length) {
    const char = source[index]

    if (quoted) {
      if (char === '"') {
        if (source[index + 1] === '"') {
          cell += '"'
          index += 2
          continue
        }
        quoted = false
        index += 1
        continue
      }
      if (char === '\n') lineBreaks += 1
      cell += char
      index += 1
      continue
    }

    // 只有「单元格开头」的引号才算包裹，中间出现的引号当普通字符
    if (char === '"' && cell === '') {
      quoted = true
      index += 1
      continue
    }
    if (char === delimiter) {
      pushCell()
      index += 1
      continue
    }
    if (char === '\r') {
      index += 1
      continue
    }
    if (char === '\n') {
      pushRow()
      lineBreaks += 1
      index += 1
      if (rows.length >= maxRows) {
        return { rows, truncated: true, totalRows: lineBreaks }
      }
      continue
    }

    cell += char
    index += 1
  }

  if (cell.length > 0 || row.length > 0) pushRow()

  return { rows, truncated: false, totalRows: lineBreaks }
}
