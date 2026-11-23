/**
 * 契约用到的 DTO 定义。
 *
 * 这些类型原先放在 Electron 主进程目录（`src/main/**`）下；本项目没有 Node 主进程，
 * 默认值由 Rust 侧产出（`database.rs` 的 `default_data()`），这里只保留契约需要的类型。
 */

/** 网络诊断单行结果（`netDiagnose` 通道） */
export interface NetDiagnoseItem {
  label: string
  url: string
  status: number | null
  ms: number
  error?: string
  detail?: string
}

/** 下载 / 录制任务快照（`downloadTask*` / `recordTask*` 通道） */
export interface TaskSnapshot {
  liveId: string
  /** 任务类型：同一 liveId 只会有一条任务，列表按它区分下载 / 录制 */
  kind: 'download' | 'record'
  url: string
  filename: string
  filePath: string
  saveDirectory: string
  status: 'running' | 'finish' | 'error'
  startedAt: number
  error?: string
}

/** FFmpeg 环境：不可用原因、来源与二进制路径（`getFfmpegStatus` 通道） */
export type FfmpegProblem = 'unset' | 'missing-dir' | 'missing-binary'
export type FfmpegSource = 'directory' | 'system'
export interface FfmpegStatus {
  ok: boolean
  problem: FfmpegProblem | null
  directory: string
  binaryPath: string
  source: FfmpegSource
}

/**
 * `database.json` 的成员数据结构（字段依据 UPDATE_INFO_URL 的真实返回逐字段建模）。
 *
 * 宽容原则：仅各分节主键必需，其余字段可选，并用索引签名兼容上游新增字段。
 */

/** ===== starInfo：成员（艺人）记录 ===== */
export interface StarInfoItem {
  userId: number
  /** 真实姓名 */
  realName?: string
  /** 艺名/昵称 */
  nickname?: string
  /** 姓名拼音缩写（如 zjm） */
  abbr?: string
  /** 姓名拼音（如 Zhao JiaMin） */
  pinyin?: string
  groupId?: number
  groupName?: string
  teamId?: number
  teamName?: string
  periodId?: number
  /** 期数名称（如 "SNH48 一期生"，对应 periodInfo） */
  periodName?: string
  /** 0=未出道 1=在团 2=暂休 3=已退团 */
  status?: number
  /** 生日（MM-dd） */
  birthday?: string
  /** 出生地 */
  birthplace?: string
  /** 血型 */
  bloodType?: string
  /** 星座 */
  constellation?: string
  /** 身高 */
  height?: string
  /** 特长 */
  specialty?: string
  /** 爱好 */
  hobbies?: string
  /** 星座解读文案 */
  starRegion?: string
  /** 入团时间（yyyy-MM-dd） */
  joinTime?: string
  /** 头像地址 */
  avatar?: string
  /** 全身照 1-4 */
  fullPhoto1?: string
  fullPhoto2?: string
  fullPhoto3?: string
  fullPhoto4?: string
  /** 微博昵称 */
  wbName?: string
  /** 微博 uid */
  wbUid?: string
  /** 创建时间（毫秒时间戳） */
  ctime?: number
  /** 更新时间（毫秒时间戳） */
  utime?: number
  [key: string]: unknown
}

/** ===== officialInfo：官方账号（字段与成员记录一致，如 口袋48/SNH48 官方号） ===== */
export type OfficialInfoItem = StarInfoItem

/** ===== domainInfo：站点域名配置（各分团官网域名） ===== */
export interface DomainInfoItem {
  domainId: number
  /** 域名（如 snh48.com） */
  domainUrl?: string
  domainStatus?: number
  /** 创建时间（毫秒时间戳） */
  createAt?: number
  /** 更新时间（毫秒时间戳） */
  updateAt?: number
  [key: string]: unknown
}

/** ===== starAdjunctInfo：兼职成员档案（带官方账号关联与证书图） ===== */
export interface StarAdjunctItem {
  adjunctId: number
  id?: number
  /** 关联的成员 userId */
  userId?: number
  /** 关联的官方账号 userId（officialInfo.userId） */
  officialId?: number
  /** 成员姓名 */
  starName?: string
  /** 艺名/昵称 */
  nickname?: string
  /** 姓名拼音缩写 */
  abbr?: string
  groupId?: number
  teamId?: number
  status?: number
  birthday?: string
  birthplace?: string
  bloodType?: string
  constellation?: string
  height?: string
  specialty?: string
  hobbies?: string
  starRegion?: string
  /** 头像地址 */
  headImg?: string
  /** 全身照 1-4 */
  fullPhoto1?: string
  fullPhoto2?: string
  fullPhoto3?: string
  fullPhoto4?: string
  /** 证书图片地址 */
  certificateImg?: string
  /** 更新时间（毫秒时间戳） */
  utime?: number
  [key: string]: unknown
}

