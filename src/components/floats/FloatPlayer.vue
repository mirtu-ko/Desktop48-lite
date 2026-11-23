<script setup lang="ts">
/**
 * 独立播放窗根组件（无边框 + 自定义标题栏）。
 *
 * 与 Electron `components/floats/FloatWindowApp.vue` 同结构：hash 只带 kind + liveId 短 key，
 * 全量载荷经 `floatPlayerGetPayload` 回取；窗口控制通道按「发起窗口」定位，所以同一份
 * `window.mainAPI` 在这里作用的就是本窗。
 *
 * 播放器直接用原版 `LivePlayer` / `PlaybackPlayer`（原样迁入）：
 * 结束卡 / 加载失败卡 / 直播下架广播 / 禁言门控 / 互动榜单 / 弹幕开关与摆放 /
 * 媒体快捷键 / 观看时长统计全部在里面，浮窗只负责窗口壳与置顶策略。
 */
import type { FloatPlayerKind, FloatPlayerPayload } from '@preload/ipc-contract'
import { isFloatTopmostPinned, nextFloatTopmost } from '@common/float-window'
import MediaIcon from '@renderer/components/ui/MediaIcon.vue'
import { useEventListener } from '@vueuse/core'
import { computed, onMounted, onUnmounted, ref, watchEffect } from 'vue'
import { flushPlayDuration, openPlayDuration } from '@renderer/utils/play-duration'
import LivePlayer from '@renderer/components/player/LivePlayer.vue'
import PlaybackPlayer from '@renderer/components/player/PlaybackPlayer.vue'
import EventBus from '@renderer/services/event-bus'

const params = new URLSearchParams(window.location.hash.split('?')[1] ?? '')
const kind: FloatPlayerKind = params.get('kind') === 'playback' ? 'playback' : 'live'
const liveId = params.get('liveId') ?? ''
// 时长不能在组件卸载时上报：关窗会销毁整个渲染进程，Vue 的 onUnmounted 不执行。
// 三段式登记：打开时记类型、首次出画记起点、关窗（或 pagehide）时上报一次分桶。
openPlayDuration(kind)

// undefined=加载中；null=主进程已查不到（窗口信息失效）
const payload = ref<FloatPlayerPayload | null | undefined>(undefined)
const avatarUrl = ref('')
const isMaximized = ref(false)
const hint = ref('')
let hintTimer: ReturnType<typeof setTimeout> | undefined

// 本窗置顶策略由主进程持有（图钉只改这一扇窗，不落盘）
const topmost = ref<'always' | 'playing' | 'never'>('playing')
// null = 播放器还没上报；图钉亮灭要以「此刻是否真的置顶」为准
const playing = ref<boolean | null>(null)
const pinned = computed(() => isFloatTopmostPinned(topmost.value, playing.value))

let disposeMaximizeChange: (() => void) | undefined
let disposeTopmostChange: (() => void) | undefined

/** 播放窗与主窗口不共享 EventBus：上报主进程转发给主窗口，列表页才能刷新 */
function onLiveUnavailable(id: string) {
  void window.mainAPI.notifyLiveUnavailable(id)
}

onMounted(() => {
  EventBus.on('live-unavailable', onLiveUnavailable)
  void syncWindowFullscreen()
  void window.mainAPI.floatPlayerGetPayload(kind, liveId).then((value) => {
    payload.value = value
  })
  void window.mainAPI.windowIsMaximized().then((value) => {
    isMaximized.value = value
  })
  // 先订阅再取初值：取的过程中被改动也不会停在旧值上
  disposeTopmostChange = window.mainAPI.windowOnTopmostChange((preference) => {
    topmost.value = preference
  })
  void window.mainAPI.floatWindowGetTopmost().then((preference) => {
    if (preference)
      topmost.value = preference
  })
  disposeMaximizeChange = window.mainAPI.windowOnMaximizeChange((value) => {
    isMaximized.value = value
  })
})

const barTitle = computed(() => {
  const current = payload.value
  if (!current)
    return ''
  return current.nickname ? `${current.nickname}: ${current.title}` : current.title
})

