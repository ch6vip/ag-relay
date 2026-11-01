// 壳前端共享类型与常量。
// 术语对齐 GLOSSARY.md：活动事件 / 实例归属 / 受管网关 / 外部网关。
export type PageId = 'overview' | 'logs' | 'settings' | 'policy'

export type PolicyGroup = 'codex' | 'general'
export type PolicyMode = 'off' | 'on_error' | 'always'
export interface PolicyProfile {
  maxAttempts: number
  baseDelayMs: number
  maxDelayMs: number
  totalBudgetMs: number
  connectTimeoutMs: number
  historyMode: PolicyMode
  historyConfirmations: number
  uncoveredErrorFallback: boolean
  stripItemIds: boolean
  rememberSession: boolean
  repairStatus: boolean
  retryHistory: boolean
  emptyResponseRepair: boolean
  emptyResponseDropReasoning: boolean
  emptyResponseNormalizeAfter: number
  emptyResponseReasoningAfter: number
}
export interface PolicySettings {
  schemaVersion: 1
  profiles: { codex: PolicyProfile; general: PolicyProfile }
}
export interface PolicySettingsView {
  defaults: PolicySettings
  saved: PolicySettings
  active: PolicySettings | null
  activeRevision: string | null
  restartRequired: boolean
  activeSupported: boolean
}
export type GatewayState = 'running' | 'stopped' | 'external' | 'error' | 'starting' | 'draining'
export type StableState = 'running' | 'stopped' | 'external' | 'error'
export type GatewayAction = 'start' | 'stop' | 'restart' | 'copy' | 'open-logs' | 'exit'
export interface RequestProgress {
  requestId: string
  phase: string | null
  attempt: number | null
  elapsedMs: number | null
  sinceLastProgressMs?: number | null
}
export interface GatewaySnapshot {
  // null / 缺失表示当前核心未提供详情，不等于没有请求。
  requests?: RequestProgress[] | null
  requestsTruncated?: boolean | null
  state: GatewayState
  inflight: number | null
  uptime: string | null
  version: string | null
  // 实例归属（GLOSSARY）：当前窗口 | 外部进程 | 无。壳侧推导，始终有值。
  owner: 'window' | 'external' | 'none'
}
// 活动事件（GLOSSARY）：壳侧把英文诊断记录提炼成的分级中文事件。
// 对齐 Rust ActivityEvent（camelCase，最新在前）；可选字段兼容旧面板返回值。
// 级别 token 用 info|warn|error；中文级别（信息/警告/错误）由展示层映射。
export interface ActivityEvent {
  attempt?: number | null
  httpStatus?: number | null
  summary?: string
  requestId?: string | null
  code?: string
  technicalDetail?: string
  level: 'info' | 'warn' | 'error'
  title: string
  detail: string
  time: string
}
// 活动事件时间「展示层」格式化（纯函数，仅渲染时调用）：数据层始终保留原始 ISO8601 透传，
// 绝不改写。规则：今天 → HH:MM:SS；非今天 → MM-DD HH:MM（本地时区）。无法解析为合法日期
// （如浏览器 mock 的 "14:18:36" 或缺省空串）→ 原样返回，不破坏回退数据。
// 单一来源：总览「最近活动」与诊断日志页共用本助手，避免两处时间格式漂移。
export function formatEventTime(raw: string): string {
  const date = new Date(raw)
  if (Number.isNaN(date.getTime())) return raw
  const pad = (n: number) => String(n).padStart(2, '0')
  const hh = pad(date.getHours())
  const mm = pad(date.getMinutes())
  const now = new Date()
  const sameDay =
    date.getFullYear() === now.getFullYear() &&
    date.getMonth() === now.getMonth() &&
    date.getDate() === now.getDate()
  if (sameDay) return `${hh}:${mm}:${pad(date.getSeconds())}`
  return `${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${hh}:${mm}`
}
export interface ActivityFeed {
  revision: number
  refreshVersion: number
  status: 'connecting' | 'watching' | 'unavailable' | 'error'
  message: string | null
  error: string | null
  loading: boolean
  retry: () => void
}
export interface PageProps {
  endpoint: string
  snapshot: GatewaySnapshot
  events: ActivityEvent[]
  activity: ActivityFeed
  onAction: (action: GatewayAction) => void
  onNavigate: (page: PageId) => void
}
// 网关生命周期错误态（T7，ADR-0003 错误态）：由变更型命令的真实失败结果驱动，
// 在总览上作持久化、可操作的中文错误条呈现（非一次性吐司）。
// portConflict=true 另驱动大状态为「端口冲突」(error)；其余保持派生状态仅加错误条。
export interface GatewayErrorInfo {
  portConflict: boolean
  title: string
  detail: string
}
// 偏好设置页 props：目前与通用页 props 相同（开机自启由页面自管读写，退出走 onAction('exit')）。
export type SettingsPageProps = PageProps
export const ENDPOINT = 'http://127.0.0.1:8787'
// 应用版本的单一来源是动态状态（gateway.ts 的 getAppVersion()，回退 FALLBACK_APP_VERSION）；
// 侧栏/页脚/总览/设置页统一渲染该状态，显示层各自加 "v" 前缀——此处不再留静态版本常量以防分叉。
// 网关状态 → 中文短名（用于 App 全局 sr-only 实时播报）。
// external 刻意取「运行中（外部实例）」：与总览大标题「运行中」+「外部实例」徽标一致，
// 使屏幕阅读器播报与所见状态相符，并保留 ADR-0003 的外部实例醒目标记（a11y 亦不丢失）。
export const stateNames: Record<GatewayState, string> = {
  running: '运行中', stopped: '未启动', external: '运行中（外部实例）',
  error: '端口冲突', starting: '正在启动', draining: '正在排空',
}
