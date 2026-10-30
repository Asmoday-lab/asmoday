<script setup lang="ts">
/**
 * CodeMirror 6 宿主组件。
 *
 * 本组件由 FilesView 通过 defineAsyncComponent + import() 按需加载，
 * CodeMirror 内核与各语言包因此都落在独立 chunk 里（设计文档 2.3）。
 * 语言包本身也在切换语言时才 import()。
 */
import { onBeforeUnmount, onMounted, ref, watch } from 'vue'
import { Compartment, EditorState, type Extension } from '@codemirror/state'
import {
  EditorView,
  drawSelection,
  dropCursor,
  highlightActiveLine,
  highlightActiveLineGutter,
  highlightSpecialChars,
  keymap,
  lineNumbers,
  rectangularSelection,
} from '@codemirror/view'
import { defaultKeymap, history, historyKeymap, indentWithTab } from '@codemirror/commands'
import {
  StreamLanguage,
  bracketMatching,
  defaultHighlightStyle,
  indentOnInput,
  syntaxHighlighting,
  type StreamParser,
} from '@codemirror/language'
import { oneDark } from '@codemirror/theme-one-dark'
import type { LanguageKey } from '../api/languages'

const props = defineProps<{
  content: string
  language: LanguageKey
  dark: boolean
  /** 正文字号（px）。整屏编辑器让用户自己调，小屏默认 13、桌面默认 14 */
  fontSize?: number
}>()

const emit = defineEmits<{
  (e: 'change', value: string): void
  (e: 'save'): void
}>()

const host = ref<HTMLElement | null>(null)

let view: EditorView | null = null
let syncing = false
const languageSlot = new Compartment()
const themeSlot = new Compartment()
const fontSlot = new Compartment()

/**
 * CSV / TSV 没有官方语法包：给一个极简 tokenizer 就够用（引号串 + 单元格）。
 * 同样走 `StreamLanguage` 通道，切换语言时才创建，不占首屏。
 */
const csvMode: StreamParser<null> = {
  name: 'csv',
  token(stream) {
    if (stream.match(/^"(?:[^"]|"")*"/)) return 'string'
    if (stream.match(/^[^,\t\r\n]+/)) return 'atom'
    stream.next()
    return null
  },
}

