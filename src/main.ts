/**
 * 入口只做一件事：先把 `window.mainAPI` 装好，再加载应用（主窗或独立播放窗）。
 *
 * 迁入的渲染层模块（如 stores/pocket-auth.ts）在模块求值阶段就会访问
 * `window.mainAPI.pocketAuthExpired`，因此 App 及其依赖必须动态导入；
 * 静态 import 会被提升到 installMainAPI() 之前，导致启动即 TypeError。
 */
import { FLOAT_WINDOW_HASH_PATH } from '@common/float-window'
import { installMainAPI } from './mainAPI'

installMainAPI()

// 主窗口与独立播放窗复用同一份 index.html，靠 hash 分流：
// 命中 FLOAT_WINDOW_HASH_PATH 只挂播放窗根组件，不装 router、不触发主界面副作用。
// 该常量与 Rust 侧 `float_window::FLOAT_WINDOW_HASH_PATH` 是同一个值。
if (window.location.hash.slice(1).startsWith(FLOAT_WINDOW_HASH_PATH))
  void import('./bootstrap').then(({ startFloat }) => startFloat())
else
  void import('./bootstrap').then(({ start }) => start())
