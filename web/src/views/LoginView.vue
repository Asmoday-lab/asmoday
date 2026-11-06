<script setup lang="ts">
/**
 * 登录页。整页独立布局，不套侧边栏。
 *
 * 单用户后没有用户名（v4.9.32）：整页只有**一个输入框**，形态跟着登录方式走——
 * 默认是密码框；账号开了 2FA 时框下方多一行「使用动态验证码登录」，点一下同一个
 * 框变成 6 位动态码输入（居中、字距拉开、输满自动提交），再点一行就换回密码框
 * （v4.9.48）。这是 GitHub / Vercel 这类站点的通行做法：方式可选，框始终一个。
 *
 * 首次部署用控制台打印的一次性密码登录，登录后 App.vue 会弹出
 * 「设置新密码」对话框完成建权——本页不再承担初始化形态（v4.9.34）。
 */
import { computed, nextTick, onMounted, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import { useRoute, useRouter } from 'vue-router'
import AppIcon from '../components/AppIcon.vue'
import PasswordInput from '../components/PasswordInput.vue'
import { authApi } from '../api/auth'
import { useTheme } from '../composables/useTheme'
import { SUPPORTED_LOCALES, type Locale } from '../locales'
import { applyLocale } from '../lib/i18n'

const { isDark, toggle } = useTheme()
const { t, locale } = useI18n()
const router = useRouter()
const route = useRoute()

type LoginMode = 'password' | 'code'

/** 同一个框的两种形态：默认密码，用户自己切到动态码 */
const mode = ref<LoginMode>('password')
const password = ref('')
const code = ref('')
const error = ref('')
const submitting = ref(false)
/** 已经自动提交过的 6 位码，避免失败后同一串反复打请求 */
let lastAutoSubmitted = ''

const passwordWrap = ref<HTMLElement | null>(null)
const codeInput = ref<HTMLInputElement | null>(null)

/** 账号开没开 2FA 只决定「要不要给切换入口」，框的个数永远是 1 */
const totpEnabled = computed(() => authApi.status.value.totp_enabled)
/** 没开 2FA 时恒为 false：即使 mode 被改过也不会露出动态码框 */
const codeMode = computed(() => totpEnabled.value && mode.value === 'code')

function toggleMode() {
  if (!totpEnabled.value || submitting.value) return
  mode.value = codeMode.value ? 'password' : 'code'
  // 两种凭证互不相干，切换时清干净，免得把密码当动态码提交出去
  password.value = ''
  code.value = ''
  error.value = ''
  lastAutoSubmitted = ''
  void nextTick(() => {
    if (codeMode.value) codeInput.value?.focus()
    else passwordWrap.value?.querySelector('input')?.focus()
  })
}

async function submit() {
  if (submitting.value) return
  error.value = ''
  const value = codeMode.value ? code.value.trim() : password.value
  if (!value) {
    error.value = t(codeMode.value ? 'auth.login.codeRequired' : 'auth.login.required')
    return
  }
  submitting.value = true
  try {
    // 密码模式只传 password，动态码模式只传 totp —— 后端按「哪个字段有值」分派
    if (codeMode.value) await authApi.login('', value)
    else await authApi.login(value)
    const target = typeof route.query.redirect === 'string' ? route.query.redirect : '/'
    await router.replace(target)
  } catch (err) {
    error.value = err instanceof Error ? err.message : t('auth.login.failed')
  } finally {
    submitting.value = false
  }
}

/** 6 位数字输满就提交（动态码框的通行手感）；恢复码是 10 位字母数字，不会误触 */
watch(code, (value) => {
  if (!codeMode.value || submitting.value) return
  const digits = value.replace(/\s+/g, '')
  if (!/^\d{6}$/.test(digits) || digits === lastAutoSubmitted) return
  lastAutoSubmitted = digits
  void submit()
})

onMounted(() => {
  void authApi.ensureAuth()
})

/** 语言切换：登录前也能改（首次运行可能整站是英文环境） */
function changeLocale(event: Event) {
  const value = (event.target as HTMLSelectElement).value as Locale
  applyLocale(value)
}
</script>

<template>
  <div class="relative flex min-h-dvh items-center justify-center bg-canvas px-4 py-10">
    <div
      class="pointer-events-none absolute top-0 left-1/2 size-80 -translate-x-1/2 rounded-full bg-accent/20 blur-[120px]"
    />

    <div class="absolute top-4 right-4 flex items-center gap-2">
      <select
        :value="locale"
        class="rounded-lg border border-line bg-elevated px-2 py-1.5 text-[12px] text-muted outline-none focus:border-accent"
        :aria-label="t('common.language.label')"
        @change="changeLocale"
      >
        <option v-for="item in SUPPORTED_LOCALES" :key="item" :value="item">
          {{ t(`common.language.${item}`) }}
        </option>
      </select>
      <button
        class="grid size-9 place-items-center rounded-lg border border-line text-muted transition-colors hover:text-ink"
        :title="isDark ? t('common.theme.toLight') : t('common.theme.toDark')"
        @click="toggle"
      >
        <AppIcon :name="isDark ? 'moon' : 'sun'" :size="15" />
      </button>
    </div>

    <div class="relative w-full max-w-sm">
      <div class="mb-6 flex flex-col items-center text-center">
        <span
          class="mb-3 grid size-12 place-items-center rounded-2xl bg-[linear-gradient(135deg,#818cf8,#4f46e5)] text-white shadow-md"
        >
          <AppIcon name="sparkle" :size="48" :stroke-width="1.75" />
        </span>
        <h1 class="text-xl font-semibold text-ink">Asmoday</h1>
        <p class="mt-1 text-xs text-muted">{{ t('auth.login.tagline') }}</p>
      </div>

      <form class="glass-card space-y-3 rounded-2xl p-5" @submit.prevent="submit">
        <!-- 密码形态 -->
        <label v-if="!codeMode" class="block">
          <span class="mb-1 block text-[12px] text-muted">{{ t('auth.login.password') }}</span>
          <div ref="passwordWrap">
            <PasswordInput
              v-model="password"
              autocomplete="current-password"
              class="w-full rounded-lg border border-line bg-elevated px-3 py-2 text-sm text-ink outline-none focus:border-accent"
              placeholder="••••••••"
            />
          </div>
        </label>
        <!-- 动态码形态：同一个框，换成居中等宽数字样式 -->
        <label v-else class="block">
          <span class="mb-1 block text-[12px] text-muted">{{ t('auth.login.codeLabel') }}</span>
          <input
            ref="codeInput"
            v-model="code"
            type="text"
            autocomplete="one-time-code"
            autocapitalize="characters"
            spellcheck="false"
            class="w-full rounded-lg border border-line bg-elevated px-3 py-2.5 text-center text-lg font-medium tracking-[0.45em] text-ink outline-none placeholder:text-muted/40 focus:border-accent"
            placeholder="000000"
            @input="error = ''"
          />
          <span
            class="mt-1.5 block text-[11px] leading-relaxed whitespace-pre-line text-muted"
          >
            {{ t('auth.login.codeHint') }}
          </span>
        </label>

        <!-- 方式切换：只有开了 2FA 才有第二条路，也才出现这一行 -->
        <button
          v-if="totpEnabled"
          type="button"
          class="mx-auto block text-[12px] text-muted underline-offset-4 transition-colors hover:text-ink hover:underline disabled:opacity-40"
          :disabled="submitting"
          data-testid="login-mode-toggle"
          @click="toggleMode"
        >
          {{ t(codeMode ? 'auth.login.usePassword' : 'auth.login.useTotp') }}
        </button>

        <p
          v-if="error"
          class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-xs fg-danger"
        >
          {{ error }}
        </p>

        <button
          type="submit"
          :disabled="submitting"
          data-testid="login-submit"
          class="w-full rounded-lg bg-accent py-2.5 text-sm font-medium text-white transition-opacity hover:opacity-90 disabled:opacity-40"
        >
          {{ submitting ? t('auth.login.submitting') : t('auth.login.submit') }}
        </button>

        <p class="pt-1 text-center text-[12px] leading-relaxed text-muted">
          {{ t('auth.login.footerLogin') }}
        </p>
      </form>
    </div>
  </div>
</template>
