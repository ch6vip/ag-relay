import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import test from 'node:test'
import { createPolicyEditor, getPolicySettings, savePolicySettings } from '../src/lib/policy-settings.ts'
import type { PolicySettings } from '../src/lib/types.ts'

const defaults: PolicySettings = JSON.parse(readFileSync(new URL('./fixtures/policy-settings.json', import.meta.url), 'utf8'))

test('策略可通过桌面命令保存并回读，保存不重启正在运行的网关', async () => {
  let saved = structuredClone(defaults)
  const commands: string[] = []
  const view = () => ({ defaults, saved, active: defaults, activeRevision: 'fixture-baseline', restartRequired: saved.profiles.codex.maxAttempts !== 110, activeSupported: true })
  Object.assign(globalThis, { __TAURI__: { core: { invoke: async (command: string, args?: unknown) => {
    commands.push(command)
    if (command === 'get_policy_settings') { assert.equal(args, undefined); return structuredClone(view()) }
    if (command === 'save_policy_settings') {
      assert.deepEqual(Object.keys(args as object), ['settings'])
      saved = structuredClone((args as { settings: typeof defaults }).settings)
      return structuredClone(view())
    }
    throw new Error(`保存不能执行网关操作：${command}`)
  } } } })
  try {
    const first = await getPolicySettings()
    assert.equal(first.defaults.profiles.codex.maxAttempts, 110)
    const next = structuredClone(first.saved)
    next.profiles.codex.maxAttempts = 17
    const result = await savePolicySettings(next)
    assert.equal(result.restartRequired, true)
    assert.equal(result.active?.profiles.codex.maxAttempts, 110)
    assert.equal((await getPolicySettings()).saved.profiles.codex.maxAttempts, 17)
    assert.deepEqual(commands, ['get_policy_settings', 'save_policy_settings', 'get_policy_settings'])
  } finally { Reflect.deleteProperty(globalThis, '__TAURI__') }
})

test('草稿切组与刷新不丢失；恢复默认只填草稿，非法或失败的保存不改已保存设置', async () => {
  const backend = { defaults: structuredClone(defaults), saved: structuredClone(defaults), active: null, activeRevision: null, restartRequired: false, activeSupported: false }
  backend.defaults.profiles.general.maxAttempts = 90 // 证明恢复值来自后端，不从前端抄默认。
  let failSave = false
  let writes = 0
  Object.assign(globalThis, { __TAURI__: { core: { invoke: async (command: string, args?: { settings: typeof defaults }) => {
    if (command === 'get_policy_settings') return structuredClone(backend)
    if (command === 'save_policy_settings') {
      writes++
      if (failSave) throw new Error('模拟磁盘不可写')
      backend.saved = structuredClone(args!.settings)
      return structuredClone(backend)
    }
    throw new Error(command)
  } } } })
  try {
    const editor = createPolicyEditor()
    await editor.refresh()
    editor.editProfile('codex', { maxAttempts: '' })
    editor.selectGroup('general')
    await editor.refresh()
    editor.selectGroup('codex')
    assert.equal(editor.getSnapshot().draft?.profiles.codex.maxAttempts, '')
    await editor.save()
    assert.equal(writes, 0)
    assert.match(editor.getSnapshot().validation ?? '', /次数/)
    editor.editProfile('codex', { maxAttempts: '31' })
    editor.reset('general')
    assert.equal(editor.getSnapshot().draft?.profiles.general.maxAttempts, '90')
    assert.equal(editor.getSnapshot().draft?.profiles.codex.maxAttempts, '31')
    assert.equal(backend.saved.profiles.general.maxAttempts, 110)
    failSave = true
    await editor.save()
    await editor.refresh() // 后台读取成功不能吞掉保存失败的原因。
    assert.match(editor.getSnapshot().error ?? '', /磁盘不可写/)
    assert.equal(editor.getSnapshot().draft?.profiles.codex.maxAttempts, '31')
    assert.equal(editor.getSnapshot().view?.saved.profiles.codex.maxAttempts, 110)
    failSave = false
    await editor.save()
    assert.equal(editor.getSnapshot().dirty, false)
    assert.equal((await getPolicySettings()).saved.profiles.codex.maxAttempts, 31)
    editor.editProfile('general', { maxAttempts: '42' })
    editor.discard()
    assert.equal(editor.getSnapshot().draft?.profiles.general.maxAttempts, '90')
  } finally { Reflect.deleteProperty(globalThis, '__TAURI__') }
})

