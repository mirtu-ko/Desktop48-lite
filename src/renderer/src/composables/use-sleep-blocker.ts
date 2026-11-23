import { ref } from 'vue'
import { debugLog } from '@renderer/utils/debug'

/**
 * 播放防休眠：持有主进程的 powerSaveBlocker id，
 * 播放开始时 acquire、暂停/结束/卸载时 release。
 * LivePlayer（直播）与 PlaybackPlayer（录播）共用。
 *
 * id 是主进程分配的句柄，release 时必须原样传回，否则解除的是别人的锁。
 */
export function useSleepBlocker() {
  const id = ref<number | null>(null)

  async function acquire() {
    if (id.value !== null)
      return
    try {
      id.value = await window.mainAPI.preventSleep()
    }
    catch (error) {
      // 防休眠是可选能力：未接入的平台不能因此阻断开播。
      debugLog('playback', `防休眠不可用，继续播放: ${error}`)
    }
  }

  function release() {
    if (id.value === null)
      return
    // ★ 跨进程：preload/index.ts → main/app.ts 的 'allowSleep' handler
    window.mainAPI.allowSleep(id.value)
    id.value = null
  }

  return { acquire, release }
}
