/**
 * 主窗口路由：页面与 Electron 版同路径（`routes.ts` 的迁移版）。
 *
 * 全部页面已接入（直播 / 回放 / 公演 / 专辑 / 成员 / 总选 / 房间 / 设置）。
 */
import type { RouteRecordRaw } from 'vue-router'
import { createRouter, createWebHashHistory } from 'vue-router'

// vue-router 原生支持懒加载，无需再套 defineAsyncComponent；KeepAlive 缓存解析后的实例
const Albums = () => import('@renderer/pages/Albums.vue')
const Elections = () => import('@renderer/pages/Elections.vue')
const Lives = () => import('@renderer/pages/Lives.vue')
const Members = () => import('@renderer/pages/Members.vue')
const Playbacks = () => import('@renderer/pages/Playbacks.vue')
const Rooms = () => import('@renderer/pages/Rooms.vue')
const Setting = () => import('@renderer/pages/Setting.vue')
const Shows = () => import('@renderer/pages/Shows.vue')

const routes: RouteRecordRaw[] = [
  { path: '/lives', component: Lives },
  { path: '/lives/playbacks', component: Playbacks, props: route => ({
    memberPreset: route.query.member ? { userId: String(route.query.member) } : null,
  }) },
  { path: '/shows', component: Shows },
  { path: '/albums', component: Albums },
  { path: '/members', component: Members },
  { path: '/elections', component: Elections },
  { path: '/rooms', component: Rooms },
  { path: '/setting', component: Setting },
  { path: '/', redirect: '/lives' },
]

export const router = createRouter({
  history: createWebHashHistory(),
  routes,
})
