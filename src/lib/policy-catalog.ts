import type { PolicyGroup, PolicyProfile } from '@/lib/types'
import type { PolicyDraftProfile } from '@/lib/policy-settings'

export type PolicyModuleId = 'stripItemIds' | 'historyMode' | 'uncoveredErrorFallback' | 'rememberSession' | 'repairStatus' | 'emptyResponseRepair' | 'retryHistory'
export interface PolicyModule {
  id: PolicyModuleId
  title: string
  fields: string
  purpose: string
  trigger: string
  preserves: string
  impact: string
  groups: PolicyGroup[]
}

// 仅描述已存在的处理能力；开关默认值从 PolicySettingsView.defaults 读取。
export const policyModules: PolicyModule[] = [
  {
    id: 'stripItemIds', title: '首发可选编号清理', fields: 'item.id', groups: ['codex'],
    purpose: '在首次发送前清理部分历史条目的可选编号，减少旧编号无法识别的问题。',
    trigger: '提前执行；含搜索时保留其所需 reasoning 编号，不是无差别删除所有 id。',
    preserves: '正文、推理密文、工具调用与返回的配对关系。',
    impact: '部分条目不再带原资源编号。关掉首发编号清理不等于禁止历史兼容降级删除编号。',
  },
  {
    id: 'historyMode', title: '历史兼容降级', fields: 'reasoning / 资源编号', groups: ['codex', 'general'],
    purpose: '移除无法继续使用的旧推理与适用编号，让同一段对话有机会继续。',
    trigger: '默认等明确历史错误达到门槛；可改为报错前就尝试处理。',
    preserves: '可验证的正文与工具配对；遇到无法安全改写的历史，整组保留，不半剥离。',
    impact: '可能丢失旧推理连续性及资源关联。只改转发副本，不删除本地聊天记录，也不关闭模型本轮思考。',
  },
  {
    id: 'uncoveredErrorFallback', title: '未覆盖错误保底', fields: 'reasoning / 适用编号', groups: ['codex', 'general'],
    purpose: '遇到现有规则未覆盖的错误时，原样确认一次，再尝试一次有损清理，让同一段对话有机会继续。',
    trigger: '仅明确识别的 Claude / Codex Responses 请求，且历史模式为“报错后”时参与；ZCode、未知或冲突身份不参与。两次连续合格的未覆盖错误才尝试清理，明确限流、鉴权等错误不触发。',
    preserves: '正文、完整工具配对与参数/结果、身份和缓存键（prompt_cache_key）；不修改本地聊天记录。',
    impact: '可能损失旧推理连续性。清理后的候选请求最多实际发送 2 次，仍受原次数与时间预算约束，不追加预算、不跨请求记忆；不能安全清理就保留当前错误。',
  },
  {
    id: 'rememberSession', title: '复用会话兼容处理', fields: 'compatibility memory', groups: ['codex', 'general'],
    purpose: '同一会话曾明确报错并生成可用的兼容请求，后续可提前使用同组恢复。',
    trigger: '只在“报错后”模式复用本组记忆；历史模块关闭或提前模式下暂不参与，不绕过关闭项。',
    preserves: '两组缓存隔离；提前模式不读写会话记忆。',
    impact: '后续可能提前省略旧推理。记忆不代表上游已成功接收；有效期 24 小时、容量 4096，重启清空。',
  },
  {
    id: 'repairStatus', title: '定向状态字段修复', fields: 'input[i].status', groups: ['codex', 'general'],
    purpose: '只删上游点名不支持的那一个状态字段，不把整个历史抹掉重来。',
    trigger: '必须有明确的条目位置与 status 错误；不提供提前处理。',
    preserves: '正文、其他条目的状态、嵌套工具数据。',
    impact: '该条目的状态标记会省略；关掉后不再针对这一错误修复。',
  },
  {
    id: 'emptyResponseRepair', title: '空回应分级兼容', fields: 'id / type → reasoning', groups: ['general'],
    purpose: '回复没有可交付内容时，先整理字段，再按需允许省略旧推理。',
    trigger: '第一阶段规范化 id/type；第二阶段必须晚于第一阶段，且第一阶段确实改过。真正 limit 错误不触发。',
    preserves: '第二阶段可单独关闭；没有改过字段就不会跳级删除旧推理。',
    impact: '第一阶段改变字段表达；第二阶段可能损失旧推理连续性。不提供“提前空回应修复”。',
  },
  {
    id: 'retryHistory', title: '无法剥离时保留历史再试', fields: '原样历史重试', groups: ['codex'],
    purpose: '历史不能安全改写时保留原内容再试，不以删除未知内容换取表面成功。',
    trigger: '明确历史错误，但无法生成安全的兼容请求；仍消耗次数和时间预算。',
    preserves: '本次原始历史、服务端引用及未知内容。',
    impact: '可能多等几次仍报错；关闭后不再走这条保留历史重试路径。',
  },
]

export function moduleEnabled(module: PolicyModule, profile: PolicyProfile | PolicyDraftProfile): boolean {
  return module.id === 'historyMode' ? profile.historyMode !== 'off' : profile[module.id]
}

export function moduleTiming(module: PolicyModule, profile: PolicyProfile | PolicyDraftProfile): string {
  if (!moduleEnabled(module, profile)) return '已关闭'
  if (module.id === 'historyMode') return profile.historyMode === 'always' ? '提前处理' : '报错后'
  if (module.id === 'stripItemIds') return '提前整理'
  if (module.id === 'rememberSession') return profile.historyMode === 'on_error' ? '按会话记忆' : '暂不参与，仅报错后模式参与'
  if (module.id === 'uncoveredErrorFallback') return profile.historyMode === 'on_error' ? '原样确认一次后保底' : '暂不参与，仅报错后模式参与'
  if (module.id === 'emptyResponseRepair') return '空回应后分级'
  return '报错后'
}
