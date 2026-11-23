/**
 * Tauri 侧的 `window.mainAPI` 适配层：把迁入渲染层的 Electron preload 契约接到通用桥。
 *
 * invoke 通道 → `main_api`（位置参数与 Rust `dispatch` 对应）；事件 `onXxx` → `listen('xxx')`；
 * 窗口壳与少量降级项走 `OVERRIDES`，不经过桥。
 * 未实现的通道仍会走桥并由 Rust 返回「未知通道」，渲染层无需维护第二份能力清单。
 */
import type { mainAPI } from '@preload/ipc-contract'
import { invoke } from '@tauri-apps/api/core'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import { getCurrentWebviewWindow } from '@tauri-apps/api/webviewWindow'

/** 事件通道（与 Electron `IpcEventMap` 同名）。 */
const EVENT_CHANNELS = new Set<string>([
  'ffmpegDownloadProgress',
  'roomAlbumDownloadProgress',
  'windowOnMaximizeChange',
  'windowOnTopmostChange',
  'liveUnavailable',
  'danmakuBatch',
  'pocketDanmakuBatch',
  'pocketRoomBatch',
  'pocketUnreadBatch',
  'openRoomFromTray',
  'pocketAuthExpired',
  'downloadTaskProgress',
  'downloadTaskEnd',
  'downloadTaskError',
  'downloadTaskStarted',
  'recordTaskProgress',
  'recordTaskEnd',
  'recordTaskError',
  'recordTaskStarted',
])

function invokeChannel(channel: string, args: unknown[]): Promise<unknown> {
  return invoke('main_api', { channel, args })
}

/**
 * 多参数事件：主进程沿用 Electron 的 `webContents.send(channel, ...args)` 约定，
 * 把多个参数打包成**数组**下发；而 Tauri 的 `listen` 只给一个 `payload`。
 *
 * 因此这些通道必须自己展开成多个实参：不展开的话回调会把整个数组当成第一个参数，
 * 渲染层 `end((liveId, filePath) => …)` 的 liveId 比对永远不成立 ——
 * 表现就是「在浮窗停止录制后，主窗的按钮与任务卡片还停在『录制中』」（反之亦然），
 * 以及进度（elapsed）在任何窗口都不刷新。
 *
 * 与 ipc-contract.d.ts 的 `TaskEventMap` 一一对应（那里的参数元组长度 > 1）；
 * 新增多参数事件必须登记在这里，否则只会拿到一个数组。
 */
const MULTI_ARG_EVENTS = new Set<string>([
  'downloadTaskProgress',
  'downloadTaskEnd',
  'downloadTaskError',
  'recordTaskProgress',
  'recordTaskEnd',
  'recordTaskError',
])

/**
 * 事件订阅：`onXxx` → Tauri 事件 `xxx`。
 * 退订可能早于 `listen()` resolve（页面快速卸载），这里等注册完成后再注销，
 * 避免留下一个永久订阅。
 */
function subscribe(event: string) {
  const spreadArgs = MULTI_ARG_EVENTS.has(event)
  return (handler: (...args: unknown[]) => void): (() => void) => {
    let disposed = false
    let unlisten: UnlistenFn | undefined
    void listen(event, (message) => {
      const payload = message.payload
      if (spreadArgs && Array.isArray(payload))
        handler(...payload)
      else
        handler(payload)
    })
      .then((stop) => {
        if (disposed)
          stop()
        else
          unlisten = stop
      })
      .catch((err) => {
        console.error(`[mainAPI] 订阅事件失败 ${event}:`, err)
      })
    return () => {
      disposed = true
      unlisten?.()
      unlisten = undefined
    }
  }
}

function eventNameOf(property: string): string | null {
  if (EVENT_CHANNELS.has(property))
    return property
  if (property.startsWith('on') && property.length > 2) {
    const name = property[2].toLowerCase() + property.slice(3)
    if (EVENT_CHANNELS.has(name))
      return name
  }
  return null
}

