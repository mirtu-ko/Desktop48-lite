import { createApp } from 'vue'
import { installElement } from '@renderer/plugins/element-plus'
import { installDensitySignal } from '@renderer/utils/apply-density'
import App from './App.vue'
import FloatPlayer from './components/floats/FloatPlayer.vue'
import './renderer/src/assets/css/app.scss'
import './styles.css'
import { router } from './router'
import { installTasks } from '@renderer/stores/tasks'

/** 前端启动：必须在 `installMainAPI()` 之后调用（迁入模块在求值期就会读 `window.mainAPI`）。 */
export function start(): void {
  installDensitySignal()

  const app = createApp(App)
  app.use(router)
  installElement(app)
  app.mount('#app')
}

/**
 * 独立播放窗：只挂播放器根组件。
 *
 * 刻意不装 router、不挂 `App.vue` —— 播放窗不触发初始化门禁、成员同步等主界面副作用，
 * 打开即播（与 Electron `main.ts` 的 hash 分流同口径）。
 */
export function startFloat(): void {
  installDensitySignal()
  // 下载 / 录制任务镜像：播放窗是独立 webview，任务 store 也各一份，
  // 不在这里安装就既不会从主进程恢复快照、也收不到 Started / End 广播，
  // 结果浮窗的录制 / 下载按钮永远显示「未在运行」，与主窗状态互不相干。
  // 静默模式：完成提示只由发起窗口弹一次，这里只做镜像。
  installTasks({ silent: true })
  const app = createApp(FloatPlayer)
  installElement(app)
  app.mount('#app')
}
