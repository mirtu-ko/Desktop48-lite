// 与 Rust 侧 DTO / `src/bridge.ts` 的通道表一一对应（serde rename_all = camelCase）。

/** FFmpeg 环境状态（对照 Electron `main/ffmpeg/ffmpeg-process.ts`）。 */
export interface FfmpegDownloadProgress {
  /** 已接收字节（压缩包，不是解压后的二进制大小） */
  received: number
  /** 响应 Content-Length；未知时为 0 */
  total: number
}

export interface FfmpegStatus {
  ok: boolean
  /** ok 为 true 时恒为 null */
  problem: 'unset' | 'missing-dir' | 'missing-binary' | null
  directory: string
  binaryPath: string
  /** 生效来源：用户目录 / 下载产物 / 系统安装 */
  source: 'directory' | 'system'
}

/** `createLiveStream` 返回：本地 HTTP-FLV 播放地址。 */
export interface LiveStreamSession {
  url: string
  liveId: string
}

export interface AppInfo {
  version: string
  platform: string
  arch: string
  device: string
  deviceId: string
}

// ===== 通用 IPC 契约（对齐 Electron `src/preload/ipc-contract.d.ts`）=====

export interface NetRequestOptions {
  url: string
  method?: string
  headers?: Record<string, string>
  body?: string
}

/**
 * 非 2xx 也照常回包：跨进程 reject 只保留 message，状态码丢了渲染层就无法区分
 * 「429 该停」与「502 该重试」（与 Electron 版同口径）。
 */
export interface NetResponse {
  status: number
  body: string
  headers: Record<string, string>
}

/** 网络诊断单行结果（对照 Electron `NetDiagnoseItem`）。 */
export interface NetDiagnoseItem {
  label: string
  url: string
  /** null 表示未收到响应 */
  status: number | null
  ms: number
  error?: string
  detail?: string
}

// ===== P1：database.json（对齐 Electron `main/database.ts` 读接口）=====

export type MemberFlagKind = 'blocked' | 'followed'

/** 配置键与 Electron `common/app-config.ts` 同源。 */
export type ConfigKey
  = | 'downloadDirectory'
    | 'ffmpegDirectory'
    | 'danmakuPlacement'
    | 'floatTopmost'

/** 成员记录：只建模稳定字段，其余原样透传。 */
export interface MemberRecord {
  userId: number
  realName?: string
  nickname?: string
  groupId?: number
  groupName?: string
  teamId?: number
  teamName?: string
  teamColor?: string
  [key: string]: unknown
}

export interface AllMembersPayload {
  allmembers: MemberRecord[]
  adjuncts?: Array<Record<string, unknown>>
  officials?: MemberRecord[]
}

export interface MemberInfo extends MemberRecord {
  teamColor: string
}

export interface MemberFlag extends MemberRecord {
  userId: number
  realName: string
  teamColor: string
}

/** 预备生升格（与 `common/member-team-promotion.ts` 同形；不含账号身份） */
export interface MemberTeamPromotion {
  userId: number
  teamName: string
  teamColor: string
}

export interface MemberTreeLeaf {
  label: string
  value: string
  userId?: number
  groupId?: number
  groupName?: string
  teamColor?: string
  [key: string]: unknown
}

export interface MemberTreeTeam {
  teamName: string
  label: string
  value: string
  teamBadge: string
  teamColor: string
  children: MemberTreeLeaf[]
}

export interface MemberTreeGroup {
  groupName: string
  groupId?: number | string
  label: string
  value: string
  children: MemberTreeTeam[]
}

/** `saveMemberData` 入参：10 个成员相关分节都可选，出现即整组覆盖。 */
export interface MemberDataContent {
  officialInfo?: MemberRecord[]
  domainInfo?: Array<Record<string, unknown>>
  starInfo?: MemberRecord[]
  starAdjunctInfo?: Array<Record<string, unknown>>
  starRepeatInfo?: Array<Record<string, unknown>>
  groupInfo?: Array<Record<string, unknown>>
  periodInfo?: Array<Record<string, unknown>>
  teamInfo?: Array<Record<string, unknown>>
  starOfficialRelationInfo?: Array<Record<string, unknown>>
  allmembers?: Array<Record<string, unknown>>
}

/** 登录态（不含 token 本体）；与 Electron `PocketAuthState` 同形。 */
export interface PocketAuthState {
  loggedIn: boolean
  nickname: string
  avatar: string
  userId: string
  mobileMasked: string
  /** 身份档位：本项目恒为口袋48（字段保留以维持契约形状） */
  clientProfile: 'pocket48'
  plainFallback: boolean
  savedAt: number
}

export interface PocketSmsResult {
  needCaptcha: boolean
  question: string
  options: string[]
  message: string
}

export interface PocketVerifyResult {
  valid: boolean
  nickname: string
  userId: string
  level: string
  imOk: boolean
  imAccid: string
  moneyOk: boolean
  moneyPay: number
  moneyFree: number
  moneyTotal: number
  support: string
  fans: string
  likes: string
  signature: string
  message: string
}

export interface PocketCheckInResult {
  addExp: number
  addMoney: number
  addSupport: number
  days: number
}