// macOS 程序坞右键菜单 / 调度中心窗口标题。
// document.title 与原生窗口标题是两个独立字段：只改前者，程序坞右键里仍是建窗时的
// 「直播」/「回放」。两条都要设，后者经 windowSetTitle 落到 Rust 的 set_title。
watchEffect(() => {
  const title = barTitle.value
  if (!title)
    return
  const full = `${kind === 'live' ? '直播' : '回放'} · ${title}`
  document.title = full
  void window.mainAPI.windowSetTitle(full).catch(() => undefined)
})

const viewportWidth = ref(window.innerWidth)
const compact = computed(() => viewportWidth.value < 420)

function onClose() {
  // 先上报观看时长再关窗：窗口销毁后渲染层不再有机会发事件
  flushPlayDuration()
  void window.mainAPI.windowClose()
}

function onMinimize() {
  void window.mainAPI.windowMinimize()
}

/** 切换由主进程执行，新状态经 windowOnMaximizeChange 推回来（不做本地乐观更新） */
function onToggleMaximize() {
  void window.mainAPI.windowToggleMaximize()
}

/** 子播放器上报视频宽高比 → 主进程按比例定形窗口 */
function onAspect(aspect: number) {
  void window.mainAPI.floatWindowFitAspect(aspect)
}

function onAvatar(url: string) {
  if (url && url !== avatarUrl.value)
    avatarUrl.value = url
}

function onPlaying(value: boolean) {
  playing.value = value
  void window.mainAPI.floatWindowSetPlaying(value)
}

function showHint(text: string) {
  hint.value = text
  clearTimeout(hintTimer)
  hintTimer = setTimeout(() => {
    hint.value = ''
  }, 1800)
}

/** 图钉是本窗二态：点亮时取消置顶，未点亮时改为始终置顶 */
function onToggleTopmost() {
  const next = nextFloatTopmost(topmost.value, playing.value)
  void window.mainAPI.floatWindowSetTopmost(next).then(() => {
    showHint(next === 'always' ? '本窗已置顶 · 始终在最前' : '本窗已取消置顶 · 播放时也不置顶')
  })
}

const topmostTitle = computed(() => {
  if (pinned.value) {
    return topmost.value === 'always'
      ? '本窗已置顶 · 点击取消置顶'
      : '本窗播放中置顶（设置页策略）· 点击取消置顶'
  }
  return topmost.value === 'never'
    ? '本窗不置顶 · 点击始终置顶'
    : '本窗已暂停，暂不置顶（设置页策略）· 点击始终置顶'
})

// 全屏时标题栏虽不可见，其拖拽区仍可能吞点击：全屏期间停用 drag
/** 窗口全屏（不是 DOM 全屏）：全屏时标题栏隐藏，只留画面 */
const htmlFullscreen = ref(false)

async function syncWindowFullscreen() {
  try {
    htmlFullscreen.value = await window.mainAPI.windowIsFullscreen()
  }
  catch {
    htmlFullscreen.value = false
  }
}
useEventListener(window, 'resize', () => {
  viewportWidth.value = window.innerWidth
  // 窗口全屏状态由主进程持有；玩家按 Esc / 绿灯按钮退出全屏时这里同步回来，
  // 否则标题栏的收起状态会与实际不同步
  void syncWindowFullscreen()
})

// Alt+F4 / 任务栏关闭走 pagehide，同样要上报
useEventListener(window, 'pagehide', () => {
  flushPlayDuration()
})

useEventListener(document, 'fullscreenchange', () => {
  htmlFullscreen.value = !!document.fullscreenElement
})
// 窗口全屏状态由主进程持有，本窗只镜像（切换在播放器的全屏按钮里发起）
onUnmounted(() => {
  EventBus.off('live-unavailable', onLiveUnavailable)
  disposeMaximizeChange?.()
  disposeTopmostChange?.()
  clearTimeout(hintTimer)
})
</script>

