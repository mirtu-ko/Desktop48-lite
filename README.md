# Desktop48 lite

口袋48 的桌面客户端，[Desktop48.app](https://github.com/mirtu-ko/Desktop48.app)（Electron + Vue 3）的
Tauri 2 重构版：去掉 Electron 主进程，网络与系统能力改由 Rust 承载。

渲染层（Vue 3 + Vite + Element Plus）基本原样保留，只替换进程边界——所有 `window.mainAPI` 调用
经一个通用桥转发到 Rust。

## 功能

直播 / 回放 / 公演 / 专辑 / 成员 / 总选 / 房间 / 设置；RTMP 转流、HLS 回放、独立播放窗、
口袋与 B 站弹幕、房间未读与小房间、下载与录制。

房间只读（不提供发消息、删消息）；macOS 不提供菜单栏常驻，关窗即退出。

## 开始

```bash
npm install
npm run dev        # tauri dev（自动起 Vite 1420）
npm run build:app  # 日常构建，只出 .app
npm run build      # 发版构建，额外出 .dmg
npm run test       # 前端单测（Vitest）
npm run check      # CI 等价：前端单测 + typecheck + build:web + fmt + clippy + cargo test
```

首次启动会导入旧版 Electron 数据（成员库、关注名单、配置、设备 ID），幂等且只读源目录。
凭证不导入（旧版用 `safeStorage` 加密，跨实现解不开），需要重新登录。

## 结构

```
src/
  main.ts             入口：装 mainAPI 后加载 bootstrap
  mainAPI.ts          Electron preload 契约 → invoke('main_api')
  bridge.ts           通道契约表 + call()
  types.ts            前端 DTO
  App.vue             主窗壳
  components/floats/  播放窗根组件
  common/ preload/    纯类型与 IPC 契约
  renderer/src/       从原版迁入的渲染层
test/                 前端单测
src-tauri/src/
  bridge/mod.rs       上下文 / 窗口权限表 / 按域路由
  bridge/channels/    各域通道实现
  lib.rs              启动装配、唯一 command `main_api`、退出清理
  examples/probe.rs   无 GUI 端到端探针（走同一个桥）
```

Rust 模块按域划分：`net`（出口闸门）、`identity`、`database` / `db_slot`、`migrate`、
`pocket`（账号与凭证）、`media` / `ffmpeg` / `download` / `tasks`（转流与任务）、
`nim*` / `danmaku` / `room*` / `unread`（弹幕与房间）、`bilibili*`、`float_window` /
`float_chrome` / `window_geometry` / `windows`（窗口）、`platform`（平台差异唯一出口）、
`app_log` / `feedback`。

## 开发环境

需要 Node 22+ 与 Rust **1.90+**（下限来自 `tauri` / `muda` 等依赖；升级 Tauri 后先跑
`cargo metadata` 看依赖最高 MSRV，再改 `src-tauri/Cargo.toml` 的 `rust-version`）。
CI 见 `.github/workflows/ci.yml`（含 Linux 系统依赖清单）。
