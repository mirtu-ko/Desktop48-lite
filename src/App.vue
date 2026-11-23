<script setup lang="ts">
import type { DockItem } from '@renderer/components/app/AppDock.vue'
import { ChatDotRound, Headset, Microphone, Setting, Trophy, User, VideoCamera } from '@element-plus/icons-vue'
import AppDock from '@renderer/components/app/AppDock.vue'
import AppTitleBar from '@renderer/components/app/AppTitleBar.vue'
import BackTopButton from '@renderer/components/app/BackTopButton.vue'
import DownloadsEntry from '@renderer/components/app/DownloadsEntry.vue'
import DownloadsCard from '@renderer/components/ui/DownloadsCard.vue'
import { installTasks } from '@renderer/stores/tasks'
import FloatAudioBar from '@renderer/components/floats/FloatAudioBar.vue'
import EventBus from '@renderer/services/event-bus'
import { installPocketUnread, usePocketUnreadStore } from '@renderer/stores/pocket-unread'
import { useAppShortcuts } from '@renderer/composables/use-app-shortcuts'
import { scrollPageToTop } from '@renderer/utils/page-scroll'
import { computed, KeepAlive, ref, watch } from 'vue'
import { useRoute, useRouter } from 'vue-router'

/** 主窗口骨架：自定义标题栏 + 页面层 + 底部悬浮 Dock + 全局浮层。 */
const route = useRoute()
const router = useRouter()

// 房间未读：Rust 60s 轮询检测，本 store 只做镜像；Dock「房间」项据此显示角标
const { totalUnread } = usePocketUnreadStore()
installPocketUnread()

// 任务系统：恢复一次任务快照并订阅进度（录制 / 下载）
installTasks()

/** 任务中心卡片显隐：入口是右下角常驻的 DownloadsEntry */
const downloadsOpen = ref(false)

const dockItems = computed<DockItem[]>(() => [
  { index: '/lives', label: '直播', icon: VideoCamera, color: 'var(--color-lives)' },
  { index: '/rooms', label: '房间', icon: ChatDotRound, color: 'var(--color-rooms)', badge: totalUnread.value || undefined },
  { index: '/shows', label: '公演', icon: Microphone, color: 'var(--color-shows)' },
  { index: '/albums', label: '专辑', icon: Headset, color: 'var(--color-albums)' },
  { index: '/members', label: '成员', icon: User, color: 'var(--color-members)' },
  { index: '/elections', label: '总选', icon: Trophy, color: 'var(--color-elections)' },
  { index: '/setting', label: '设置', icon: Setting, color: 'var(--color-setting)' },
])

const MENU_PATHS = dockItems.value.map(item => item.index)

/** 子页复用父菜单高亮：`/lives/playbacks` 仍属于「直播」。 */
function resolveActiveMenu(path: string): string {
  return MENU_PATHS.find(menuPath => path === menuPath || path.startsWith(`${menuPath}/`)) ?? '/lives'
}

const activeIndex = computed(() => resolveActiveMenu(route.path))

/** 直播子页路径；从其他 Dock 切回时恢复（回放页保持在直播菜单内）。 */
let lastLivePath = '/lives'

watch(
  () => route.path,
  (path) => {
    if (resolveActiveMenu(path) === '/lives')
      lastLivePath = path
  },
  { immediate: true },
)

function changeMenu(path: string) {
  if (resolveActiveMenu(route.path) === path)
    return
  void router.push(path === '/lives' ? lastLivePath : path)
}

/**
 * 托盘点未读房间：切到房间页并带上成员 query（Rooms.vue 的 watch 负责选中房间）。
 * 已在同一个 `?member=` 时 push 不触发 watch，先清空再带上，重复点击也能重新定位。
 */
window.mainAPI.openRoomFromTray((userId) => {
  const member = String(userId)
  if (route.path === '/rooms' && route.query.member === member)
    void router.replace({ path: '/rooms' }).then(() => router.push({ path: '/rooms', query: { member } }))
  else
    void router.push({ path: '/rooms', query: { member } })
})

/** 应用级快捷键：Cmd/Ctrl+1..7 切页、Cmd/Ctrl+R 刷新、Cmd/Ctrl+, 设置、Home 回顶。 */
useAppShortcuts({
  switchMenu: (index) => {
    const path = MENU_PATHS[index]
    if (path)
      changeMenu(path)
  },
  refresh: () => {
    scrollPageToTop()
    EventBus.emit('lives-refresh')
  },
  scrollTop: scrollPageToTop,
  openSetting: () => changeMenu('/setting'),
})

/** 双击 Dock 当前项：回顶；直播菜单再刷新列表（页面通过 EventBus 自行处理）。 */
function onDockRevisit(path: string) {
  scrollPageToTop()
  if (path === '/lives')
    EventBus.emit('lives-refresh')
}

</script>

<template>
  <div id="app">
    <div class="app-frame">
      <AppTitleBar />

      <div class="app-body">
        <div class="app-layout">
          <div class="app-content">
            <router-view v-slot="{ Component }">
              <KeepAlive>
                <component :is="Component" />
              </KeepAlive>
            </router-view>
          </div>

          <AppDock
            :items="dockItems"
            :active="activeIndex"
            @change="changeMenu"
            @revisit="onDockRevisit"
          />

          <BackTopButton />
          <FloatAudioBar />

          <DownloadsEntry @open="downloadsOpen = true" />
          <DownloadsCard :open="downloadsOpen" @close="downloadsOpen = false" />
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped lang="scss">
#app {
  display: flex;
  flex-direction: column;
  height: 100%;
}

/* 窗口外观：圆角由系统裁（窗影也归系统），这里只留边缘高光 */
.app-frame {
  position: relative;
  flex: 1;
  min-height: 0;
  display: flex;
  flex-direction: column;
  overflow: hidden;
  background: var(--el-bg-color-page);
  isolation: isolate;
}

/* 内侧 1px 亮线：浅底上单靠投影分不清「窗口边缘」和「页面留白」 */
.app-frame::after {
  content: '';
  position: absolute;
  inset: 0;
  z-index: 60;
  border-radius: inherit;
  box-shadow: inset 0 0 0 1px var(--window-rim);
  pointer-events: none;
}

.app-body {
  flex: 1;
  min-height: 0;
  display: flex;
  flex-direction: column;
  /* 圆角只由 .app-frame 裁，这里沿用其半径 */
  border-radius: inherit;

  /* 整窗唯一的画布背景：柔和渐变底 + 两角淡彩光晕（与 Electron 版同配方）。 */
  background:
    radial-gradient(1100px 480px at 90% -8%, rgba(var(--brand-rgb), 0.07), transparent 60%),
    radial-gradient(900px 420px at -8% 108%, rgba(255, 143, 184, 0.06), transparent 55%),
    linear-gradient(180deg, #f3f3fb, var(--el-bg-color-page));

  > * {
    flex: 1;
    min-height: 0;
  }
}

.app-content {
  flex: 1;
  min-width: 0;
  height: 100%;
  box-sizing: border-box;
  padding: 0;
  overflow: auto;
}
</style>
