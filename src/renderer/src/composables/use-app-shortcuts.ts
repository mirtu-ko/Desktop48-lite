/**
 * 应用级快捷键（迁移期新增，原版没有）。
 *
 * 设计原则：
 * - **不抢输入焦点**：在 input / textarea / contenteditable 里按键一律放行，
 *   否则用户打字时会误切页面；
 * - **不抢播放器**：媒体快捷键（空格 / 方向键）归 `use-media-shortcuts`，
 *   这里只处理应用级动作（切页 / 刷新 / 回顶 / 关闭对话框）；
 * - 播放器全屏时整体失效：DOM 全屏下浏览器把按键限制在全屏元素内，切页也没有意义。
 *
 * 键位与 Electron 的菜单快捷键（CmdOrCtrl+1..7）保持同一套直觉，只是原版的菜单栏
 * 在无边框窗口里没有，所以这里直接用 window 监听补上。
 */
import { onMounted, onUnmounted } from 'vue'

export interface AppShortcutHandlers {
  /** 按 Dock 顺序切换页面；index 从 0 开始 */
  switchMenu: (index: number) => void
  /** 刷新当前页（直播 / 回放列表） */
  refresh: () => void
  /** 回到顶部 */
  scrollTop: () => void
  /** 打开设置页 */
  openSetting: () => void
}

/** 是否正在文本输入：是则一切快捷键放行 */
function isTypingTarget(target: EventTarget | null): boolean {
  const element = target as HTMLElement | null
  if (!element)
    return false
  const tag = element.tagName
  return tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT' || element.isContentEditable
}

export function useAppShortcuts(handlers: AppShortcutHandlers) {
  function onKeydown(event: KeyboardEvent) {
    // 输入中 / 组合键（输入法）一律不拦
    if (event.isComposing || isTypingTarget(event.target))
      return
    // 播放器全屏时不接管
    if (document.fullscreenElement)
      return

    const command = event.metaKey || event.ctrlKey

    // Cmd/Ctrl + 1..9：跳到第 N 个菜单项
    if (command && /^[1-9]$/.test(event.key)) {
      const index = Number(event.key) - 1
      if (index < 9) {
        event.preventDefault()
        handlers.switchMenu(index)
      }
      return
    }

    if (command) {
      switch (event.key.toLowerCase()) {
        // 刷新当前页（浏览器默认刷新会丢状态，这里只重取数据）
        case 'r':
          event.preventDefault()
          handlers.refresh()
          return
        // 打开设置
        case ',':
          event.preventDefault()
          handlers.openSetting()
          return
        default:
          return
      }
    }

    switch (event.key) {
      // 回顶：Home（与浏览器默认一致但作用在应用滚动容器上）
      case 'Home':
        handlers.scrollTop()
        return
      default:
        break
    }
  }

  onMounted(() => window.addEventListener('keydown', onKeydown))
  onUnmounted(() => window.removeEventListener('keydown', onKeydown))
}
