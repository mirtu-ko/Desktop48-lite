// 通用 IPC 桥的通道类型表：一条通道 = 位置参数数组 + 返回值，与 Rust 的 `CHANNELS` 同步维护。
// 实现分散在 src-tauri/src/bridge/channels/<域>.rs。

import { invoke } from '@tauri-apps/api/core'
import type {
  AllMembersPayload,
  AppInfo,
  ConfigKey,
  FfmpegStatus,
  LiveStreamSession,
  MemberFlag,
  MemberFlagKind,
  MemberDataContent,
  MemberInfo,
  MemberTeamPromotion,
  MemberTreeGroup,
  NetDiagnoseItem,
  NetRequestOptions,
  NetResponse,
  PocketAuthState,
  PocketCheckInResult,
  PocketSmsResult,
  PocketVerifyResult,
} from './types'

/** B 站房间信息（与 `common/bilibili-rooms.ts` 同形） */
export interface BilibiliRoomInfo {
  roomId: number
  title: string
  /** 0=未开播 1=开播 2=轮播 */
  liveStatus: number
}

/** 运行环境摘要（与 `common/feedback.ts` 的 FeedbackMeta 同形） */
export interface FeedbackMeta {
  appVersion: string
  platform: string
  arch: string
  /** 运行时版本（Electron 版为 Electron 版本，Tauri 版为 Tauri 版本） */
  electron: string
  locale: string
  clientProfile: string
  loggedIn: boolean
}

export interface FeedbackMetaResponse {
  meta: FeedbackMeta
  endpointReady: boolean
}

export interface FeedbackInput {
  content: string
  contact?: string
}

// 自动更新已整块移除：检查 / 下载 / 安装三个通道与 `updaterState` 事件都不再存在。
// 设置页只留一个跳 GitHub Releases 的静态入口，走通用 `openExternal`。

/** 单个房间的未读快照 */
export interface UnreadRoomState {
  count: number
  bigCount: number
  smallCount: number
  latestTime: number
}

/** 房间未读全量快照 */
export interface PocketUnreadBatch {
  rooms: Record<string, UnreadRoomState>
  closedChannels: PocketRoomChannel[]
}

/** 已读水位（拉一页最新消息对齐后的结果） */
export interface PocketRoomReadMark {
  channelId: string
  msgTime: number
  msgIdServer: string
}

/** 房间频道 */
export interface PocketRoomChannel {
  serverId: string
  channelId: string
}

/** server 下的频道（channelId + 显示名） */
export interface PocketServerChannel {
  channelId: string
  channelName: string
}

/** 房间发言权限（只取 `sendText` 作判据，其余字段供展示） */
export interface PocketRoomPermission {
  sendText: boolean
  cdTime: number
  roomRole: string
  followFlag: boolean
  hasMsgBoard: boolean | null
}

/** 分页消息回包 */
export interface PocketRoomMessages {
  messages: unknown[]
  nextTime: number
}

/** 频道最后一条消息（左侧预览） */
export interface PocketRoomLastMessage {
  channelId: string
  serverId: string
  starName: string
  msgContent: string
  msgKey: string
  msgTime: number
  unreadCount: number | null
}

/** 下载 / 录制任务快照 */
export interface TaskSnapshot {
  liveId: string
  url: string
  filename: string
  filePath: string
  saveDirectory: string
  status: 'running' | 'finish' | 'error'
  startedAt: number
  error?: string
}

/** 相册里的一张图 */
export interface RoomAlbumSource {
  url: string
  width?: number
  height?: number
  nickname?: string
  msgTime?: number
}

/** 相册下载汇总 */
export interface RoomAlbumResult {
  canceled: boolean
  directory: string
  downloaded: number
  skipped: number
  failed: number
}

/** 暗号提交结果 */
export interface PocketRoomAnswerResult {
  ok: boolean
  via?: 'master' | 'server'
  reason?: 'wrong' | 'unverified'
  message?: string
}

/** 小房间的暗号问题 */
export interface PocketRoomQuestion {
  questionId: string
  question: string
}

