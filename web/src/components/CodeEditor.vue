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
} from '@codemirror/language'
import { oneDark } from '@codemirror/theme-one-dark'
import type { LanguageKey } from '../api/languages'

const props = defineProps<{
  content: string
  language: LanguageKey
  dark: boolean
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

/** 各语言的动态加载器：只有真正切到该语言时才会下载对应 chunk */
const languageLoaders: Record<string, () => Promise<Extension>> = {
  js: async () => (await import('@codemirror/lang-javascript')).javascript(),
  jsx: async () => (await import('@codemirror/lang-javascript')).javascript({ jsx: true }),
  ts: async () => (await import('@codemirror/lang-javascript')).javascript({ typescript: true }),
  tsx: async () =>
    (await import('@codemirror/lang-javascript')).javascript({ typescript: true, jsx: true }),
  rust: async () => (await import('@codemirror/lang-rust')).rust(),
  python: async () => (await import('@codemirror/lang-python')).python(),
  toml: async () => {
    const mode = await import('@codemirror/legacy-modes/mode/toml')
    return StreamLanguage.define(mode.toml)
  },
  nginx: async () => {
    const mode = await import('@codemirror/legacy-modes/mode/nginx')
    return StreamLanguage.define(mode.nginx)
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
      // 源码模式字号 14px（13px 在 1080p 上偏小，用户反馈看不清）
      '&': { height: '100%', fontSize: '14px' },
      '.cm-scroller': { fontFamily: 'var(--font-app-mono)', lineHeight: '1.7' },
      '.cm-content': { padding: '14px 0' },
      '.cm-gutters': { fontSize: '13px' },
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

defineExpose({
  focus: () => view?.focus(),
})
</script>

<template>
  <div ref="host" class="h-full w-full overflow-hidden bg-panel text-left" />
</template>