/** Makefile 同样没有官方包：只标目标、注释与变量，够读就行 */
const makefileMode: StreamParser<null> = {
  name: 'makefile',
  token(stream) {
    if (stream.sol() && stream.match(/^[^\s#:][^:=]*:(?!=)/)) return 'variableName'
    if (stream.match(/^#[^\n]*/)) return 'comment'
    if (stream.match(/^\$[({][^)}]+[)}]/)) return 'variableName.special'
    stream.next()
    return null
  },
}

/**
 * 各语言的动态加载器（设计文档 3.2.2.2）：只有真正切到该语言时才会下载对应 chunk。
 *
 * 用 `Record<LanguageKey, …>` 而不是 `Record<string, …>` 是刻意的：新增语言键却
 * 忘了写加载器，TypeScript 当场报错，不会等用户点开才发现「没高亮」。
 */
const languageLoaders: Record<LanguageKey, () => Promise<Extension>> = {
  plain: async () => [],
  json: async () => (await import('@codemirror/lang-json')).json(),
  yaml: async () => (await import('@codemirror/lang-yaml')).yaml(),
  xml: async () => (await import('@codemirror/lang-xml')).xml(),
  toml: async () => {
    const mode = await import('@codemirror/legacy-modes/mode/toml')
    return StreamLanguage.define(mode.toml)
  },
  csv: async () => StreamLanguage.define(csvMode),
  properties: async () => {
    const mode = await import('@codemirror/legacy-modes/mode/properties')
    return StreamLanguage.define(mode.properties)
  },
  html: async () => (await import('@codemirror/lang-html')).html(),
  css: async () => (await import('@codemirror/lang-css')).css(),
  scss: async () => {
    const mode = await import('@codemirror/legacy-modes/mode/sass')
    return StreamLanguage.define(mode.sass)
  },
  javascript: async () => (await import('@codemirror/lang-javascript')).javascript(),
  typescript: async () =>
    (await import('@codemirror/lang-javascript')).javascript({ typescript: true }),
  jsx: async () => (await import('@codemirror/lang-javascript')).javascript({ jsx: true }),
  tsx: async () =>
    (await import('@codemirror/lang-javascript')).javascript({ typescript: true, jsx: true }),
  vue: async () => (await import('@codemirror/lang-vue')).vue(),
  // Svelte 没有官方语法包：单文件组件本质是 HTML + 表达式，用 HTML 模式兜底
  svelte: async () => (await import('@codemirror/lang-html')).html(),
  rust: async () => (await import('@codemirror/lang-rust')).rust(),
  python: async () => (await import('@codemirror/lang-python')).python(),
  go: async () => (await import('@codemirror/lang-go')).go(),
  java: async () => (await import('@codemirror/lang-java')).java(),
  cpp: async () => (await import('@codemirror/lang-cpp')).cpp(),
  csharp: async () => {
    const mode = await import('@codemirror/legacy-modes/mode/clike')
    return StreamLanguage.define(mode.csharp)
  },
  php: async () => (await import('@codemirror/lang-php')).php(),
  ruby: async () => {
    const mode = await import('@codemirror/legacy-modes/mode/ruby')
    return StreamLanguage.define(mode.ruby)
  },
  lua: async () => {
    const mode = await import('@codemirror/legacy-modes/mode/lua')
    return StreamLanguage.define(mode.lua)
  },
  shell: async () => {
    const mode = await import('@codemirror/legacy-modes/mode/shell')
    return StreamLanguage.define(mode.shell)
  },
  powershell: async () => {
    const mode = await import('@codemirror/legacy-modes/mode/powershell')
    return StreamLanguage.define(mode.powerShell)
  },
  nginx: async () => {
    const mode = await import('@codemirror/legacy-modes/mode/nginx')
    return StreamLanguage.define(mode.nginx)
  },
  dockerfile: async () => {
    const mode = await import('@codemirror/legacy-modes/mode/dockerfile')
    return StreamLanguage.define(mode.dockerFile)
  },
  makefile: async () => StreamLanguage.define(makefileMode),
  sql: async () => (await import('@codemirror/lang-sql')).sql(),
  markdown: async () => (await import('@codemirror/lang-markdown')).markdown(),
  assembly: async () => {
    const mode = await import('@codemirror/legacy-modes/mode/gas')
    return StreamLanguage.define(mode.gas)
  },
  protobuf: async () => {
    const mode = await import('@codemirror/legacy-modes/mode/protobuf')
    return StreamLanguage.define(mode.protobuf)
  },
}

async function resolveLanguage(key: LanguageKey): Promise<Extension> {
  const loader = languageLoaders[key]
  if (!loader) return []
  try {
    return await loader()
  } catch (err) {
    console.warn('[asmoday] 语言包加载失败，回退纯文本', key, err)
    return []
  }
}

function currentFontSize(): number {
  const size = props.fontSize ?? 14
  return Number.isFinite(size) ? Math.min(24, Math.max(10, Math.round(size))) : 14
}

/**
 * 字号单独成段：顶栏的 A−/A+ 只重配这个 compartment，
 * 编辑器实例本身不重建（撤销栈、滚动位置、光标都保住）。
 */
function fontTheme(): Extension {
  const size = currentFontSize()
  return EditorView.theme({
    '&': { height: '100%', fontSize: `${size}px` },
    '.cm-gutters': { fontSize: `${Math.max(10, size - 1)}px` },
  })
}

function baseExtensions(): Extension[] {
  return [
    lineNumbers(),
    highlightActiveLineGutter(),
    highlightSpecialChars(),
    history(),
    drawSelection(),
    dropCursor(),
    indentOnInput(),
    bracketMatching(),
    rectangularSelection(),
    highlightActiveLine(),
    syntaxHighlighting(defaultHighlightStyle, { fallback: true }),
    keymap.of([
      {
        key: 'Mod-s',
        preventDefault: true,
        run: () => {
          emit('save')
          return true
        },
      },
      ...defaultKeymap,
      ...historyKeymap,
      indentWithTab,
    ]),
    EditorView.updateListener.of((update) => {
      if (!update.docChanged || syncing) return
      emit('change', update.state.doc.toString())
    }),
    EditorView.theme({
      // 字号见 fontTheme()：源码模式 14px 起步（13px 在 1080p 上偏小，用户反馈看不清）
      '.cm-scroller': { fontFamily: 'var(--font-app-mono)', lineHeight: '1.7' },
      '.cm-content': { padding: '14px 0' },
    }),
  ]
}

async function mountEditor() {
  if (!host.value) return
  const language = await resolveLanguage(props.language)

  view = new EditorView({
    parent: host.value,
    state: EditorState.create({
      doc: props.content,
      extensions: [
        ...baseExtensions(),
        languageSlot.of(language),
        themeSlot.of(props.dark ? oneDark : []),
        fontSlot.of(fontTheme()),
      ],
    }),
  })
  view.focus()
}

onMounted(mountEditor)

onBeforeUnmount(() => {
  view?.destroy()
  view = null
})

// 外部内容变化（例如重新载入文件）时整体替换
watch(
  () => props.content,
  (next) => {
    if (!view || view.state.doc.toString() === next) return
    syncing = true
    view.dispatch({
      changes: { from: 0, to: view.state.doc.length, insert: next },
    })
    syncing = false
  },
)

// 切换语言
watch(
  () => props.language,
  async (next) => {
    if (!view) return
    const extension = await resolveLanguage(next)
    view.dispatch({ effects: languageSlot.reconfigure(extension) })
  },
)

// 跟随全局深浅色
watch(
  () => props.dark,
  (next) => {
    view?.dispatch({ effects: themeSlot.reconfigure(next ? oneDark : []) })
  },
)

// 顶栏改字号：只重配字号段，不重建实例
watch(
  () => props.fontSize,
  () => {
    view?.dispatch({ effects: fontSlot.reconfigure(fontTheme()) })
  },
)

defineExpose({
  focus: () => view?.focus(),
})
</script>

<template>
  <div ref="host" class="h-full w-full overflow-hidden bg-panel text-left" />
</template>
