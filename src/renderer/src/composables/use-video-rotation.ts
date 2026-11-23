import type { ComputedRef, Ref } from 'vue'
import { useResizeObserver } from '@vueuse/core'
import { computed, onMounted, onUnmounted, ref, watch } from 'vue'

interface UseVideoRotationOptions {
  /** 视频容器（全屏作用在它上面，浮层才不会丢） */
  videoBoxRef: Ref<HTMLElement | null>
  /** 当前媒体元素（video/audio 二选一，随电台模式切换） */
  getMedia: () => HTMLMediaElement | null
  /** 当前媒体 video 元素（仅监听其尺寸变化） */
  getVideo: () => HTMLVideoElement | null
  /** 电台模式：旋转/缩放/aspect 上报均跳过 */
  isRadio: Ref<boolean> | ComputedRef<boolean>
  /** 画面实际显示宽高比变化时上报（含旋转后的宽高交换），浮窗据此自动定形 */
  onAspect: (_aspect: number) => void
}

/**
 * 画面旋转 + 容器全屏 + 迷你控制条状态，供 LivePlayer / PlaybackPlayer 共用。
 * 旋转作用于 video wrapper，不参与旋转的浮层（弹幕等）由组件自己保证同级不旋转。
 */
export function useVideoRotation(options: UseVideoRotationOptions) {
  const { videoBoxRef, getMedia, getVideo, isRadio, onAspect } = options

  const rotationAngle = ref(0)
  // 渲染角度：与 rotationAngle 语义一致（0° = 原方向），但允许跨 0° 连续累加，
  // 专用于 transform，让动画永远走 [-180°, 180°) 内的最短路径，
  // 避免「0° 左转」先归一化到 270° 再让 CSS 绕 270° 一大圈。
  const renderAngle = ref(0)
  const videoWidth = ref(0)
  const videoHeight = ref(0)
  const boxDimensions = ref({ width: 0, height: 0 })

  const isVerticalRotation = computed(() => {
    const normalizedAngle = ((rotationAngle.value % 360) + 360) % 360
    return normalizedAngle === 90 || normalizedAngle === 270
  })

  // 视频实际渲染区（object-fit: contain 的 letterbox 结果）在容器内的矩形：
  // 竖屏视频放进宽屏全屏容器时左右有黑边，弹幕锚在本矩形左下角才不会离画面太远。
  // bottom 为相对容器底边的偏移，便于自下而上堆叠的弹幕条定位。
  const videoRect = computed(() => {
    const boxW = boxDimensions.value.width || videoBoxRef.value?.clientWidth || 0
    const boxH = boxDimensions.value.height || videoBoxRef.value?.clientHeight || 0
    if (boxW <= 0 || boxH <= 0)
      return null
    // 元数据未就绪：按撑满容器兜底，位置稳定在容器左下角
    if (videoWidth.value <= 0 || videoHeight.value <= 0)
      return { left: 0, top: 0, width: boxW, height: boxH, bottom: 0 }

    // 旋转 90/270 时显示宽高交换
    const ratio = isVerticalRotation.value
      ? videoHeight.value / videoWidth.value
      : videoWidth.value / videoHeight.value
    // contain：撑满一条边，另一条边居中留黑边
    const [width, height] = ratio < boxW / boxH
      ? [boxH * ratio, boxH]
      : [boxW, boxW / ratio]
    // 矩形两轴都居中，故 left/right 与 top/bottom 各自对称
    return {
      left: (boxW - width) / 2,
      top: (boxH - height) / 2,
      width,
      height,
      bottom: (boxH - height) / 2,
    }
  })

  // 旋转 90/270 度时，视频显示宽高会交换，这里单独计算一个缩放系数，
  // 保证旋转后的画面仍然完整落在容器内。
  function calculateRotationScale() {
    if (!isVerticalRotation.value)
      return 1

    const boxWidth = boxDimensions.value.width || videoBoxRef.value?.clientWidth || 0
    const boxHeight = boxDimensions.value.height || videoBoxRef.value?.clientHeight || 0
    const sourceWidth = videoWidth.value
    const sourceHeight = videoHeight.value

    if (boxWidth <= 0 || boxHeight <= 0 || sourceWidth <= 0 || sourceHeight <= 0)
      return 1

    const videoRatio = sourceWidth / sourceHeight
    const boxRatio = boxWidth / boxHeight
    let renderedWidth = 0
    let renderedHeight = 0

    if (videoRatio < boxRatio) {
      renderedHeight = boxHeight
      renderedWidth = renderedHeight * videoRatio
    }
    else {
      renderedWidth = boxWidth
      renderedHeight = renderedWidth / videoRatio
    }

    return Math.min(boxWidth / renderedHeight, boxHeight / renderedWidth)
  }

  const videoWrapperStyle = computed(() => {
    const angle = renderAngle.value
    const scale = calculateRotationScale()

    return {
      transform: `rotate(${angle}deg) scale(${scale})`,
    }
  })

  const videoStyle = computed(() => {
    if (isVerticalRotation.value) {
      return {
        maxWidth: '100%',
        maxHeight: '100%',
        width: 'auto',
        height: 'auto',
      }
    }

    return {}
  })

  // 旋转是一组连续状态，界面上只暴露一个控件，所以每次变更都给一条短暂提示，
  // 避免用户转完就忘了自己停在第几个 90°
  const rotateHint = ref('')
  let rotateHintTimer: ReturnType<typeof setTimeout> | null = null

  function showRotateHint(text: string) {
    rotateHint.value = text
    if (rotateHintTimer)
      clearTimeout(rotateHintTimer)
    rotateHintTimer = setTimeout(() => {
      rotateHint.value = ''
    }, 1400)
  }

  function notifyRotation() {
    showRotateHint(rotationAngle.value === 0 ? '已恢复原方向' : `已旋转 ${rotationAngle.value}°`)
  }

  /** 计算从 from 走到 target 的最短旋转差，落在 [-180°, 180°) 内 */
  function shortestRotationDelta(from: number, target: number) {
    return ((target - from + 540) % 360) - 180
  }

  /** rotationAngle 变更后，把渲染角度沿最短路径同步过去 */
  function applyRotation() {
    renderAngle.value += shortestRotationDelta(renderAngle.value, rotationAngle.value)
  }

  function rotateLeft() {
    rotationAngle.value = ((rotationAngle.value - 90) % 360 + 360) % 360
    applyRotation()
    notifyRotation()
  }

  function rotateRight() {
    rotationAngle.value = (rotationAngle.value + 90) % 360
    applyRotation()
    notifyRotation()
  }

  function resetRotation() {
    if (rotationAngle.value === 0)
      return
    rotationAngle.value = 0
    applyRotation()
    notifyRotation()
  }

  /** 双击：无论是否旋转都切换容器全屏（全屏作用在视频容器上，旋转随 wrapper 一并保留） */
  function onBoxDblClick(event: MouseEvent) {
    if ((event.target as HTMLElement).closest('.player-actions, .mini-controls'))
      return
    event.preventDefault()
    void toggleFullscreen()
  }

  /**
   * 全屏走**窗口全屏**，不用 HTML5 元素全屏。
   *
   * Electron（Chromium）里 `requestFullscreen` 干净可用，所以原版是 `useFullscreen(videoBoxRef)`；
   * 但 macOS 的 WKWebView 会给元素全屏叠一层系统自带的
   * 「Click to exit full screen」提示条（灰色区域），CSS 去不掉。
   * 改走窗口全屏后：提示条消失，标题栏由 FloatPlayer 按 fullscreen 状态收起，
   * 浮层（LIVE 徽标 / 旋转 / 录制）照常可见。
   *
   * `isFullscreen` 改为镜像窗口状态：主进程是唯一真相，本模块只读。
   */
  const isFullscreen = ref(false)

  async function toggleFullscreen() {
    try {
      isFullscreen.value = await window.mainAPI.windowToggleFullscreen()
    }
    catch (error) {
      console.error('[use-video-rotation] 切换全屏失败:', error)
    }
  }

  /** 窗口尺寸变化时同步全屏态（用户可能按 Esc / 绿灯按钮退出全屏） */
  function syncFullscreen() {
    void window.mainAPI
      .windowIsFullscreen()
      .then((value) => {
        isFullscreen.value = value
      })
      .catch(() => {})
  }

  // 旋转 90/270 时原生控制条会跟着画面侧躺，改用自绘迷你条（见 MiniControls）
  const playing = ref(false)
  const muted = ref(false)

  function togglePlay() {
    const el = getMedia()
    if (!el)
      return
    if (el.paused)
      void Promise.resolve(el.play()).catch((error: any) => console.error('[use-video-rotation.ts] 播放失败:', error))
    else
      el.pause()
  }

  function toggleMute() {
    const el = getMedia()
    if (!el)
      return
    el.muted = !el.muted
  }

  function onVolumeChange(event: Event) {
    muted.value = (event.target as HTMLMediaElement).muted
  }

  // 计算画面实际显示宽高比（90/270° 旋转会交换源宽高），上报给浮窗自动定形
  function reportAspect() {
    if (isRadio.value)
      return
    const w = videoWidth.value
    const h = videoHeight.value
    if (!w || !h)
      return
    onAspect(isVerticalRotation.value ? h / w : w / h)
  }

  function updateVideoDimensions() {
    const video = getVideo()
    if (video) {
      videoWidth.value = video.videoWidth || 0
      videoHeight.value = video.videoHeight || 0
    }
    reportAspect()
  }

  function handleNativeVideoResize() {
    updateVideoDimensions()
  }

  // 旋转后画面宽高比随之交换，重新上报给浮窗调整窗口形状
  watch(rotationAngle, () => reportAspect())

  // 容器尺寸观察：电台无视频轨不观察（元素置 null）；ResizeObserver 挂上后
  // 首帧必回调一次，不需要额外做挂载时的初始快照
  useResizeObserver(computed(() => (isRadio.value ? null : videoBoxRef.value)), (entries) => {
    for (const entry of entries) {
      boxDimensions.value = {
        width: entry.contentRect.width,
        height: entry.contentRect.height,
      }
    }
  })

  onMounted(() => {
    const video = getVideo()
    if (video)
      video.addEventListener('resize', handleNativeVideoResize)
  })

  onUnmounted(() => {
    const video = getVideo()
    if (video)
      video.removeEventListener('resize', handleNativeVideoResize)
  })

  return {
    rotationAngle,
    isVerticalRotation,
    videoRect,
    videoWrapperStyle,
    videoStyle,
    rotateHint,
    rotateLeft,
    rotateRight,
    resetRotation,
    onBoxDblClick,
    isFullscreen,
    toggleFullscreen,
    syncFullscreen,
    playing,
    muted,
    togglePlay,
    toggleMute,
    onVolumeChange,
    updateVideoDimensions,
  }
}
