<script setup lang="ts">
/**
 * 全局布局：PC 固定 240px 侧边栏 + 顶栏；移动端侧边栏收为抽屉。
 * 登录页（meta.public）走独立整页布局。
 * 另挂：全局音乐播放条（跨页面常驻）与一次性密码强制改密弹窗。
 */
import { computed, onScopeDispose, reactive, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import { RouterLink, RouterView, useRoute, useRouter } from 'vue-router'
import AppIcon from './components/AppIcon.vue'
import AppSidebar from './components/AppSidebar.vue'
import MediaLightbox from './components/MediaLightbox.vue'
import MusicBar from './components/MusicBar.vue'
import PasswordInput from './components/PasswordInput.vue'
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

/**
 * 账号菜单只在点按钮时开合，此前点页面任何别处都不会收起 —— 菜单盖在内容上，
 * 用户点「看起来是页面」的位置其实点在菜单边缘外的空白里，菜单还杵在那儿（v4.6.18）。
 * 这里补三条最朴素的失焦语义：点到菜单与按钮之外、按 Esc、窗口失焦（切标签页 /
 * 点到浏览器外），都当作用户已经离开菜单。
 *
 * 用捕获阶段的 pointerdown 而不是 click：按下即收起，不等手指抬起；
 * 判定放在按钮与菜单的公共父节点上，点菜单里的主题项不算「点到外面」。
 */
const accountWrap = ref<HTMLElement | null>(null)

function closeAccountOnOutside(event: PointerEvent) {
  if (!accountOpen.value) return
  const target = event.target as Node | null
  if (target && accountWrap.value?.contains(target)) return
  accountOpen.value = false
}

function closeAccountOnEsc(event: KeyboardEvent) {
  if (event.key === 'Escape' && accountOpen.value) accountOpen.value = false
}

function closeAccountOnBlur() {
  if (accountOpen.value) accountOpen.value = false
}

watch(accountOpen, (open) => {
  if (open) {
    document.addEventListener('pointerdown', closeAccountOnOutside, true)
    document.addEventListener('keydown', closeAccountOnEsc)
    window.addEventListener('blur', closeAccountOnBlur)
    return
  }
  document.removeEventListener('pointerdown', closeAccountOnOutside, true)
  document.removeEventListener('keydown', closeAccountOnEsc)
  window.removeEventListener('blur', closeAccountOnBlur)
})

onScopeDispose(() => {
  document.removeEventListener('pointerdown', closeAccountOnOutside, true)
  document.removeEventListener('keydown', closeAccountOnEsc)
  window.removeEventListener('blur', closeAccountOnBlur)
})

// 移动端抽屉与账号菜单也是一层历史：按返回先收起它，而不是直接退出当前页面
useBackClose(computed(() => drawerOpen.value || accountOpen.value), () => {
  drawerOpen.value = false
  accountOpen.value = false
})

const isPublic = computed(() => route.meta.public === true)
const mustChangePassword = computed(
  () => authApi.isAuthenticated.value && authApi.status.value.must_change_password === true,
)

// 强制改密表单（一次性密码会话：登录时已验过一次性密码，无需再填「当前密码」）
const passwordForm = reactive({ next: '', confirm: '' })
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
    await authApi.changePassword('', passwordForm.next)
    passwordForm.next = passwordForm.confirm = ''
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

/**
 * 满高应用面（终端 / 文件管理）：自带底部状态条，靠 `h-full` 吃满剩余高度。
 * 它们和普通文档页的区别在于「底部那条状态条必须贴住窗口底边」，
 * 见下面的 playerSpace。
 */
const fullHeightSurface = computed(() => route.meta.fullHeight === true)

/**
 * 播放条占位（v4.6.9）。
 *
 * 只有「铺满底部的经典条」在窄屏才真的需要内容让出高度 —— 它是 dock，不让位就压住内容。
 * lg 起经典条变成居中悬浮卡片、极简形态一直是右下角角标，两者都浮在内容之上：
 * 此时再按整条高度预留，终端/文件管理会被整块顶起来（底栏离开窗口底边），
 * 这就是用户反馈的「底部信息上浮」。所以满高应用面在 lg 断点一律不预留，
 * 普通文档页仍然保留 —— 否则滚动到末尾时最后一行会被悬浮卡片压住。
 */
const playerSpace = computed(() => {
  if (!currentTrack.value) return ''
  const docked = player.state.barStyle === 'classic'
  if (fullHeightSurface.value) {
    return docked ? 'pb-[calc(5rem+env(safe-area-inset-bottom,0px))] lg:pb-0' : 'pb-24 lg:pb-0'
  }
  return docked ? 'pb-[calc(5rem+env(safe-area-inset-bottom,0px))] lg:pb-[132px]' : 'pb-24'
})
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
          <!-- 自带路径条的页面（文件管理）不再挂「Home > 页面名」：两条 A > B 叠在一起时
               用户分不清该点哪个（设计文档 8.6），这里只留页面名当标题 -->
          <div class="hidden items-center gap-1.5 text-sm text-muted md:flex">
            <template v-if="!route.meta.hideCrumb">
              <RouterLink to="/" class="cursor-pointer transition-colors hover:text-ink">Home</RouterLink>
              <AppIcon name="chevronRight" :size="10" />
            </template>
            <span class="capitalize text-ink">{{ t(String(route.meta.title)) }}</span>
          </div>
          <p class="truncate text-base font-bold capitalize text-ink md:hidden">
            {{ t(String(route.meta.title)) }}
          </p>
        </div>

        <div ref="accountWrap" class="relative">
          <button
            class="grid size-8 place-items-center rounded-full bg-accent text-xs font-semibold text-white"
            :title="t('common.account.label')"
            @click="accountOpen = !accountOpen"
          >
            A
          </button>

          <div
            v-if="accountOpen"
            class="absolute right-0 z-50 mt-2 w-48 overflow-hidden rounded-lg border border-line bg-panel py-1 shadow-xl"
            data-testid="account-menu"
          >
            <p class="truncate px-3 py-2 text-[12px] text-muted">
              <!-- 单用户后没有可展示的用户名（v4.9.72）：固定品牌标识 -->
              Asmoday
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

      <div id="app-scroll" class="min-h-0 flex-1 overflow-y-auto" :class="playerSpace">
        <!-- 一次性密码会话：后端只放行改密接口，这里也别把主界面挂起来，
             否则底层页面会连着发一堆 403 请求，弹一堆错误 toast -->
        <RouterView v-if="!mustChangePassword" v-slot="{ Component }">
          <!-- v4.6.16：去掉 mode="out-in"。out-in 的「先卸载再挂载」必有一帧纯空白，
               门户大卡片 → 文件管理小网格被这帧空白串成「内容从大变小」的错觉；
               改成同帧交叉淡入，离场页绝对定位叠在原位（配合 #app-scroll relative）。 -->
          <Transition name="route-fade">
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
          <PasswordInput
            v-model="passwordForm.next"
            :placeholder="t('auth.forceChange.next')"
            autocomplete="new-password"
            class="w-full rounded-lg border border-line bg-elevated px-3 py-2 text-xs text-ink outline-none focus:border-accent"
          />
          <PasswordInput
            v-model="passwordForm.confirm"
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