<template>
  <div class="fw-shell" :class="{ 'is-fullscreen': htmlFullscreen }">
    <div class="fw-root">
      <!-- 皮肤类 app-title-bar 提供底色 / 拖拽 / 按钮外观（与主窗同源），fw-bar 只补本窗排布 -->
      <div
        class="fw-bar app-title-bar"
        :class="{ 'is-html-fullscreen': htmlFullscreen }"
        data-tauri-drag-region
        @dblclick="onToggleMaximize"
      >
        <button
          class="fw-pin"
          :class="{ 'is-on': pinned }"
          :title="topmostTitle"
          @click.stop="onToggleTopmost"
          @dblclick.stop
        >
          <MediaIcon name="pushpinFilled" />
        </button>
        <!-- 拖拽区必须逐个标：data-tauri-drag-region 不继承，子元素不标就会吞掉拖拽 -->
        <span class="fw-kind" :class="{ 'is-playback': kind === 'playback' }" data-tauri-drag-region>
          {{ kind === 'live' ? '直播' : '回放' }}
        </span>
        <img v-if="avatarUrl" :src="avatarUrl" alt="avatar" class="fw-avatar" data-tauri-drag-region draggable="false">
        <span class="fw-title ellipsis" :title="barTitle" data-tauri-drag-region>{{ barTitle }}</span>
        <div class="app-title-bar-actions" @dblclick.stop>
          <button class="app-title-bar-btn" title="最小化" @click.stop="onMinimize">
            <MediaIcon name="minus" />
          </button>
          <button class="app-title-bar-btn" :title="isMaximized ? '还原' : '最大化'" @click.stop="onToggleMaximize">
            <MediaIcon :name="isMaximized ? 'windowRestore' : 'windowMaximize'" />
          </button>
          <button class="app-title-bar-btn app-title-bar-btn--close" title="关闭" @click.stop="onClose">
            <MediaIcon name="close" />
          </button>
        </div>
      </div>

      <div class="fw-body">
        <!-- 直播：原版 LivePlayer（结束卡 / 失败卡 / 下架广播 / 禁言门控 / 榜单 / 弹幕开关都在里面） -->
        <LivePlayer
          v-if="payload && kind === 'live'"
          :live-title="payload.title"
          :live-id="payload.liveId"
          :start-time="payload.startTime"
          :live-type="payload.liveType ?? 1"
          :live-mode="payload.liveMode ?? 0"
          :source="payload.source || 'user'"
          :avatar-url="payload.avatar || ''"
          :bilibili-room-id="payload.bilibiliRoomId ?? 0"
          :compact="compact"
          @avatar="onAvatar"
          @aspect="onAspect"
          @playing="onPlaying"
          @close="onClose"
        />
        <!-- 回放：原版 PlaybackPlayer（无 close emit，回放不依赖「直播下架」关闭路径） -->
        <PlaybackPlayer
          v-else-if="payload"
          :live-id="payload.liveId"
          :start-time="payload.startTime"
          :source="payload.source || 'user'"
          :avatar-url="payload.avatar || ''"
          :compact="compact"
          @avatar="onAvatar"
          @aspect="onAspect"
          @playing="onPlaying"
        />
        <div v-else-if="payload === null" class="fw-hint">
          播放信息已失效，请关闭后重新打开
        </div>
        <div v-else class="fw-hint">
          正在打开播放窗口…
        </div>

        <Transition name="fw-toast">
          <div v-if="hint" class="fw-toast" role="status">
            <MediaIcon name="pushpinFilled" />
            <span>{{ hint }}</span>
          </div>
        </Transition>
      </div>
    </div>
  </div>
</template>

<style scoped lang="scss">
@use '@renderer/assets/css/fluid' as *;

/*
 * 窗口外壳：透明窗 + 自绘圆角/投影（float_window.rs 已关掉系统窗影）。
 * 播放画面本身是直角全幅的，圆角只作用在窗壳上；全屏时去掉圆角与留白，
 * 否则全屏画面四角会被切掉一圈、还会留出没有内容的投影边。
 */
.fw-shell {
  display: flex;
  flex-direction: column;
  height: 100%;
  box-sizing: border-box;
}

.fw-root {
  position: relative;
  display: flex;
  flex-direction: column;
  flex: 1;
  min-height: 0;
  overflow: hidden;
  background: var(--el-bg-color-page);
}

