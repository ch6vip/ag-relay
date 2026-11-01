import assert from 'node:assert/strict'
import test from 'node:test'
import { createElement } from 'react'
import { renderToStaticMarkup } from 'react-dom/server'
import { RequestProgressCard } from '../src/components/request-progress-card.tsx'
import { deriveSnapshot } from '../src/lib/gateway.ts'
import type { GatewaySnapshot } from '../src/lib/types.ts'

const running: GatewaySnapshot = {
  state: 'running', owner: 'window', inflight: 1, uptime: '1分钟', version: 'abcd1234',
  requests: [{ requestId: 'fixture-001', phase: 'waiting_first_content', attempt: 3, elapsedMs: 65000, sinceLastProgressMs: null }],
}
const render = (snapshot: GatewaySnapshot) => renderToStaticMarkup(createElement(RequestProgressCard, { snapshot }))

test('请求进展展示真实耗时与尝试，尚无内容不伪造进展或预计完成时间', () => {
  const html = render(running)
  assert.match(html, /等待首段内容/)
  assert.match(html, /1分5秒/)
  assert.match(html, /第 3 次尝试/)
  assert.match(html, /尚未收到内容进展/)
  assert.doesNotMatch(html, /已卡死|预计.*\d.*完成|倒计时/)
})

test('等待重试说明是网关等待，保留实际尝试次数且不显示倒计时', () => {
  const html = render({ ...running, requests: [{ ...running.requests![0], phase: 'retry_waiting' }] })
  assert.match(html, /等待重试/)
  assert.match(html, /网关正在等待后再试，不是上游正在生成内容；下一次尝试尚未发出。/)
  assert.match(html, /第 3 次尝试/)
  assert.match(html, /1分5秒/)
  assert.doesNotMatch(html, /第 4 次尝试|倒计时|剩余[^<>]*\d[^<>]*秒|预计[^<>]*\d[^<>]*完成/)
  assert.equal((html.match(/<th\b/g) ?? []).length, 6, '沿用既有表格列')
  const next = render({ ...running, requests: [{ ...running.requests![0], phase: 'waiting_headers', attempt: 4 }] })
  assert.match(next, /等待上游响应/)
  assert.match(next, /第 4 次尝试/)
  assert.doesNotMatch(next, /等待重试/)
})

test('旧核心无详情和离线不误报空闲，未知阶段诚实回退', () => {
  assert.match(render({ ...running, requests: null }), /暂未提供请求详情/)
  assert.match(render({ ...running, state: 'stopped' }), /网关未连接/)
  assert.match(render({ ...running, requests: [{ ...running.requests![0], phase: 'future_phase' }] }), /future_phase/)
  assert.match(render({ ...running, inflight: 0, requests: [] }), /暂无进行中的请求/)
  assert.match(render({ ...running, requestsTruncated: true }), /仅展示/)
})


test('缺失进展时间不等于尚无进展，工具调用不误报为已执行', () => {
  const unknown = render({ ...running, requests: [{ ...running.requests![0], sinceLastProgressMs: undefined }] })
  assert.match(unknown, /内容进展时间未知/)
  assert.doesNotMatch(unknown, /尚未收到内容进展/)
  const tool = render({ ...running, requests: [{ ...running.requests![0], phase: 'tool_output', sinceLastProgressMs: 500 }] })
  assert.match(tool, /不代表工具已执行完成/)
  assert.match(tool, /距上次内容进展 不足1秒/)
})


test('桌面状态接线保留请求详情，离线时清除旧进展', () => {
  const status = { available: true, activeRequests: 1, uptimeText: '1分钟', version: 'fixture', owner: 'window' as const, requests: running.requests, requestsTruncated: true }
  assert.deepEqual(deriveSnapshot(status).requests, running.requests)
  assert.equal(deriveSnapshot(status).requestsTruncated, true)
  assert.equal(deriveSnapshot({ ...status, available: false }).requests, null)
  assert.equal(deriveSnapshot({ ...status, requests: undefined }).requests, null)
})

test('请求明细使用紧凑表格，一条请求对应一行', () => {
  const html = render(running)
  assert.match(html, /<table/)
  for (const heading of ['请求', '最近阶段', '已用时', '尝试', '距上次进展']) assert.match(html, new RegExp(heading))
  assert.equal((html.match(/<tbody/g) ?? []).length, 1)
  assert.doesNotMatch(html, /<li/)
})