export interface ChannelSpec<A extends unknown[], R> {
  args: A
  result: R
}

export interface Channels {
  /** 通用 HTTP 代理：正式 UI 的数据层全部走这条（白名单 / 限流 / App 身份在 Rust） */
  netRequest: ChannelSpec<[options: NetRequestOptions], NetResponse>
  /** 跨域图片转 data URL（画布不被污染） */
  imageDataUrl: ChannelSpec<[url: string], string>
  /** 网络诊断：探测业务依赖的全部端点 */
  netDiagnose: ChannelSpec<[], NetDiagnoseItem[]>
  /** 后端自检信息 */
  getAppInfo: ChannelSpec<[], AppInfo>
  /** database.json 只读：成员库是否已同步 */
  hasMembers: ChannelSpec<[], boolean>
  /** 全量成员名单 + 兼职档案 + 官方账号 */
  getAllMembers: ChannelSpec<[], AllMembersPayload>
  /** 单个成员：原始字段 + teamColor */
  getMemberInfo: ChannelSpec<[userId: number], MemberInfo | null>
  /** 屏蔽 / 关注名单解析（不存在的成员自动过滤） */
  getMemberFlags: ChannelSpec<[kind: MemberFlagKind], MemberFlag[]>
  /** 配置生效值（缺失 / 脏值回落默认值） */
  getConfig: ChannelSpec<[key: ConfigKey], string>
  /** 团体 → 队伍 → 成员三层树（内存派生，不落盘） */
  getMemberTree: ChannelSpec<[], MemberTreeGroup[]>
  /** 写配置：键 / 枚举值校验，原子写 + 写前备份 */
  setConfig: ChannelSpec<[key: ConfigKey, value: string], null>
  /** 整组替换屏蔽 / 关注名单 */
  setMemberFlags: ChannelSpec<[kind: MemberFlagKind, ids: Array<number | string>], null>
  /** 幂等添加单个成员；已存在时不写盘 */
  addMemberFlag: ChannelSpec<[kind: MemberFlagKind, userId: number], null>
  /** 批量幂等合并（口袋关注同步） */
  addMemberFlags: ChannelSpec<[kind: MemberFlagKind, userIds: number[]], null>
  removeMemberFlag: ChannelSpec<[kind: MemberFlagKind, userId: number], null>
  /** 成员同步落库：10 个分节，出现即整组覆盖 */
  saveMemberData: ChannelSpec<[content: MemberDataContent], { ok: true }>
  // ===== 升格：归属（ownerId）只由 Rust 按登录态推导 =====
  /** 当前账号的升格列表（未登录返回空数组） */
  getMemberTeamPromotions: ChannelSpec<[], MemberTeamPromotion[]>
  /** 写入 / 覆盖升格；未登录报「登录口袋账号后才能设置升格」 */
  setMemberTeamPromotion: ChannelSpec<[promotion: MemberTeamPromotion], null>
  /** 取消升格（只删当前账号的记录） */
  removeMemberTeamPromotion: ChannelSpec<[userId: number], null>
  // ===== 系统集成 =====
  /** 用系统默认程序打开本地路径（白名单：标准用户目录 + 配置目录） */
  openPath: ChannelSpec<[filePath: string], null>
  /** 用系统浏览器打开 http/https 外链（替代 Electron 的 setWindowOpenHandler） */
  openExternal: ChannelSpec<[url: string], null>
  /** PNG data URL → 系统剪贴板 */
  copyImageToClipboard: ChannelSpec<[dataUrl: string], null>
  /** PNG data URL → 原生保存对话框；取消返回 null */
  saveImageFile: ChannelSpec<[dataUrl: string, suggestedName: string], string | null>
  // ===== 下载 / 录制任务（ffmpeg 子进程，事件走 downloadTask* / recordTask*）=====
  /** 起任务；返回输出文件绝对路径。失败抛错（地址白名单 / 文件名 / 目录 / 并发上限） */
  downloadTaskStart: ChannelSpec<[url: string, filename: string, liveId: string], string>
  downloadTaskStop: ChannelSpec<[liveId: string], null>
  downloadTaskList: ChannelSpec<[], TaskSnapshot[]>
  downloadTaskRemove: ChannelSpec<[liveId: string], null>
  recordTaskStart: ChannelSpec<[url: string, filename: string, liveId: string], string>
  recordTaskStop: ChannelSpec<[liveId: string], null>
  recordTaskList: ChannelSpec<[], TaskSnapshot[]>
  recordTaskRemove: ChannelSpec<[liveId: string], null>
  /** B 站官方直播间信息（5 分钟缓存，单房间失败以空标题占位） */
  bilibiliRoomInfos: ChannelSpec<[], BilibiliRoomInfo[]>
  /** 播放窗 → 主进程 → 主窗口：直播下架中转（主窗列表据此刷新） */
  notifyLiveUnavailable: ChannelSpec<[liveId: string], null>
  /** 起 B 站弹幕会话；false = 握手失败（房间号取不到 / WBI 被拒 / 网络异常） */
  danmakuStart: ChannelSpec<[roomId: number], boolean>
  /** 停 B 站弹幕会话 */
  danmakuStop: ChannelSpec<[roomId: number], null>
  // ===== 设置页 =====
  /** 路径拼接（平台分隔符由 Rust 决定） */
  pathJoin: ChannelSpec<[...paths: string[]], string>
  /** 应用版本号 */
  getAppVersion: ChannelSpec<[], string>
  /** 反馈页初始化：环境摘要 + 上报通道是否可用 */
  feedbackMeta: ChannelSpec<[], FeedbackMetaResponse>
  /** 提交问题反馈（环境摘要由 Rust 补齐） */
  submitFeedback: ChannelSpec<[input: FeedbackInput], null>
  /** 直播 ID → 聊天室 ID（未登录 / 无聊天室返回空串） */
  pocketGetChatroomId: ChannelSpec<[liveId: string], string>
  /** 直播复盘（未登录报错） */
  pocketGetLiveResult: ChannelSpec<[liveId: string], Record<string, unknown>>
  /** 起聊天室弹幕会话并登记订阅；false = 未登录 / 凭据失效 / 参数无效 */
  pocketDanmakuStart: ChannelSpec<[chatroomId: string, liveId: string], boolean>
  // ===== 房间域（只读：不做发送 / 删除 / 问答应答）=====
  /** 成员 userId → 房间频道（未登录返回空 ID） */
  pocketGetRoomChannel: ChannelSpec<[userId: number], PocketRoomChannel>
  /** 批量解析关注成员的房间频道（未缓存的才解析；空频道 30 分钟后重试） */
  pocketGetRoomChannels: ChannelSpec<[userIds: number[]], Array<{ userId: number, channel: PocketRoomChannel }>>
  /** server 下的频道列表（channelId + 显示名），5 分钟缓存 */
  pocketGetServerChannels: ChannelSpec<[serverId: string, force?: boolean], PocketServerChannel[]>
  /** 房间发言权限（30s 缓存；`sendText=false` = 禁言，渲染层据此禁用输入） */
  pocketGetRoomPermissions: ChannelSpec<[channelId: string], PocketRoomPermission | null>
  /** 房间历史消息；scope = member（仅房主）/ all（全员） */
  pocketGetRoomMessages: ChannelSpec<[channelId: string, serverId: string, nextTime?: number, scope?: 'member' | 'all'], PocketRoomMessages>
  /** 房间精华消息 */
  pocketGetRoomEssenceMessages: ChannelSpec<[channelId: string, serverId: string, nextTime?: number], PocketRoomMessages>
  /** 各 server 下频道的最后一条消息（左侧列表预览） */
  pocketGetRoomLastMessages: ChannelSpec<[serverIds: string[]], PocketRoomLastMessage[]>
  /** 房间未读全量快照（启动时回取一次；之后变化走 pocketUnreadBatch 事件） */
  pocketUnreadState: ChannelSpec<[], PocketUnreadBatch>
  /** 小房间（暗号频道）归属：force=true 立刻重新枚举 */
  pocketRefreshSmallChannel: ChannelSpec<[userId: number, force?: boolean], PocketRoomChannel | null>
  /** 作废成员的小房间归属（旧频道失效） */
  pocketInvalidateSmallChannel: ChannelSpec<[userId: number], null>
  /** 房间相册批量下载：目录只弹一次，进度走 roomAlbumDownloadProgress 事件 */
  roomAlbumDownload: ChannelSpec<[sources: RoomAlbumSource[], filterSmall: boolean], RoomAlbumResult>
  /** 提交小房间暗号；ok=true 才放行（via: master 万能口令 / server 服务端校验） */
  pocketCheckRoomAnswer: ChannelSpec<[channelId: string, questionId: string, answer: string], PocketRoomAnswerResult>
  /** 小房间的暗号问题；null = 无门禁 */
  pocketGetRoomQuestion: ChannelSpec<[channelId: string], PocketRoomQuestion | null>
  /** 标记已读：拉一页最新消息对齐水位并清零该成员未读 */
  pocketMarkRead: ChannelSpec<[userId: number, target?: 'room' | 'small'], PocketRoomReadMark | null>
  /** 进入房间频道：起 QChat 双会话并订阅；事件走 pocketRoomBatch */
  pocketRoomQChatEnter: ChannelSpec<[serverId: string, channelId: string], { ok: boolean, message?: string }>
  /** 离开房间频道；无剩余订阅者时断开连接 */
  pocketRoomQChatLeave: ChannelSpec<[serverId: string, channelId: string], null>
  /** 退订聊天室弹幕；最后一个订阅者离开时断开连接 */
  pocketDanmakuStop: ChannelSpec<[chatroomId: string], null>
  /** 播放窗置顶偏好（always / playing / never）；写库 + 立即作用到窗口 */
  floatWindowSetTopmostPreference: ChannelSpec<[topmost: 'always' | 'playing' | 'never'], null>
  /** FFmpeg 环境状态：省略 dir 时校验当前生效配置 */
  getFfmpegStatus: ChannelSpec<[dir?: string], FfmpegStatus>
  /** 一键下载 ffmpeg-static 预编译二进制，返回所在目录（进度走 ffmpegDownloadProgress 事件） */
  downloadFfmpeg: ChannelSpec<[], string>
  /** 原生目录选择；取消返回 null */
  selectDirectory: ChannelSpec<[], string | null>
  /** 登记直播会话并返回本地 HTTP-FLV 地址（FFmpeg 在播放器 GET 时才启动） */
  createLiveStream: ChannelSpec<[rtmpUrl: string, liveId: string], LiveStreamSession>
  /** 撤销直播会话并中止在途转流 */
  stopLiveStream: ChannelSpec<[liveId: string], null>
  // ===== P3：账号域 =====
  pocketSendSms: ChannelSpec<[mobile: string, area?: string, answer?: string, clientProfile?: string], PocketSmsResult>
  pocketLoginByCode: ChannelSpec<[mobile: string, code: string, area?: string, clientProfile?: string], PocketAuthState>
  pocketLoginByToken: ChannelSpec<[token: string, mobile?: string, clientProfile?: string], PocketAuthState>
  pocketGetAuthState: ChannelSpec<[], PocketAuthState>
  pocketLogout: ChannelSpec<[], null>
  pocketVerifyLogin: ChannelSpec<[], PocketVerifyResult>
  pocketCheckIn: ChannelSpec<[], PocketCheckInResult>
  pocketGetFollowedIds: ChannelSpec<[], number[]>
  pocketFollowMember: ChannelSpec<[userId: number], null>
  pocketUnfollowMember: ChannelSpec<[userId: number], null>
}

export type ChannelName = keyof Channels

/** 与 Rust `main_api` 对应：`invoke('main_api', { channel, args })`。 */
export function call<C extends ChannelName>(
  channel: C,
  ...args: Channels[C]['args']
): Promise<Channels[C]['result']> {
  return invoke<Channels[C]['result']>('main_api', { channel, args })
}
