<script setup lang="ts">
/**
 * 全局布局：PC 固定 240px 侧边栏 + 顶栏；移动端侧边栏收为抽屉。
 * 登录页（meta.public）走独立整页布局。
 * 另挂：全局音乐播放条（跨页面常驻）与一次性密码强制改密弹窗。
 */
import { computed, reactive, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import { RouterLink, RouterView, useRoute, useRouter } from 'vue-router'
import AppIcon from './components/AppIcon.vue'
import AppSidebar from './components/AppSidebar.vue'
import MediaLightbox from './components/MediaLightbox.vue'
import MusicBar from './components/MusicBar.vue'
import ShareDialog from './components/ShareDialog.vue'
import { currentTrack, player } from './stores/player'
import { authApi } from './api/auth'
import { useTheme, type ThemeMode } from './composables/useTheme'
import { useBackClose } from './composables/useBackClose'

const route = useRoute()
const router = useRouter()
const { t, locale } = useI18n()

// 主题切换入口统一收在右上角账号菜单里（设计文档 8.2：侧边栏底部不得留孤立图标）
const { mode: themeMode, setTheme } = useTheme()
const themeOptions: { key: ThemeMode; label: string }[] = [
  { key: 'light', label: 'common.theme.light' },
  { key: 'dark', label: 'common.theme.dark' },
  { key: 'system', label: 'common.theme.system' },
]

function pickTheme(next: ThemeMode) {
  setTheme(next)
  accountOpen.value = false
}

const drawerOpen = ref(false)
const accountOpen = ref(false)

// 移动端抽屉与账号菜单也是一层历史：按返回先收起它，而不是直接退出当前页面
useBackClose(computed(() => drawerOpen.value || accountOpen.value), () => {
  drawerOpen.value = false
  accountOpen.value = false
})

const isPublic = computed(() => route.meta.public === true)
const username = authApi.username
const mustChangePassword = computed(
  () => authApi.isAuthenticated.value && authApi.status.value.must_change_password === true,
)

// 强制改密表单
const passwordForm = reactive({ current: '', next: '', confirm: '' })
const passwordBusy = ref(false)
const passwordError = ref('')

async function submitPasswordChange() {
  if (passwordBusy.value) return
  passwordError.value = ''
  if (passwordForm.next !== passwordForm.confirm) {
    passwordError.value = t('auth.forceChange.mismatch')
    return
  }
  if (passwordForm.next.length < 8) {
    passwordError.value = t('auth.forceChange.tooShort')
    return
  }
  passwordBusy.value = true
  try {
    await authApi.changePassword(passwordForm.current, passwordForm.next)
    passwordForm.current = passwordForm.next = passwordForm.confirm = ''
  } catch (err) {
    passwordError.value = err instanceof Error ? err.message : t('auth.forceChange.failed')
  } finally {
    passwordBusy.value = false
  }
}

// 路由切换时收起抽屉与账号菜单
watch(
  () => route.fullPath,
  () => {
    drawerOpen.value = false
    accountOpen.value = false
  },
)

// 路由或语言变化时同步浏览器标题（设计文档 3.8：切换语言无需刷新）
watch(
  [() => route.meta.title, locale],
  () => {
    const key = String(route.meta.title ?? '')
    document.title = key ? `${t(key)} · Asmoday` : 'Asmoday'
  },
  { immediate: true },
)

async function logout() {
  accountOpen.value = false
  try {
    await authApi.logout()
  } finally {
    await router.replace('/login')
  }
}
</script>

<template>
  <RouterView v-if="isPublic" />

  <div v-else class="flex h-dvh w-full overflow-hidden bg-canvas text-ink">
    <!-- 移动端抽屉遮罩 -->
    <Transition name="fade">
      <div
        v-if="drawerOpen"
        class="fixed inset-0 z-40 bg-black/50 backdrop-blur-sm md:hidden"
        @click="drawerOpen = false"
      />
    </Transition>

    <AppSidebar :open="drawerOpen" @close="drawerOpen = false" />

    <main class="flex min-w-0 flex-1 flex-col">
      <!-- 顶栏让出刘海/灵动岛：viewport-fit=cover 之后 env() 才有值（设计文档 8.6） -->
      <header
        class="relative flex min-h-14 shrink-0 items-center gap-3 border-b border-line bg-panel px-4 md:px-6"
        style="padding-top: env(safe-area-inset-top, 0px)"
      >
        <button
          class="-ml-1 grid size-9 place-items-center rounded-lg text-muted transition-colors hover:bg-elevated hover:text-ink md:hidden"
          :aria-label="t('common.sidebar.openMenu')"
          @click="drawerOpen = true"
        >
          <AppIcon name="menu" :size="19" />
        </button>

        <div class="min-w-0 flex-1">
          <div class="hidden items-center gap-1.5 text-sm text-muted md:flex">
            <RouterLink to="/" class="cursor-pointer transition-colors hover:text-ink">Home</RouterLink>
            <AppIcon name="chevronRight" :size="10" />
            <span class="capitalize text-ink">{{ t(String(route.meta.title)) }}</span>
          </div>
          <p class="truncate text-base font-bold capitalize text-ink md:hidden">
            {{ t(String(route.meta.title)) }}
          </p>
        </div>

        <div class="relative">
          <button
            class="grid size-8 place-items-center rounded-full bg-accent text-xs font-semibold text-white"
            :title="username ?? t('common.account.label')"
            @click="accountOpen = !accountOpen"
          >
            {{ (username ?? 'A').slice(0, 1).toUpperCase() }}
          </button>

          <div
            v-if="accountOpen"
            class="absolute right-0 z-50 mt-2 w-48 overflow-hidden rounded-lg border border-line bg-panel py-1 shadow-xl"
            data-testid="account-menu"
          >
            <p class="truncate px-3 py-2 text-[12px] text-muted">
              {{ username ?? t('common.account.notLoggedIn') }}
            </p>

            <!-- 主题三态（设计文档 8.2：浅色 / 深色 / 跟随系统，当前项打勾） -->
            <div class="my-1 border-t border-line" />
            <p class="px-3 pt-1 pb-1 text-[11px] font-medium text-muted">
              {{ t('common.theme.switch') }}
            </p>
            <button
              v-for="option in themeOptions"
              :key="option.key"
              class="flex w-full items-center gap-2 px-3 py-2 text-xs text-ink transition-colors hover:bg-elevated"
              :data-testid="`theme-${option.key}`"
              @click="pickTheme(option.key)"
            >
              <AppIcon
                :name="option.key === 'system' ? 'chip' : option.key === 'dark' ? 'moon' : 'sun'"
                :size="13"
              />
              <span class="flex-1 text-left">{{ t(option.label) }}</span>
              <AppIcon
                v-if="themeMode === option.key"
                name="check"
                :size="12"
                class="text-accent"
              />
            </button>

            <div class="my-1 border-t border-line" />
            <button
              class="flex w-full items-center gap-2 px-3 py-2 text-xs text-ink transition-colors hover:bg-elevated"
              @click="logout"
            >
              <AppIcon name="lock" :size="13" />
              {{ t('common.account.logout') }}
            </button>
          </div>
        </div>
      </header>

      <div
        id="app-scroll"
        class="min-h-0 flex-1 overflow-y-auto"
        :class="
          currentTrack
            ? player.state.barStyle === 'minimal'
              ? 'pb-24'
              : 'pb-[calc(5rem+env(safe-area-inset-bottom,0px))] lg:pb-[132px]'
            : ''
        "
      >
        <!-- 一次性密码会话：后端只放行改密接口，这里也别把主界面挂起来，
             否则底层页面会连着发一堆 403 请求，弹一堆错误 toast -->
        <RouterView v-if="!mustChangePassword" v-slot="{ Component }">
          <Transition name="route-fade" mode="out-in">
            <component :is="Component" />
          </Transition>
        </RouterView>
      </div>
    </main>

    <!-- 全局音乐播放条（跨页面常驻） -->
    <MusicBar />

    <!-- 全局媒体灯箱（单实例，Teleport 到 body；文件管理与相册共用） -->
    <MediaLightbox />

    <!-- 全局分享弹窗（单实例；文件右键与各媒体库共用） -->
    <ShareDialog />

    <!-- 一次性密码强制改密（--init / --reset 后首登） -->
    <div v-if="mustChangePassword" class="fixed inset-0 z-[60] grid place-items-center modal-overlay bg-black/60 p-4">
      <div class="w-full max-w-sm modal-card rounded-2xl border border-line bg-panel p-6 shadow-xl">
        <span class="mb-3 grid size-10 place-items-center rounded-full bg-amber-500/15 fg-warn">
          <AppIcon name="lock" :size="18" />
        </span>
        <p class="text-sm font-medium text-ink">{{ t('auth.forceChange.title') }}</p>
        <p class="mt-1 text-xs text-muted">{{ t('auth.forceChange.desc') }}</p>

        <div class="mt-4 space-y-2">
          <input
            v-model="passwordForm.current"
            type="password"
            :placeholder="t('auth.forceChange.current')"
            autocomplete="current-password"
            class="w-full rounded-lg border border-line bg-elevated px-3 py-2 text-xs text-ink outline-none focus:border-accent"
          />
          <input
            v-model="passwordForm.next"
            type="password"
            :placeholder="t('auth.forceChange.next')"
            autocomplete="new-password"
            class="w-full rounded-lg border border-line bg-elevated px-3 py-2 text-xs text-ink outline-none focus:border-accent"
          />
          <input
            v-model="passwordForm.confirm"
            type="password"
            :placeholder="t('auth.forceChange.confirm')"
            autocomplete="new-password"
            class="w-full rounded-lg border border-line bg-elevated px-3 py-2 text-xs text-ink outline-none focus:border-accent"
          />
        </div>

        <p v-if="passwordError" class="mt-2 text-xs fg-danger">{{ passwordError }}</p>

        <button
          class="mt-4 w-full rounded-lg bg-accent py-2 text-xs font-medium text-white transition-opacity hover:opacity-90 disabled:opacity-50"
          :disabled="passwordBusy"
          @click="submitPasswordChange"
        >
          {{ passwordBusy ? t('auth.forceChange.submitting') : t('auth.forceChange.submit') }}
        </button>
      </div>
    </div>
  </div>
</template>
