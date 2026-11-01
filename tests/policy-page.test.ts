import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import test from 'node:test'
import { createElement } from 'react'
import { renderToStaticMarkup } from 'react-dom/server'
import { PolicyPage } from '../src/pages/policy-page.tsx'
import { createPolicyEditor } from '../src/lib/policy-settings.ts'
import { moduleEnabled, moduleTiming, policyModules } from '../src/lib/policy-catalog.ts'
import type { GatewaySnapshot, PolicySettings } from '../src/lib/types.ts'

const defaults: PolicySettings = JSON.parse(readFileSync(new URL('./fixtures/policy-settings.json', import.meta.url), 'utf8'))
const snapshot: GatewaySnapshot = { state: 'running', owner: 'window', inflight: 1, uptime: '2 分钟', version: 'fixture', requests: [], requestsTruncated: false }

test('旧核心运行中不冒充策略已生效；停机显示下次启动；页面区分两组真实模块', async () => {
  Object.assign(globalThis, { __TAURI__: { core: { invoke: async () => ({ defaults, saved: defaults, active: null, activeRevision: null, activeSupported: false, restartRequired: false }) } } })
  try {
    const editor = createPolicyEditor()
    await editor.refresh()
    const render = (current = snapshot) => renderToStaticMarkup(createElement(PolicyPage, { editor, snapshot: current, onAction: () => {} }))
    const codex = render()
    assert.match(codex, /运行策略未核实/)
    assert.doesNotMatch(codex, /data-policy-status="applied"/)
    assert.match(codex, /首发可选编号清理/)
    assert.match(codex, /包含首次发送/)
    assert.match(codex, /关掉首发编号清理不等于禁止历史兼容降级删除编号/)
    assert.match(codex, /当前请求同类累计，不要求连续/)
    editor.editProfile('codex', { historyMode: 'always' })
    assert.match(render(), /每次首发完整预处理，不读写会话记忆/)
    assert.match(render(), /仅报错后模式参与/)
    editor.selectGroup('general')
    const general = render()
    assert.match(general, /空回应分级兼容/)
    assert.doesNotMatch(general, /data-policy-module="stripItemIds"/)
    assert.match(render({ ...snapshot, state: 'stopped', owner: 'none' }), /下次启动时应用/)
  } finally { Reflect.deleteProperty(globalThis, '__TAURI__') }
})

test('未覆盖错误保底沿用模块开关，区分范围、暂停与损失说明', async () => {
  Object.assign(globalThis, { __TAURI__: { core: { invoke: async () => ({ defaults, saved: defaults, active: defaults, activeRevision: 'fixture', activeSupported: true, restartRequired: false }) } } })
  try {
    const editor = createPolicyEditor()
    await editor.refresh()
    const module = policyModules.find(item => item.id === 'uncoveredErrorFallback')!
    assert.ok(module)
    assert.deepEqual(module.groups, ['codex', 'general'])
    const render = () => renderToStaticMarkup(createElement(PolicyPage, { editor, snapshot, onAction: () => {} }))
    for (const group of ['codex', 'general'] as const) {
      editor.selectGroup(group)
      assert.equal(defaults.profiles[group].uncoveredErrorFallback, true)
      let html = render()
      assert.match(html, /data-policy-module="uncoveredErrorFallback"/)
      assert.match(html, /aria-label="移除未覆盖错误保底"/)
      assert.match(html, /原样确认一次，再尝试一次有损清理/)
      assert.match(html, /候选请求最多实际发送 2 次/)
      assert.match(html, /受原次数与时间预算约束/)
      assert.match(html, /可能损失旧推理连续性/)
      assert.match(html, /正文、完整工具配对与参数\/结果、身份和缓存键/)
      assert.match(html, /不跨请求记忆/)
      assert.match(html, /未覆盖错误保底不使用此门槛/)
      assert.match(html, group === 'general' ? /本组仅 Claude 参与，ZCode 不参与/ : /仅明确识别为 Codex 的请求参与/)
      for (const historyMode of ['off', 'always'] as const) {
        editor.editProfile(group, { historyMode })
        const profile = editor.getSnapshot().draft!.profiles[group]
        assert.equal(profile.uncoveredErrorFallback, true)
        assert.equal(moduleEnabled(module, profile), true, '暂停不能从已选模块移除')
        assert.equal(moduleTiming(module, profile), '暂不参与，仅报错后模式参与')
        html = render()
        assert.match(html, /data-policy-module="uncoveredErrorFallback"/)
        assert.match(html, /未覆盖错误保底暂不参与；仅报错后模式参与，已保留开启选择/)
      }
      editor.editProfile(group, { historyMode: 'on_error' })
      assert.equal(moduleTiming(module, editor.getSnapshot().draft!.profiles[group]), '原样确认一次后保底')
      editor.editProfile(group, { uncoveredErrorFallback: false })
      assert.equal(moduleEnabled(module, editor.getSnapshot().draft!.profiles[group]), false)
      assert.equal(moduleTiming(module, editor.getSnapshot().draft!.profiles[group]), '已关闭')
      html = render()
      assert.doesNotMatch(html, /data-policy-module="uncoveredErrorFallback"/)
      assert.match(html, /未覆盖错误保底已关闭/)
      for (const historyMode of ['off', 'always', 'on_error'] as const) {
        editor.editProfile(group, { historyMode })
        assert.equal(editor.getSnapshot().draft!.profiles[group].uncoveredErrorFallback, false)
      }
      editor.editProfile(group, { uncoveredErrorFallback: true })
      assert.match(render(), /data-policy-module="uncoveredErrorFallback"/)
    }
  } finally { Reflect.deleteProperty(globalThis, '__TAURI__') }
})
