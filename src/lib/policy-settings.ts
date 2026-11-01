import { invoke } from '@/lib/gateway'
import type { PolicyGroup, PolicyProfile, PolicySettings, PolicySettingsView } from '@/lib/types'

// 默认值只来自桌面命令。普通浏览器请使用 tests 中的隔离模拟桥，不引入生产默认副本。
export function getPolicySettings(): Promise<PolicySettingsView> {
  return invoke<PolicySettingsView>('get_policy_settings')
}

export function savePolicySettings(settings: PolicySettings): Promise<PolicySettingsView> {
  const error = policySettingsError(settings)
  if (error) return Promise.reject(new Error(error))
  return invoke<PolicySettingsView>('save_policy_settings', { settings })
}

export const policyGroupNames: Record<PolicyGroup, string> = { codex: 'Codex', general: 'Claude / Zcode' }
const numberFields = ['maxAttempts', 'baseDelayMs', 'maxDelayMs', 'totalBudgetMs', 'connectTimeoutMs', 'historyConfirmations', 'emptyResponseNormalizeAfter', 'emptyResponseReasoningAfter'] as const
export type PolicyNumberField = typeof numberFields[number]
const timeFields = new Set<PolicyNumberField>(['baseDelayMs', 'maxDelayMs', 'totalBudgetMs', 'connectTimeoutMs'])
// 时间在草稿中以“秒”编辑，提交时精确换成整数毫秒；其余数值以次数编辑。
export type PolicyDraftProfile = Omit<PolicyProfile, PolicyNumberField> & Record<PolicyNumberField, string>
export interface PolicyDraft { schemaVersion: 1; profiles: Record<PolicyGroup, PolicyDraftProfile> }

function toDraft(settings: PolicySettings): PolicyDraft {
  const profile = (value: PolicyProfile): PolicyDraftProfile => {
    const numeric = Object.fromEntries(numberFields.map(key => [key, String(value[key] / (timeFields.has(key) ? 1000 : 1))])) as Record<PolicyNumberField, string>
    return { ...value, ...numeric }
  }
  return { schemaVersion: 1, profiles: { codex: profile(settings.profiles.codex), general: profile(settings.profiles.general) } }
}

function fromDraft(draft: PolicyDraft): PolicySettings {
  const profile = (value: PolicyDraftProfile): PolicyProfile => {
    const numeric = Object.fromEntries(numberFields.map(key => {
      const raw = value[key].trim()
      if (!timeFields.has(key)) return [key, /^\d+$/.test(raw) ? Number(raw) : NaN]
      if (!/^\d+(?:\.\d{1,3})?$/.test(raw)) return [key, NaN]
      const [seconds, fraction = ''] = raw.split('.')
      return [key, Number(seconds) * 1000 + Number(fraction.padEnd(3, '0'))]
    })) as Record<PolicyNumberField, number>
    return { ...value, ...numeric }
  }
  return { schemaVersion: 1, profiles: { codex: profile(draft.profiles.codex), general: profile(draft.profiles.general) } }
}

/** 保存桥与草稿即时校验共用规则，数字限制不散落在控件里。 */
export function policySettingsError(settings: PolicySettings): string | null {
  if (settings.schemaVersion !== 1) return '不支持此策略版本，请更新桌面面板。'
  for (const group of ['codex', 'general'] as const) {
    const p = settings.profiles[group]
    const prefix = `${policyGroupNames[group]}：`
    const integer = (n: number, min: number, max: number) => Number.isInteger(n) && n >= min && n <= max
    if (!integer(p.maxAttempts, 1, 1000)) return `${prefix}常规尝试次数须为 1–1000 的整数（含首次发送）。`
    if (!integer(p.baseDelayMs, 1, 300000) || !integer(p.maxDelayMs, 1, 300000)) return `${prefix}等待须为 0.001–300 秒，最多三位小数（整数毫秒）。`
    if (p.baseDelayMs > p.maxDelayMs) return `${prefix}起始等待不能大于最长等待。`
    if (!integer(p.totalBudgetMs, 1000, 3600000)) return `${prefix}总时间预算须为 1–3600 秒，最多三位小数。`
    if (!integer(p.connectTimeoutMs, 1000, 600000)) return `${prefix}单次等待上限须为 1–600 秒，最多三位小数。`
    if (p.connectTimeoutMs > p.totalBudgetMs) return `${prefix}单次等待上限不能大于总时间预算。`
    if (!['off', 'on_error', 'always'].includes(p.historyMode)) return `${prefix}历史兼容处理时机无效。`
    if (!integer(p.historyConfirmations, 1, group === 'codex' ? 4 : 1000)) return `${prefix}历史错误确认次数须为 1–${group === 'codex' ? 4 : 1000} 的整数。`
    if (p.historyConfirmations > p.maxAttempts) return `${prefix}历史错误确认次数不能大于常规尝试次数。`
    if (!integer(p.emptyResponseNormalizeAfter, 1, 1000) || !integer(p.emptyResponseReasoningAfter, 1, 1000)) return `${prefix}空回应门槛须为 1–1000 的整数。`
    if (p.emptyResponseReasoningAfter <= p.emptyResponseNormalizeAfter) return `${prefix}空回应第二阶段门槛必须晚于第一阶段。`
    if (p.emptyResponseDropReasoning && !p.emptyResponseRepair) return `${prefix}空回应第二阶段依赖第一阶段，请先启用字段规范化。`
    for (const key of ['uncoveredErrorFallback', 'stripItemIds', 'rememberSession', 'repairStatus', 'retryHistory', 'emptyResponseRepair', 'emptyResponseDropReasoning'] as const) {
      if (typeof p[key] !== 'boolean') return `${prefix}模块开关无效：${key}。`
    }
    if (group === 'codex' && (p.emptyResponseRepair || p.emptyResponseDropReasoning)) return `${prefix}不支持通用组的空回应修复。`
    if (group === 'general' && (p.stripItemIds || p.retryHistory)) return `${prefix}不支持 Codex 专属的编号清理或原样再试。`
  }
  return null
}

