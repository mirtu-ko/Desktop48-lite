// @vitest-environment jsdom
import { beforeEach, describe, expect, it, vi } from 'vitest'

/**
 * `mainAPI.ts` 的外链接管回归。
 *
 * Tauri 没有 Electron 的 `setWindowOpenHandler`：不接管的话，迁入渲染层里的
 * `window.open(url, '_blank')` 与 `<a target="_blank">` 点了没反应（外链死掉）。
 * 这里固定住「哪些链接交给系统浏览器、哪些留在 webview 内」。
 */

const invoke = vi.fn(() => Promise.resolve(null))

vi.mock('@tauri-apps/api/core', () => ({ invoke: (...args: unknown[]) => invoke(...(args as [])) }))
vi.mock('@tauri-apps/api/event', () => ({ listen: () => Promise.resolve(() => {}) }))

const { installMainAPI } = await import('../src/mainAPI')

/** 拦下的 openExternal 调用（mainAPI 是异步 fire-and-forget） */
async function openedUrls(): Promise<string[]> {
  await Promise.resolve()
  return invoke.mock.calls
    .filter(([channel]) => channel === 'main_api')
    .map(([, payload]) => payload as { channel: string, args: string[] })
    .filter(payload => payload.channel === 'openExternal')
    .map(payload => payload.args[0])
}

describe('外链接管', () => {
  // jsdom 不实现 Window.open / 跨文档导航，会经 virtualConsole 打 Not implemented；
  // 这两条路径的断言在用例里，静音掉噪音即可（jsdom 走的是 console.error）。

  beforeEach(() => {
    invoke.mockClear()
    // 每次重新装一遍：窗口对象在 jsdom 里是复用的，先清掉上一次的拦截
    document.body.innerHTML = ''
    installMainAPI()
  })

  it('window.open 站外 http(s) 链接交给 openExternal，且不开新窗口', async () => {
    const result = window.open('https://weibo.com/u/123', '_blank', 'noopener')
    expect(result).toBeNull()
    expect(await openedUrls()).toEqual(['https://weibo.com/u/123'])
  })

  it('站外 <a target=_blank> 点击被拦下并转交', async () => {
    document.body.innerHTML = '<a id="x" href="https://weibo.com/u/42" target="_blank">微博</a>'
    const anchor = document.getElementById('x')!
    anchor.addEventListener('click', (event) => {
      // 默认行为被阻止 = 不会把当前 webview 导航到远端页面
      expect(event.defaultPrevented).toBe(true)
    })
    anchor.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true }))
    expect(await openedUrls()).toEqual(['https://weibo.com/u/42'])
  })

  it('点了子元素（图标）也命中父链接', async () => {
    document.body.innerHTML = '<a id="x" href="https://example.com/a" target="_blank"><span id="s">链接</span></a>'
    document.getElementById('s')!.dispatchEvent(
      new MouseEvent('click', { bubbles: true, cancelable: true }),
    )
    expect(await openedUrls()).toEqual(['https://example.com/a'])
  })

  it('站内 / 非 http 链接不放行给系统浏览器', async () => {
    // tauri://localhost 下的相对路径 = 站内
    const anchor = document.createElement('a')
    anchor.href = '/lives'
    document.body.append(anchor)
    anchor.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true }))

    window.open('mailto:someone@example.com')
    window.open('#/rooms')
    expect(await openedUrls()).toEqual([])
  })

  it('非外链的 window.open 仍走原实现', () => {
    const original = window.open
    // 这里只验证没有被我们吞成 null：jsdom 的原生 open 会返回 null 或 Window，
    // 关键是不能抛出、也不能误发 openExternal。
    expect(() => window.open('about:blank')).not.toThrow()
    window.open = original
    expect(invoke).not.toHaveBeenCalled()
  })
})