/** 是否是需要交给系统浏览器的站外 http(s) 链接。 */
function isExternalHttpUrl(raw: string): boolean {
  try {
    const parsed = new URL(raw, window.location.href)
    return (parsed.protocol === 'http:' || parsed.protocol === 'https:')
      && parsed.origin !== window.location.origin
  }
  catch {
    return false
  }
}

/** 交给 Rust 的 `openExternal`；失败只记录，不阻断页面。 */
function openExternal(url: string): void {
  void invokeChannel('openExternal', [url]).catch((error) => {
    console.error(`[mainAPI] 打开外链失败 ${url}:`, error)
  })
}

/** 外链接管只装一次（dev 热更新重复求值时不叠加监听器）。 */
let externalLinksInstalled = false

/**
 * 接管 Electron 时代的 `setWindowOpenHandler` 行为。
 *
 * 迁入的渲染层仍按 Electron 写法触发外链：
 * - `window.open(url, '_blank', 'noopener')`（Setting / 专辑页）；
 * - `<a href="https://..." target="_blank">`（成员微博 / 关于页）。
 *
 * Tauri 没有默认的新窗口处理器，WKWebView 会把请求直接吞掉；这里统一转给系统浏览器，
 * 同时拦掉站外 `<a>` 的默认导航，避免当前 webview 被远端页面替换。
 */
function installExternalLinkHandling(): void {
  if (externalLinksInstalled)
    return
  externalLinksInstalled = true

  const originalOpen = window.open
  window.open = ((
    url?: string | URL,
    target?: string,
    features?: string,
  ): Window | null => {
    const href = typeof url === 'string' ? url : url?.toString() ?? ''
    if (href && isExternalHttpUrl(href)) {
      openExternal(href)
      return null
    }
    return originalOpen.call(window, url, target, features)
  }) as typeof window.open

  document.addEventListener('click', (event) => {
    if (event.defaultPrevented)
      return
    const target = event.target
    if (!(target instanceof Element))
      return
    const anchor = target.closest<HTMLAnchorElement>('a[href]')
    if (!anchor?.href || !isExternalHttpUrl(anchor.href))
      return
    event.preventDefault()
    openExternal(anchor.href)
  }, true)
}

/** 当前窗口：窗口壳通道直接作用于发起调用的那一窗（主窗与播放窗共用这份代码）。 */
const currentWindow = () => getCurrentWebviewWindow()

/** 已实现或已有明确降级行为的通道覆盖；其余按通道名直接转发。 */
const OVERRIDES: Record<string, (...args: any[]) => unknown> = {
  // ===== 窗口壳：Tauri 原生窗口 API，不再经 Rust 桥 =====
  windowMinimize: () => currentWindow().minimize().then(() => null),
  windowToggleMaximize: async () => {
    const win = currentWindow()
    if (await win.isMaximized())
      await win.unmaximize()
    else
      await win.maximize()
    return null
  },
  windowClose: () => currentWindow().close().then(() => null),
  windowIsMaximized: () => currentWindow().isMaximized(),
  windowToggleFullscreen: async () => {
    const win = currentWindow()
    const next = !(await win.isFullscreen())
    await win.setFullscreen(next)
    return next
  },
  windowIsFullscreen: () => currentWindow().isFullscreen(),
  windowSetTitle: (title: string) => currentWindow().setTitle(String(title)).then(() => null),
}

/** 把 preload 契约装到当前窗口；只应在应用启动时调用一次。 */
export function installMainAPI(): void {
  installExternalLinkHandling()
  const api = new Proxy({}, {
    get(_target, property) {
      if (typeof property !== 'string')
        return undefined
      if (OVERRIDES[property])
        return OVERRIDES[property]
      const event = eventNameOf(property)
      if (event)
        return subscribe(event)
      return (...args: unknown[]) => invokeChannel(property, args)
    },
  })
  window.mainAPI = api as unknown as mainAPI
}