/* 深色边缘上的高光要压到几乎不可见，否则一圈白线会显得脏 */
.fw-root::after {
  content: '';
  position: absolute;
  inset: 0;
  z-index: 60;
  border-radius: inherit;
  box-shadow: inset 0 0 0 1px rgba(255, 255, 255, 0.08);
  pointer-events: none;
}

/* 窗口全屏：铺满整块屏幕，不画边缘高光 */
.fw-shell.is-fullscreen {
  .fw-root::after {
    display: none;
  }
}

/* 高度与 Rust 侧 float_bar_height() 是同一条曲线：窄窗 30px、900 及以上 36px */
.fw-bar {
  --bar-h: #{fp(30px, 36px, 1)};
  height: var(--bar-h);
  --bar-btn-w: #{fp(28px, 48px, 1)};
  --bar-icon-size: #{fp(13px, 16px, 1)};
  gap: #{fp(6px, 9px, 1)};
  padding: 0 0 0 6px;
}

/* 窗口全屏时整条标题栏收起：全屏就该只剩画面，标题栏会占掉顶部一截 */
.fw-bar.is-html-fullscreen {
  display: none;
}

.fw-pin {
  flex-shrink: 0;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: fp(24px, 28px);
  height: fp(24px, 28px);
  padding: 0;
  border: none;
  border-radius: 50%;
  font-size: var(--bar-icon-size);
  color: var(--el-text-color-secondary);
  background: transparent;
  cursor: pointer;
  transition:
    color 0.15s ease,
    background-color 0.15s ease,
    transform 0.12s ease;

  &:hover {
    color: var(--brand-primary);
    background: rgba(var(--brand-rgb), 0.1);
  }

  &:active {
    transform: scale(0.9);
  }
}

.fw-pin.is-on {
  color: var(--brand-primary);
  background: rgba(var(--brand-rgb), 0.16);

  &:hover {
    background: rgba(var(--brand-rgb), 0.22);
  }
}

.fw-kind {
  flex-shrink: 0;
  padding: 2px fp(6px, 8px);
  border-radius: var(--radius-pill);
  font-size: fp(10px, 11px);
  line-height: 1.4;
  color: #fff;
  background: var(--gradient-brand);

  &.is-playback {
    background: linear-gradient(135deg, var(--brand-secondary), #ffb0c8);
  }
}

.fw-avatar {
  width: fp(20px, 22px);
  height: fp(20px, 22px);
  border-radius: 50%;
  flex-shrink: 0;
  object-fit: cover;
  border: 1px solid var(--el-border-color-lighter);
}

.fw-title {
  flex: 1;
  min-width: 0;
  font-size: fp(12px, 13px);
  font-weight: 500;
  color: var(--el-text-color-primary);
}

.fw-body {
  position: relative;
  flex: 1;
  min-height: 0;
  overflow: hidden;
  background: #000;
}

.fw-toast {
  position: absolute;
  top: 12px;
  left: 50%;
  z-index: 40;
  display: inline-flex;
  align-items: center;
  gap: 6px;
  max-width: calc(100% - 24px);
  padding: fp(8px, 10px) fp(16px, 20px);
  border: 1px solid rgba(255, 255, 255, 0.14);
  border-radius: var(--radius-pill);
  font-size: var(--fp-font);
  line-height: 1.4;
  color: #fff;
  background: rgba(22, 18, 38, 0.82);
  box-shadow: 0 10px 26px -14px rgba(0, 0, 0, 0.8);
  backdrop-filter: blur(8px);
  pointer-events: none;
  transform: translateX(-50%);
}

.fw-toast-enter-active,
.fw-toast-leave-active {
  transition:
    opacity 0.18s ease,
    transform 0.18s ease;
}

.fw-toast-enter-from,
.fw-toast-leave-to {
  opacity: 0;
  transform: translate(-50%, -6px);
}

@media (prefers-reduced-motion: reduce) {
  .fw-toast-enter-active,
  .fw-toast-leave-active {
    transition: none;
  }
}

.fw-hint {
  display: flex;
  align-items: center;
  justify-content: center;
  height: 100%;
  font-size: 13px;
  color: rgba(255, 255, 255, 0.6);
}
</style>