test('保存拒绝非法整数、时间与依赖关系，合法边界值可透传', async () => {
  let writes = 0
  Object.assign(globalThis, { __TAURI__: { core: { invoke: async (_command: string, args: { settings: typeof defaults }) => {
    writes++
    return { defaults, saved: args.settings, active: null, activeRevision: null, activeSupported: false, restartRequired: false }
  } } } })
  try {
    const invalid = [
      ['codex', { maxAttempts: 0 }], ['general', { maxAttempts: 1001 }], ['codex', { maxAttempts: 1.5 }],
      ['codex', { baseDelayMs: 0 }], ['general', { maxDelayMs: 300001 }], ['codex', { baseDelayMs: 1500 }],
      ['codex', { totalBudgetMs: 999 }], ['general', { totalBudgetMs: 3600001 }],
      ['codex', { connectTimeoutMs: 999 }], ['general', { connectTimeoutMs: 600001 }],
      ['codex', { totalBudgetMs: 1000, connectTimeoutMs: 2000 }],
      ['codex', { historyConfirmations: 5 }], ['general', { maxAttempts: 2, historyConfirmations: 3 }],
      ['general', { emptyResponseNormalizeAfter: 6, emptyResponseReasoningAfter: 6 }],
      ['general', { emptyResponseRepair: false, emptyResponseDropReasoning: true }],
      ['codex', { emptyResponseRepair: true }], ['general', { stripItemIds: true }],
    ] as const
    for (const [group, patch] of invalid) {
      const next = structuredClone(defaults)
      Object.assign(next.profiles[group], patch)
      await assert.rejects(() => savePolicySettings(next))
    }
    assert.equal(writes, 0)
    const valid = structuredClone(defaults)
    Object.assign(valid.profiles.codex, { maxAttempts: 1000, historyConfirmations: 4, baseDelayMs: 1, maxDelayMs: 300000, totalBudgetMs: 3600000, connectTimeoutMs: 600000 })
    Object.assign(valid.profiles.general, { maxAttempts: 1000, historyConfirmations: 1000 })
    assert.deepEqual((await savePolicySettings(valid)).saved, valid)
    assert.equal(writes, 1)
  } finally { Reflect.deleteProperty(globalThis, '__TAURI__') }
})

test('秒输入精确换算整数毫秒，留空或更细小数不能保存', async () => {
  let received = structuredClone(defaults)
  const view = () => ({ defaults, saved: received, active: null, activeRevision: null, activeSupported: false, restartRequired: false })
  Object.assign(globalThis, { __TAURI__: { core: { invoke: async (command: string, args?: { settings: typeof defaults }) => {
    if (command === 'save_policy_settings') received = structuredClone(args!.settings)
    return structuredClone(view())
  } } } })
  try {
    const editor = createPolicyEditor()
    await editor.refresh()
    assert.equal(editor.getSnapshot().draft?.profiles.codex.baseDelayMs, '0.5')
    editor.editProfile('codex', { baseDelayMs: '1.001', maxDelayMs: '2' })
    await editor.save()
    assert.equal(received.profiles.codex.baseDelayMs, 1001)
    editor.editProfile('codex', { baseDelayMs: '0.0001' })
    await editor.save()
    assert.equal(received.profiles.codex.baseDelayMs, 1001)
    assert.match(editor.getSnapshot().validation ?? '', /整数毫秒/)
    editor.editProfile('codex', { baseDelayMs: '' })
    assert.match(editor.getSnapshot().validation ?? '', /等待/)
  } finally { Reflect.deleteProperty(globalThis, '__TAURI__') }
})

