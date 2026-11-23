// @vitest-environment jsdom
import { beforeEach, describe, expect, it, vi } from 'vitest'

/**
 * 跨窗口任务镜像回归（`mainAPI` → `stores/tasks` → `services/task-runtime`）。
 *
 * 复现场景：录制在独立播放窗发起，主窗口只有镜像。停止发生在发起窗，
 * 主进程广播 End；镜像窗若拿不到（或参数形状不对）就会一直停在「录制中」，
 * 表现就是「浮窗点了停止，主窗口还在录制 / 反方向也一样」。
 *
 * 这里用假的主进程事件流把整条链路跑一遍：Started 建卡片 → Progress 刷新时长
 * → End 复位；同时确认 End 只作用于它自己的 liveId。
 */

type Handler = (_message: { payload: unknown }) => void

const handlers = new Map<string, Handler>()

vi.mock('@tauri-apps/api/core', () => ({ invoke: () => Promise.resolve([]) }))
vi.mock('@tauri-apps/api/event', () => ({
  listen: (event: string, handler: Handler) => {
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
vi.mock('element-plus', () => {
  const message = Object.assign(vi.fn(), { error: vi.fn(), warning: vi.fn(), success: vi.fn() })
  return { ElMessage: message }
})

/** 主进程广播一条事件（payload 形状与 Rust `event_payload` 一致） */
function broadcast(event: string, payload: unknown) {
  const handler = handlers.get(event)
  expect(handler, `事件 ${event} 未被订阅`).toBeDefined()
  handler!({ payload })
}

const LIVE_ID = 'live-1'

function snapshot(overrides: Record<string, unknown> = {}) {
  return {
    liveId: LIVE_ID,
    kind: 'record',
    url: 'rtmp://cyflv.48.cn/live',
    filename: 'a.flv',
    filePath: '',
    saveDirectory: 'D:/videos',
    status: 'running',
    startedAt: 0,
    ...overrides,
  }
}

/** 每个用例都重装一遍：store 是模块级单例，`installed` / 列表都跨用例复用 */
async function loadStore() {
  vi.resetModules()
  handlers.clear()
  const { installMainAPI } = await import('../src/mainAPI')
  installMainAPI()
  const store = await import('../src/renderer/src/stores/tasks')
  store.installTasks({ silent: true })
  return store
}

describe('跨窗口任务镜像', () => {
  beforeEach(() => {
    vi.resetModules()
    handlers.clear()
  })

  it('别的窗口发起录制：Started 建出镜像卡片，按钮进入运行态', async () => {
    const store = await loadStore()
    const tasks = store.useTasksStore()
    expect(tasks.isTaskRunning('record', LIVE_ID)).toBe(false)

    broadcast('recordTaskStarted', snapshot())

    expect(tasks.isTaskRunning('record', LIVE_ID)).toBe(true)
    expect(tasks.recordTasks.value).toHaveLength(1)
  })

  it('发起窗停止后 End 广播让镜像复位（不再显示录制中）', async () => {
    const store = await loadStore()
    const tasks = store.useTasksStore()
    broadcast('recordTaskStarted', snapshot())
    expect(tasks.isTaskRunning('record', LIVE_ID)).toBe(true)

    // Rust 侧多参数事件按 Electron 约定打包成数组下发
    broadcast('recordTaskEnd', [LIVE_ID, 'D:/videos/a.flv'])

    expect(tasks.isTaskRunning('record', LIVE_ID)).toBe(false)
    expect(tasks.recordTasks.value[0].status).toBe('finished')
    expect(tasks.recordTasks.value[0].filePath).toBe('D:/videos/a.flv')
  })

  it('Progress 广播刷新镜像的已录制时长', async () => {
    const store = await loadStore()
    broadcast('recordTaskStarted', snapshot())
    broadcast('recordTaskProgress', [LIVE_ID, '00:01:23.45'])
    expect(store.useTasksStore().recordTasks.value[0].elapsed).toBe('00:01:23.45')
    // 结束即清空，避免卡片一直挂着一个旧时长
    broadcast('recordTaskEnd', [LIVE_ID, 'D:/videos/a.flv'])
    expect(store.useTasksStore().recordTasks.value[0].elapsed).toBeUndefined()
  })

  it('录制与下载各存一份镜像：同一 liveId 的 End 只复位自己那一类', async () => {
    const store = await loadStore()
    broadcast('downloadTaskStarted', snapshot({ kind: 'download', filename: 'a.mp4' }))
    broadcast('recordTaskStarted', snapshot())
    const tasks = store.useTasksStore()
    expect(tasks.isTaskRunning('download', LIVE_ID)).toBe(true)
    expect(tasks.isTaskRunning('record', LIVE_ID)).toBe(true)

    // 录制的 End 不该把同 liveId 的下载镜像一起复位（kind 各存一份）
    broadcast('recordTaskEnd', [LIVE_ID, 'D:/videos/a.flv'])
    expect(tasks.isTaskRunning('record', LIVE_ID)).toBe(false)
    expect(tasks.isTaskRunning('download', LIVE_ID)).toBe(true)
  })
})
