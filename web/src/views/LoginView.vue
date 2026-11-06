<script setup lang="ts">
/**
 * 登录页。整页独立布局，不套侧边栏。
 *
 * 单用户后没有用户名可言（v4.9.32）：整页只有一个输入框。
 * 开了 2FA 就只认动态码（服务端 `status` 未登录也回 `totp_enabled`），
 * 没开就只有密码框——不再让用户自己点「已开启 2FA」把第二个框叫出来（v4.9.46）。
 * 首次部署用控制台打印的一次性密码登录，登录后 App.vue 会弹出
 * 「设置新密码」对话框完成建权——本页不再承担初始化形态（v4.9.34）。
 */
import { computed, onMounted, ref } from 'vue'
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

const password = ref('')
const code = ref('')
const error = ref('')
const submitting = ref(false)

/** 2FA 开着时整页只有动态码框；6 位数字走 TOTP，其余当恢复码（与后端同一判据） */
const totpMode = computed(() => authApi.status.value.totp_enabled)

async function submit() {
  if (submitting.value) return
  error.value = ''
  if (totpMode.value) {
    if (!code.value.trim()) {
      error.value = t('auth.login.totpRequired')
      return
    }
  } else if (!password.value) {
    error.value = t('auth.login.required')
    return
  }
  submitting.value = true
  try {
    await authApi.login(totpMode.value ? '' : password.value, totpMode.value ? code.value.trim() : '')
    const target = typeof route.query.redirect === 'string' ? route.query.redirect : '/'
    await router.replace(target)
  } catch (err) {
    error.value = err instanceof Error ? err.message : t('auth.login.failed')
  } finally {
    submitting.value = false
  }
}

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
        <label v-if="totpMode" class="block">
          <span class="mb-1 block text-[12px] text-muted">{{ t('auth.login.totpLabel') }}</span>
          <input
            v-model="code"
            type="text"
            autocomplete="one-time-code"
            autocapitalize="characters"
            spellcheck="false"
            class="w-full rounded-lg border border-line bg-elevated px-3 py-2 text-center text-base font-medium tracking-[0.4em] text-ink outline-none focus:border-accent"
            placeholder="000000"
            @input="error = ''"
          />
          <span class="mt-1.5 block text-[11px] leading-relaxed text-muted">
            {{ t('auth.login.totpHint') }}
          </span>
        </label>
        <label v-else class="block">
          <span class="mb-1 block text-[12px] text-muted">{{ t('auth.login.password') }}</span>
          <PasswordInput
            v-model="password"
            autocomplete="current-password"
            class="w-full rounded-lg border border-line bg-elevated px-3 py-2 text-sm text-ink outline-none focus:border-accent"
            placeholder="••••••••"
          />
        </label>

        <p
          v-if="error"
          class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-xs fg-danger"
        >
          {{ error }}
        </p>

        <button
          type="submit"
          :disabled="submitting"
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
