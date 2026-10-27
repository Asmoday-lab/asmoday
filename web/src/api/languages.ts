/**
 * 编辑器语言映射：按扩展名 / 文件名选出 CodeMirror 语言键。
 */

export type LanguageKey = 'js' | 'jsx' | 'ts' | 'tsx' | 'rust' | 'python' | 'toml' | 'nginx' | 'plain'

export interface LanguageOption {
  key: LanguageKey
  label: string
}

/** 编辑器内的手动切换列表 */
export const LANGUAGES: LanguageOption[] = [
  { key: 'plain', label: '纯文本' },
  { key: 'js', label: 'JavaScript' },
  { key: 'ts', label: 'TypeScript' },
  { key: 'rust', label: 'Rust' },
  { key: 'python', label: 'Python' },
  { key: 'toml', label: 'TOML' },
  { key: 'nginx', label: 'Nginx' },
]

const BY_EXT: Record<string, LanguageKey> = {
  js: 'js',
  mjs: 'js',
  cjs: 'js',
  jsx: 'jsx',
  ts: 'ts',
  mts: 'ts',
  cts: 'ts',
  tsx: 'tsx',
  vue: 'js',
  rs: 'rust',
  py: 'python',
  pyi: 'python',
  toml: 'toml',
  conf: 'nginx',
  nginx: 'nginx',
}

/** 依据文件名判定语言；未命中返回 plain（不强加高亮）。 */
export function detectLanguage(fileName: string): LanguageKey {
  const lower = fileName.toLowerCase()
  if (lower === 'nginx.conf' || lower.endsWith('.nginx.conf')) return 'nginx'
  const ext = lower.includes('.') ? lower.slice(lower.lastIndexOf('.') + 1) : ''
  return BY_EXT[ext] ?? 'plain'
}
