/**
 * 编辑器语言映射（设计文档 3.2.2.2）。
 *
 * 两条硬规则：
 * 1. **禁止硬编码少数几种语言**——词表覆盖文档要求的全部矩阵；
 * 2. 语言包必须按需 `import()`（见 CodeEditor.vue），首屏不允许全量打包。
 *
 * 识别顺序：特殊文件名（Dockerfile / Makefile / .env …）→ 扩展名 → 纯文本。
 */

export type LanguageKey =
  // 结构化数据
  | 'json'
  | 'yaml'
  | 'xml'
  | 'toml'
  | 'csv'
  | 'properties'
  // 前端
  | 'html'
  | 'css'
  | 'scss'
  | 'javascript'
  | 'typescript'
  | 'jsx'
  | 'tsx'
  | 'vue'
  | 'svelte'
  // 后端
  | 'rust'
  | 'python'
  | 'go'
  | 'java'
  | 'cpp'
  | 'csharp'
  | 'php'
  | 'ruby'
  | 'lua'
  // 脚本与配置
  | 'shell'
  | 'powershell'
  | 'nginx'
  | 'dockerfile'
  | 'makefile'
  | 'sql'
  | 'markdown'
  // 底层
  | 'assembly'
  | 'protobuf'
  // 兜底
  | 'plain'

export interface LanguageOption {
  key: LanguageKey
  label: string
}

/**
 * 编辑器顶部下拉框的全量选项（纯前端常量，不加载任何语法包）。
 *
 * 顺序按文档矩阵分组排列，方便用户按类别找。
 */
export const LANGUAGES: LanguageOption[] = [
  // 语言名一律用技术原名（JSON / Rust / Dockerfile…），不进 i18n：
  // 它们是专有名词，翻译反而更容易认错（设计文档 1.4 第 3 条指界面文案，不含语言名）
  { key: 'plain', label: 'Plain Text' },
  { key: 'json', label: 'JSON' },
  { key: 'yaml', label: 'YAML' },
  { key: 'xml', label: 'XML' },
  { key: 'toml', label: 'TOML' },
  { key: 'csv', label: 'CSV' },
  { key: 'properties', label: 'Properties / INI' },
  { key: 'html', label: 'HTML' },
  { key: 'css', label: 'CSS' },
  { key: 'scss', label: 'SCSS / Sass / Less' },
  { key: 'javascript', label: 'JavaScript' },
  { key: 'typescript', label: 'TypeScript' },
  { key: 'jsx', label: 'JSX' },
  { key: 'tsx', label: 'TSX' },
  { key: 'vue', label: 'Vue' },
  { key: 'svelte', label: 'Svelte' },
  { key: 'rust', label: 'Rust' },
  { key: 'python', label: 'Python' },
  { key: 'go', label: 'Go' },
  { key: 'java', label: 'Java' },
  { key: 'cpp', label: 'C / C++' },
  { key: 'csharp', label: 'C#' },
  { key: 'php', label: 'PHP' },
  { key: 'ruby', label: 'Ruby' },
  { key: 'lua', label: 'Lua' },
  { key: 'shell', label: 'Shell (Bash/Zsh)' },
  { key: 'powershell', label: 'PowerShell' },
  { key: 'nginx', label: 'Nginx' },
  { key: 'dockerfile', label: 'Dockerfile' },
  { key: 'makefile', label: 'Makefile' },
  { key: 'sql', label: 'SQL' },
  { key: 'markdown', label: 'Markdown' },
  { key: 'assembly', label: 'Assembly' },
  { key: 'protobuf', label: 'ProtoBuf' },
]

/** 扩展名 → 语言（key 一律小写、不带点） */
const BY_EXT: Record<string, LanguageKey> = {
  // 结构化数据
  json: 'json',
  jsonc: 'json',
  json5: 'json',
  yaml: 'yaml',
  yml: 'yaml',
  xml: 'xml',
  svg: 'xml',
  plist: 'xml',
  xsd: 'xml',
  toml: 'toml',
  lock: 'toml',
  csv: 'csv',
  tsv: 'csv',
  ini: 'properties',
  cfg: 'properties',
  env: 'properties',
  properties: 'properties',
  // 前端
  html: 'html',
  htm: 'html',
  xhtml: 'html',
  css: 'css',
  scss: 'scss',
  sass: 'scss',
  less: 'scss',
  styl: 'scss',
  js: 'javascript',
  mjs: 'javascript',
  cjs: 'javascript',
  ts: 'typescript',
  mts: 'typescript',
  cts: 'typescript',
  jsx: 'jsx',
  tsx: 'tsx',
  vue: 'vue',
  svelte: 'svelte',
  // 后端
  rs: 'rust',
  py: 'python',
  pyi: 'python',
  pyw: 'python',
  go: 'go',
  java: 'java',
  c: 'cpp',
  h: 'cpp',
  cc: 'cpp',
  cpp: 'cpp',
  cxx: 'cpp',
  hpp: 'cpp',
  hxx: 'cpp',
  cs: 'csharp',
  php: 'php',
  rb: 'ruby',
  lua: 'lua',
  // 脚本与配置
  sh: 'shell',
  bash: 'shell',
  zsh: 'shell',
  ksh: 'shell',
  fish: 'shell',
  ps1: 'powershell',
  psm1: 'powershell',
  psd1: 'powershell',
  conf: 'nginx',
  sql: 'sql',
  md: 'markdown',
  markdown: 'markdown',
  mdx: 'markdown',
  asm: 'assembly',
  s: 'assembly',
  proto: 'protobuf',
  // 构建
  mk: 'makefile',
  dockerfile: 'dockerfile',
}

/** 无扩展名 / 特殊文件名 → 语言（文档要求用文件名正则兜底） */
const BY_NAME: { pattern: RegExp; key: LanguageKey }[] = [
  { pattern: /^dockerfile(\.[\w.-]+)?$/i, key: 'dockerfile' },
  { pattern: /^(gnu)?makefile$/i, key: 'makefile' },
  { pattern: /^makefile\.(am|in)$/i, key: 'makefile' },
  // `.env` / `.env.local`：KEY=VALUE，按 properties 高亮
  { pattern: /^\.env(\.[\w-]+)?$/i, key: 'properties' },
  { pattern: /\.nginx\.conf$/i, key: 'nginx' },
  { pattern: /^nginx\.conf$/i, key: 'nginx' },
  { pattern: /^cargo\.lock$/i, key: 'toml' },
]

/** 依据文件名判定语言；未命中返回 plain（不强加高亮）。 */
export function detectLanguage(fileName: string): LanguageKey {
  const lower = fileName.toLowerCase()
  // 特殊文件名优先：Dockerfile / Makefile 这类没有「扩展名」但语义明确
  for (const rule of BY_NAME) {
    if (rule.pattern.test(lower)) return rule.key
  }
  const ext = lower.includes('.') ? lower.slice(lower.lastIndexOf('.') + 1) : ''
  if (ext && BY_EXT[ext]) return BY_EXT[ext]
  return 'plain'
}

/** 语言键 → 下拉框展示名（找不到时回退原始 key） */
export function languageLabel(key: LanguageKey): string {
  return LANGUAGES.find((item) => item.key === key)?.label ?? key
}