export interface PolicyEditorState {
  view: PolicySettingsView | null
  draft: PolicyDraft | null
  group: PolicyGroup
  loading: boolean
  working: boolean
  dirty: boolean
  validation: string | null
  error: string | null
  errorSource: 'read' | 'save' | 'validation' | null
  notice: string | null
}

/** App 持有编辑会话；页面卸载只移除订阅，不销毁草稿。后台读取从不替换现有草稿。 */
export function createPolicyEditor() {
  let state: PolicyEditorState = { view: null, draft: null, group: 'codex', loading: false, working: false, dirty: false, validation: null, error: null, errorSource: null, notice: null }
  let readVersion = 0
  const listeners = new Set<() => void>()
  const update = (patch: Partial<PolicyEditorState>) => {
    state = { ...state, ...patch }
    state.validation = state.draft ? policySettingsError(fromDraft(state.draft)) : null
    state.dirty = Boolean(state.draft && state.view && JSON.stringify(state.draft) !== JSON.stringify(toDraft(state.view.saved)))
    listeners.forEach(listener => listener())
  }
  return {
    getSnapshot: () => state,
    subscribe: (listener: () => void) => { listeners.add(listener); return () => { listeners.delete(listener) } },
    selectGroup: (group: PolicyGroup) => update({ group }),
    editProfile: (group: PolicyGroup, patch: Partial<PolicyDraftProfile>) => {
      if (!state.draft || state.working) return
      update({ draft: { ...state.draft, profiles: { ...state.draft.profiles, [group]: { ...state.draft.profiles[group], ...patch } } }, notice: null, error: null })
    },
    reset: (group?: PolicyGroup) => {
      if (!state.view || !state.draft || state.working) return
      const defaults = toDraft(state.view.defaults)
      update({ draft: group ? { ...state.draft, profiles: { ...state.draft.profiles, [group]: defaults.profiles[group] } } : defaults, error: null, notice: `${group ? policyGroupNames[group] : '全部策略'}默认值已填入草稿；保存后才会使用。连接、端口、自启和外观未改动。` })
    },
    discard: () => {
      if (state.view && !state.working) update({ draft: toDraft(state.view.saved), error: null, notice: '未保存的修改已放弃，回到最近读取的已保存策略。' })
    },
    refresh: async () => {
      if (state.working || state.loading) return
      const version = ++readVersion
      update({ loading: true })
      try {
        const view = await getPolicySettings()
        if (version === readVersion) update({ view, draft: state.draft ?? toDraft(view.saved), error: state.errorSource === 'read' ? null : state.error })
      } catch (error) {
        if (version === readVersion) update({ errorSource: 'read', error: `无法读取策略：${String(error instanceof Error ? error.message : error)}。不会自动恢复默认；请检查设置文件后重试。` })
      } finally { if (version === readVersion) update({ loading: false }) }
    },
    save: async () => {
      if (!state.draft || state.working) return
      if (state.validation) { update({ error: state.validation, errorSource: 'validation' }); return }
      ++readVersion // 保存前发出的旧读取结果不能覆盖新保存结果。
      const settings = fromDraft(state.draft)
      update({ working: true, loading: false, error: null, notice: null })
      try {
        const view = await savePolicySettings(settings)
        update({ view, draft: toDraft(view.saved), notice: '策略已保存；没有重启网关，也没有打断当前请求。' })
      } catch (error) {
        update({ errorSource: 'save', error: `保存失败：${String(error instanceof Error ? error.message : error)}。草稿仍保留，请稍后重试。` })
      } finally { update({ working: false }) }
    },
  }
}
export type PolicyEditor = ReturnType<typeof createPolicyEditor>