test('未覆盖错误保底独立保存并回读，只有新开关不同也保留待重启回执', async () => {
  let saved = structuredClone(defaults)
  const commands: string[] = []
  const view = () => ({ defaults, saved, active: defaults, activeRevision: 'fixture-baseline', restartRequired: JSON.stringify(saved) !== JSON.stringify(defaults), activeSupported: true })
  Object.assign(globalThis, { __TAURI__: { core: { invoke: async (command: string, args?: { settings: PolicySettings }) => {
    commands.push(command)
    if (command === 'save_policy_settings') saved = structuredClone(args!.settings)
    else assert.equal(command, 'get_policy_settings', '保存不能重启网关')
    return structuredClone(view())
  } } } })
  try {
    for (const group of ['codex', 'general'] as const) {
      const next = structuredClone(defaults)
      next.profiles[group].uncoveredErrorFallback = false
      const result = await savePolicySettings(next)
      assert.equal(result.restartRequired, true)
      assert.equal(result.active?.profiles[group].uncoveredErrorFallback, true)
      assert.deepEqual(result.saved, next)
      assert.deepEqual((await getPolicySettings()).saved, next)
    }
    assert.deepEqual(commands, ['save_policy_settings', 'get_policy_settings', 'save_policy_settings', 'get_policy_settings'])
  } finally { Reflect.deleteProperty(globalThis, '__TAURI__') }
})

test('未覆盖错误保底必须为布尔值，缺失或错类型在调用保存前拒绝', async () => {
  let writes = 0
  Object.assign(globalThis, { __TAURI__: { core: { invoke: async () => { writes++; throw new Error('不应调用后端') } } } })
  try {
    for (const group of ['codex', 'general'] as const) {
      for (const value of [null, 'false', 0, [], {}, undefined]) {
        const next = structuredClone(defaults)
        if (value === undefined) Reflect.deleteProperty(next.profiles[group], 'uncoveredErrorFallback')
        else Object.assign(next.profiles[group], { uncoveredErrorFallback: value })
        await assert.rejects(() => savePolicySettings(next), /模块开关无效：uncoveredErrorFallback/)
      }
    }
    assert.equal(writes, 0)
  } finally { Reflect.deleteProperty(globalThis, '__TAURI__') }
})

test('历史模式暂停保底但保留选择，恢复默认取后端而非前端副本', async () => {
  const backend = { defaults: structuredClone(defaults), saved: structuredClone(defaults), active: null, activeRevision: null, restartRequired: false, activeSupported: false }
  backend.defaults.profiles.codex.uncoveredErrorFallback = false
  Object.assign(globalThis, { __TAURI__: { core: { invoke: async (command: string, args?: { settings: PolicySettings }) => {
    if (command === 'save_policy_settings') backend.saved = structuredClone(args!.settings)
    else assert.equal(command, 'get_policy_settings')
    return structuredClone(backend)
  } } } })
  try {
    const editor = createPolicyEditor()
    await editor.refresh()
    for (const group of ['codex', 'general'] as const) {
      for (const enabled of [true, false]) {
        editor.editProfile(group, { uncoveredErrorFallback: enabled })
        for (const historyMode of ['off', 'always', 'on_error'] as const) {
          editor.editProfile(group, { historyMode })
          assert.equal(editor.getSnapshot().validation, null)
          await editor.save()
          await editor.refresh()
          assert.equal(editor.getSnapshot().draft?.profiles[group].uncoveredErrorFallback, enabled)
          assert.equal(editor.getSnapshot().draft?.profiles[group].historyMode, historyMode)
          assert.equal(backend.saved.profiles[group].uncoveredErrorFallback, enabled)
        }
      }
    }
    editor.editProfile('codex', { uncoveredErrorFallback: true })
    editor.reset('codex')
    assert.equal(editor.getSnapshot().draft?.profiles.codex.uncoveredErrorFallback, false)
    assert.equal(editor.getSnapshot().draft?.profiles.general.uncoveredErrorFallback, false)
    editor.reset()
    assert.equal(editor.getSnapshot().draft?.profiles.general.uncoveredErrorFallback, true)
    assert.equal(backend.saved.profiles.general.uncoveredErrorFallback, false, '恢复默认不能先保存')
  } finally { Reflect.deleteProperty(globalThis, '__TAURI__') }
})
