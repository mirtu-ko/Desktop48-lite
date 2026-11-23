// @vitest-environment jsdom
import { beforeEach, describe, expect, it, vi } from 'vitest'

/**
 * `mainAPI.ts` 事件参数形状回归。
 *
 * 主进程把多参数事件（任务的 Progress / End / Error）按 Electron
 * `webContents.send(channel, ...args)` 的约定打包成数组下发，而 Tauri 的
 * `listen` 只给一个 payload。桥不展开数组的话，渲染层
 * `end((liveId, filePath) => …)` 会把整个数组当成 liveId，
 * 于是**跨窗口**的任务状态永远对不上（停止后另一窗还显示「录制中 / 下载中」）。
 * 这里固定住「哪些通道展开、哪些保持单参」。
 */

type EventHandler = (_message: { payload: unknown }) => void

const handlers = new Map<string, EventHandler>()

vi.mock('@tauri-apps/api/core', () => ({ invoke: () => Promise.resolve(null) }))
vi.mock('@tauri-apps/api/event', () => ({
  listen: (event: string, handler: EventHandler) => {
    handlers.set(event, handler)
    return Promise.resolve(() => handlers.delete(event))
  },
}))
vi.mock('@tauri-apps/api/webviewWindow', () => ({
  getCurrentWebviewWindow: () => ({
    minimize: () => Promise.resolve(),
    close: () => Promise.resolve(),
    isMaximized: () => Promise.resolve(false),
    maximize: () => Promise.resolve(),
    unmaximize: () => Promise.resolve(),
    isFullscreen: () => Promise.resolve(false),
    setFullscreen: () => Promise.resolve(),
    setTitle: () => Promise.resolve(),
  }),
}))

const { installMainAPI } = await import('../src/mainAPI')

/** 订阅一个事件，返回「主进程推一条 payload」的触发器 */
function subscribe(event: string) {
  const received: unknown[][] = []
  ;(window.mainAPI as any)[event]((...args: unknown[]) => received.push(args))
  const handler = handlers.get(event)
  expect(handler, `事件 ${event} 未被订阅`).toBeDefined()
  return {
    received,
    push: (payload: unknown) => handler!({ payload }),
  }
}

describe('任务事件参数展开', () => {
  beforeEach(() => {
    handlers.clear()
    installMainAPI()
  })

  it('End / Progress / Error 把数组 payload 展开成多个实参', () => {
    for (const event of [
      'recordTaskEnd',
      'recordTaskProgress',
      'recordTaskError',
      'downloadTaskEnd',
      'downloadTaskProgress',
      'downloadTaskError',
    ]) {
      const channel = subscribe(event)
      channel.push(['live-1', 'payload-2'])
      expect(channel.received, event).toEqual([['live-1', 'payload-2']])
    }
  })

  it('Started 保持单个快照参数（跨窗镜像据此并入本地列表）', () => {
    const channel = subscribe('recordTaskStarted')
    const snapshot = { liveId: 'live-1', status: 'running' }
    channel.push(snapshot)
    expect(channel.received).toEqual([[snapshot]])
  })

  it('单参数事件不展开：批量事件与字符串参数原样传入', () => {
    const batch = subscribe('pocketUnreadBatch')
    batch.push({ rooms: [1, 2] })
    expect(batch.received).toEqual([[{ rooms: [1, 2] }]])

    const live = subscribe('liveUnavailable')
    live.push('live-1')
    expect(live.received).toEqual([['live-1']])
  })
})