/** ===== starRepeatInfo：成员重复档案（样本仅 1 条，字段按原始数据定义） ===== */
export interface StarRepeatItem {
  repeatId: number
  id?: number
  userId?: number
  groupId?: number
  teamId?: number
  status?: number
  /** 更新时间（毫秒时间戳） */
  utime?: number
  [key: string]: unknown
}

/** ===== groupInfo：团体记录 ===== */
export interface GroupInfoItem {
  groupId: number
  groupName?: string
  /** 团体主色（HEX，如 38BAEE） */
  groupColor?: string
  /** 排序权重（成员树按此排序） */
  groupSort?: number
  status?: number
  /** 创建时间（毫秒时间戳） */
  ctime?: number
  /** 更新时间（毫秒时间戳） */
  utime?: number
  [key: string]: unknown
}

/** ===== periodInfo：期数（生）信息 ===== */
export interface PeriodInfoItem {
  periodId: number
  /** 期数名称（如 "SNH48 一期生"） */
  periodName?: string
  /** 排序权重 */
  periodSort?: number
  groupId?: number
  status?: number
  /** 创建时间（毫秒时间戳） */
  ctime?: number
  /** 更新时间（毫秒时间戳） */
  utime?: number
  [key: string]: unknown
}

/** ===== teamInfo：队伍记录 ===== */
export interface TeamInfoItem {
  teamId: number
  teamName?: string
  groupId?: number
  /** 队伍主色（HEX，getMemberInfo/teamColorOf 派生 teamColor 的来源） */
  teamColor?: string
  /** 排序权重（成员树按此排序） */
  teamSort?: number
  /** 队伍 logo 地址 */
  teamLogo?: string
  /** 队伍徽章（可能是相对路径，渲染端负责归一化） */
  seineTeamBadge?: string
  /** 队伍制式 logo 地址 */
  seineTeamLogo?: string
  /** 头像背景（HEX） */
  avatarBg?: string
  status?: number
  /** 创建时间（毫秒时间戳） */
  ctime?: number
  /** 更新时间（毫秒时间戳） */
  utime?: number
  [key: string]: unknown
}

/** ===== starOfficialRelationInfo：官方账号与成员的关联（接口当前返回空数组，字段未知，先预留） ===== */
export interface StarOfficialRelationItem {
  [key: string]: unknown
}

/** ===== allmember 记录：h5.48.cn/resource/jsonp/allmembers.php 返回 rows 的展开（字段名沿用原始蛇形命名） ===== */
export interface AllMemberItem {
  sid?: string
  gid?: string
  gname?: string
  sname?: string
  fname?: string
  pinyin?: string
  abbr?: string
  tid?: string
  tname?: string
  pid?: string
  pname?: string
  nickname?: string
  company?: string
  join_day?: string
  height?: string
  birth_day?: string
  star_sign_12?: string
  star_sign_48?: string
  birth_place?: string
  speciality?: string
  hobby?: string
  experience?: string
  catch_phrase?: string
  weibo_uid?: string
  weibo_verifier?: string
  blood_type?: string
  tieba_kw?: string
  status?: string
  ranking?: string
  pocket_id?: string
  is_group_new?: string
  tcolor?: string
  gcolor?: string
  [key: string]: unknown
}

/**
 * UPDATE_INFO_URL 返回的成员相关全量数据（对应渲染端 api-types.ts 的 SyncInfoContent），
 * 也是 database.json 里成员部分的结构（不含 blockedMemberIds/config）。
 */
export interface MemberDataContent {
  officialInfo: OfficialInfoItem[]
  domainInfo: DomainInfoItem[]
  starInfo: StarInfoItem[]
  starAdjunctInfo: StarAdjunctItem[]
  starRepeatInfo: StarRepeatItem[]
  groupInfo: GroupInfoItem[]
  periodInfo: PeriodInfoItem[]
  teamInfo: TeamInfoItem[]
  starOfficialRelationInfo: StarOfficialRelationItem[]
  /** h5.48.cn 全量成员名单（allmembers.php） */
  allmembers: AllMemberItem[]
}
