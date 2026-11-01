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
 */
import { ref, useAttrs } from 'vue'
import { useI18n } from 'vue-i18n'
import AppIcon from './AppIcon.vue'

defineOptions({ inheritAttrs: false })

defineProps<{ modelValue: string }>()
const emit = defineEmits<{ 'update:modelValue': [string] }>()

const attrs = useAttrs()
const { t } = useI18n()
const reveal = ref(false)

function onInput(event: Event) {
  emit('update:modelValue', (event.target as HTMLInputElement).value)
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
    />
    <button
      type="button"
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
