<script setup lang="ts">
/**
 * 密码输入框 + 常驻「显示 / 隐藏」按钮。
 *
 * 为什么不用浏览器自带那个眼睛：Chromium 的 reveal 只在输入框**聚焦且非空**时
 * 才冒出来，失焦就消失（这就是「小眼睛有时不出现」），右侧位置还会被 input 的
 * padding-right 与圆角裁掉一截。这里关掉原生控件（style.css 的 `::-ms-reveal`），
 * 自建一个始终可见、始终可点的按钮。
 *
 * 细节：
 * - 按钮必须 `type="button"`，否则放在 <form> 里点一下就把表单提交了；
 * - `@pointerdown.prevent` 让 input 保持焦点：桌面上不闪、移动端小键盘不收起；
 * - 除 class / 监听器外，placeholder、autocomplete、autofocus 等原样透传给 input。
 * - **只留可见 ASCII**（v4.9.51，可用 `:ascii-only="false"` 关掉）：密码里不该出现空格、
 *   中文或全角符号——屏幕上看一样，码点却不同，粘贴/输入法一掺进来就是「密码对却 401」。
 *   输入与粘贴时直接把非法字符滤掉（后端 `core::auth::check_password_charset` 同一口径），
 *   连输入法组合中的候选也等 `compositionend` 后再清，避免中途裁剪打断输入。
 * - **失焦 / 回车时抹掉首尾空白**（v4.9.50，可用 `:trim="false"` 关掉）：密码里最常
 *   出的事故是从密码管理器、聊天记录里粘贴时多带一个空格，以及中文输入法留下的全角
 *   空格（`String.trim()` 会一并吃掉 `U+3000`）。这类字符看不见，却让人一直 401。
 *   不在 `input` 事件里 trim：那样光标会乱跳（空格本身已由字符集过滤拦掉）。
 */
import { ref, useAttrs } from 'vue'
import { useI18n } from 'vue-i18n'
import AppIcon from './AppIcon.vue'
import { filterPasswordChars } from '../lib/password'

defineOptions({ inheritAttrs: false })

const props = withDefaults(
  defineProps<{ modelValue: string; trim?: boolean; asciiOnly?: boolean }>(),
  { trim: true, asciiOnly: true },
)
const emit = defineEmits<{ 'update:modelValue': [string] }>()

const attrs = useAttrs()
const { t } = useI18n()
const reveal = ref(false)

/** 滤掉非法字符（字符集收口在 `filterPasswordChars`）；确实删了东西就把 DOM 也改掉，
 *  否则界面上还留着「已删掉」的字符 */
function filterValue(value: string, el?: HTMLInputElement): string {
  if (!props.asciiOnly) return value
  const cleaned = filterPasswordChars(value)
  if (el && cleaned !== value) el.value = cleaned
  return cleaned
}

function onInput(event: Event) {
  const el = event.target as HTMLInputElement
  // 输入法组合中不裁剪：会把候选打断。中文本来就不在允许集里，等落定再统一清。
  if ((event as InputEvent).isComposing) {
    emit('update:modelValue', el.value)
    return
  }
  emit('update:modelValue', filterValue(el.value, el))
}

function onCompositionEnd(event: Event) {
  const el = event.target as HTMLInputElement
  emit('update:modelValue', filterValue(el.value, el))
}

/** 失焦与回车时收口：这两个时机改值不会打断输入，也不会挪光标 */
function trimValue() {
  if (!props.trim) return
  const trimmed = props.modelValue.trim()
  if (trimmed !== props.modelValue) emit('update:modelValue', trimmed)
}
</script>

<template>
  <div class="pw-field relative">
    <input
      v-bind="attrs"
      :value="modelValue"
      :type="reveal ? 'text' : 'password'"
      class="w-full"
      @input="onInput"
      @compositionend="onCompositionEnd"
      @blur="trimValue"
      @keydown.enter="trimValue"
    />
    <!-- tabindex=-1：不参与 Tab 焦点链，Tab 应在输入框之间直接跳转 -->
    <button
      type="button"
      tabindex="-1"
      class="absolute inset-y-0 right-0 grid w-10 place-items-center rounded-r-lg text-muted transition-colors hover:text-ink focus-visible:text-ink focus-visible:outline-none"
      :aria-label="reveal ? t('common.password.hide') : t('common.password.show')"
      :aria-pressed="reveal"
      :title="reveal ? t('common.password.hide') : t('common.password.show')"
      :data-revealed="reveal ? '1' : '0'"
      @pointerdown.prevent
      @click="reveal = !reveal"
    >
      <AppIcon :name="reveal ? 'eyeOff' : 'eye'" :size="15" />
    </button>
  </div>
</template>
