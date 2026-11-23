/**
 * 客户端身份（**唯一来源**，主进程与渲染层共用）。
 *
 * 原版支持「口袋48」与「塞纳河48」两套官方客户端身份，登录时可选。
 * **本项目只保留口袋48**：两套身份共用同一账号体系与同一批接口，只影响请求头里
 * 的客户端伪装，维护双档只增加回归面积。详见 README 的「刻意不做」。
 *
 * 本文件只描述「身份」这一层：档位 id 与展示文案。真正的请求头取值放在
 * `main/pocket48/client-profile.ts` 按档位查表生成——那里需要设备信息与持久化装置，
 * 不能进 common（common 是同属 node / web 两份 tsconfig 的纯代码目录）。
 */

export type PocketClientProfileId = 'pocket48'

export interface PocketClientProfileOption {
  id: PocketClientProfileId
  /** 展示名（登录弹窗与设置页共用，勿在两处各写一份） */
  label: string
  /** 一句话说明，给用户看「这是什么身份」 */
  hint: string
}

export const POCKET_CLIENT_PROFILE_OPTIONS: readonly PocketClientProfileOption[] = [
  {
    id: 'pocket48',
    label: '口袋48',
    hint: '登录身份：「口袋48」。',
  },
]

/** 唯一档位（未登录时主进程请求也走它） */
export const DEFAULT_POCKET_CLIENT_PROFILE: PocketClientProfileId = 'pocket48'

/** 归一化外部传入的档位：**任何输入都回退唯一档位**（旧数据里的 seine48 也按口袋48 处理） */
export function resolvePocketClientProfile(_id?: string | null): PocketClientProfileId {
  return DEFAULT_POCKET_CLIENT_PROFILE
}

/** 按 id 取展示项；当前只有一档 */
export function pocketClientProfileOption(_id?: PocketClientProfileId | string | null): PocketClientProfileOption {
  return POCKET_CLIENT_PROFILE_OPTIONS[0]
}
