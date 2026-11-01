// 隔离验证真实核心：随机回环端口、临时日志、假上游与假凭据，不依赖 Tauri 构建。
import assert from 'node:assert/strict'
import test from 'node:test'
import http from 'node:http'
import { spawn } from 'node:child_process'
import { once } from 'node:events'
import { mkdirSync, mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import { setTimeout as delay } from 'node:timers/promises'

const core = resolve('src-tauri/resources/gateway/relay-retry-proxy.mjs')
const runtime = process.platform === 'win32'
  ? resolve('src-tauri/binaries/node-x86_64-pc-windows-msvc.exe') : process.execPath
const missingReasoning = "Item 'ws_fixture' of type 'web_search_call' was provided without its required 'reasoning' item: 'rs_fixture'. [trace_id=fixture]"
const success = { status: 200, body: { ok: true } }
const history = {
  model: 'fixture-model', store: false,
  input: [
    { type: 'message', id: 'msg_fixture', role: 'user', content: 'fixture prompt' },
    { type: 'reasoning', id: 'rs_fixture', summary: [], encrypted_content: 'fixture ciphertext' },
    { type: 'web_search_call', id: 'ws_fixture', status: 'completed', action: { type: 'search', query: 'fixture query' } },
  ],
}
const expectedHistory = {
  model: 'fixture-model', store: false,
  input: [
    { type: 'message', role: 'user', content: 'fixture prompt' },
    { type: 'reasoning', id: 'rs_fixture', summary: [], encrypted_content: 'fixture ciphertext' },
    { type: 'web_search_call', id: 'ws_fixture', status: 'completed', action: { type: 'search', query: 'fixture query' } },
  ],
}

const recoveredHistory = {
  model: 'fixture-model', store: false,
  input: [
    { type: 'message', role: 'user', content: 'fixture prompt' },
    { type: 'web_search_call', status: 'completed', action: { type: 'search', query: 'fixture query' } },
  ],
}

async function listen(server) {
  server.listen(0, '127.0.0.1')
  await once(server, 'listening')
  return server.address().port
}

async function fixture(t, respond, config = {}, script = core) {
  const dir = mkdtempSync(join(tmpdir(), 'ag-relay-history-'))
  const diagnostics = join(dir, 'diagnostics.jsonl')
  const captured = []
  let upstreamError
  const upstream = http.createServer(async (req, res) => {
    try {
      const chunks = []
      for await (const chunk of req) chunks.push(chunk)
      const raw = Buffer.concat(chunks).toString('utf8')
      const body = JSON.parse(raw)
      captured.push({ raw, body, headers: { ...req.headers } })
      const reply = await respond(body, captured.length, req, res)
      if (reply === undefined) return // 流式用例自行控制分包和结束时机。
      res.writeHead(reply.status, { 'content-type': reply.type ?? 'application/json', connection: 'close' })
      res.end(typeof reply.body === 'string' ? reply.body : JSON.stringify(reply.body))
    } catch (error) {
      upstreamError = error
      res.writeHead(500).end()
    }
  })
  let child
  let closed
  t.after(async () => {
    try {
      if (child && child.exitCode === null && child.signalCode === null) {
        if (child.connected) child.send({ type: 'shutdown' })
        await Promise.race([closed, delay(1500)])
        // 只终止本测试持有的子进程，不按端口/进程名清理。
        if (child.exitCode === null && child.signalCode === null) child.kill()
        await Promise.race([closed, delay(3000).then(() => { throw new Error('隔离核心未退出') })])
      }
    } finally {
      upstream.closeAllConnections()
      await new Promise(resolve => upstream.close(resolve))
      if (!child || child.exitCode !== null || child.signalCode !== null) {
        rmSync(dir, { recursive: true, force: true, maxRetries: 5, retryDelay: 50 })
      }
    }
    if (upstreamError) throw upstreamError
  })
  const upstreamPort = await listen(upstream)
  const reservation = http.createServer()
  const port = await listen(reservation)
  await new Promise(resolve => reservation.close(resolve))
  assert.notEqual(port, 8787)
  const env = Object.fromEntries(Object.entries(process.env).filter(([key]) => /^(SystemRoot|WINDIR|TEMP|TMP)$/i.test(key)))
  Object.assign(env, {
    PROXY_HOST: '127.0.0.1', PROXY_PORT: String(port), PROXY_UPSTREAM: `http://127.0.0.1:${upstreamPort}`,
    PROXY_DIAGNOSTICS_FILE: diagnostics, PROXY_VERBOSE: '0', PROXY_SUPERVISED: '1',
    PROXY_MAX_ATTEMPTS: '5', PROXY_DEPLOY_MAX_ATTEMPTS: '5', PROXY_BASE_DELAY_MS: '10',
    PROXY_MAX_DELAY_MS: '10', PROXY_DEPLOY_DELAY_MS: '10', PROXY_TOTAL_BUDGET_MS: '10000',
    PROXY_MIN_RETRY_WINDOW_MS: '100', PROXY_CONNECT_TIMEOUT_MS: '2000', ...config,
  })
  child = spawn(runtime, [script], { cwd: dir, env, windowsHide: true, stdio: ['ignore', 'ignore', 'pipe', 'ipc'] })
  closed = new Promise(resolve => child.once('close', resolve))
  let stderr = ''
  child.stderr.on('data', chunk => { stderr = (stderr + chunk).slice(-4096) })
  await new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error(`隔离核心就绪超时：${stderr}`)), 5000)
    child.once('error', error => { clearTimeout(timer); reject(error) })
    child.once('exit', code => { clearTimeout(timer); reject(new Error(`隔离核心退出 ${code}：${stderr}`)) })
    child.once('message', message => {
      clearTimeout(timer)
      if (message.type === 'ready') resolve()
      else reject(new Error('隔离核心未报告 ready'))
    })
  })
  // 收到自己启动的核心 ready 后才访问端口；绑定失败绝不探测可能占位的其他实例。
  const url = `http://127.0.0.1:${port}`
  const status = await (await fetch(`${url}/_relay/status`)).json()
  assert.equal(status.pid, child.pid)
  return {
    url,
    captured,
    diagnostics,
    rows: () => readFileSync(diagnostics, 'utf8').trim().split(/\r?\n/).map(line => JSON.parse(line)),
    async request(body = history, headers = {}, options = {}) {
      const response = await fetch(`${url}${options.path ?? '/v1/responses'}`, {
        method: 'POST', signal: options.signal ?? AbortSignal.timeout(12000),
        headers: { authorization: 'Bearer fixture-only-not-a-key', 'content-type': 'application/json', 'x-relay-client': 'codex', ...headers },
        body: typeof body === 'string' ? body : JSON.stringify(body),
      })
      return { status: response.status, body: await response.text() }
    },
  }
}

// 仅对本测试已知的 ws_fixture → rs_fixture 配对校验，不假装能从任意历史推断隐式依赖。
function validateSearch(body) {
  const hasSearch = body.input.some(item => item.type === 'web_search_call' && item.id === 'ws_fixture')
  const hasReasoning = body.input.some(item => item.type === 'reasoning' && item.id === 'rs_fixture')
  return hasSearch && !hasReasoning
    ? { status: 400, body: { error: { type: 'invalid_request_error', message: missingReasoning } } }
    : success
}

test('Codex 网页搜索首发保留配套推理及编号', async t => {
  const f = await fixture(t, validateSearch)
  const response = await f.request()
  assert.equal(response.status, 200, response.body)
  assert.equal(f.captured.length, 1)
  assert.deepEqual(f.captured[0].body, expectedHistory)
})

function historyError(message, carrier = 'http', code) {
  const body = { error: { type: 'invalid_request_error', message, ...(code ? { code } : {}) } }
  return {
    status: carrier === 'http' ? 400 : 200,
    type: carrier === 'sse' ? 'text/event-stream' : 'application/json',
    body: carrier === 'sse' ? `data: ${JSON.stringify(body)}\n\n` : body,
  }
}

test('Codex 首次明确缺推理错误后移除旧推理与搜索编号', async t => {
  const f = await fixture(t, body => body.input.some(item => item.type === 'reasoning' || Object.hasOwn(item, 'id'))
    ? historyError(missingReasoning) : success)
  const response = await f.request()
  assert.equal(response.status, 200, response.body)
  assert.equal(f.captured.length, 2)
  assert.deepEqual(f.captured[0].body, expectedHistory)
  assert.deepEqual(f.captured[1].body, recoveredHistory)
  assert.equal(f.rows().filter(row => row.event === 'repair_applied').length, 1)
  assert.equal(f.rows().filter(row => row.event === 'session_compat_enabled').length, 1)
})

test('Codex 的兼容记忆不会影响同身份的 Claude 和 ZCode', async t => {
  const f = await fixture(t, (body, count) => count === 1 ? historyError(missingReasoning) : validateSearch(body))
  const session = { 'x-relay-session-id': 'fixture-shared-session' }
  assert.equal((await f.request(history, session)).status, 200)
  assert.equal(f.captured.length, 2)
  for (const client of ['claude', 'zcode']) {
    const response = await f.request(history, { ...session, 'x-relay-client': client })
    assert.equal(response.status, 200, `${client}: ${response.body}`)
    assert.equal(f.captured.at(-1).raw, JSON.stringify(history), `${client} 不得读取 Codex 降级标记`)
  }
  assert.equal((await f.request(history, session)).status, 200)
  assert.equal(f.captured.length, 5)
  assert.deepEqual(f.captured[4].body, recoveredHistory, 'Codex 自己仍应记住兼容模式')
})

test('Codex 明确上下文超限不能被跨资源文案覆盖', async t => {
  const f = await fixture(t, (_, count) => count === 1
    ? historyError('The requested item was created under a different Azure OpenAI resource', 'json', 'context_length_exceeded') : success)
  assert.equal((await f.request()).status, 400)
  assert.equal(f.captured.length, 1)
  assert.ok(!f.rows().some(row => ['repair_applied', 'session_compat_enabled'].includes(row.event)))
})

test('Codex 降级保留原生搜索的查询数组与来源扩展', async t => {
  const body = structuredClone(history)
  body.input[2].action = { type: 'search', queries: ['fixture first', 'fixture second'], sources: [{ type: 'url', url: 'https://example.invalid/fixture', id: 'nested_keep' }] }
  const f = await fixture(t, (_, count) => count === 1 ? historyError(missingReasoning) : success)
  assert.equal((await f.request(body)).status, 200)
  assert.equal(f.captured.length, 2)
  assert.deepEqual(f.captured[1].body, {
    model: 'fixture-model', store: false,
    input: [recoveredHistory.input[0], { type: 'web_search_call', status: 'completed', action: body.input[2].action }],
  })
})

for (const carrier of ['http', 'json', 'sse']) {
  for (const [kind, message] of [
    ['跨资源', 'The requested item was created under a different Azure OpenAI resource'],
    ['失效推理', 'encrypted content could not be verified'],
    ['缺配套推理', missingReasoning],
  ]) {
    test(`Codex ${carrier}/${kind} 首次错误后剥离并登记会话`, async t => {
      const f = await fixture(t, (body, count) => count === 1 ? historyError(message, carrier) : validateSearch(body))
      const response = await f.request()
      assert.equal(response.status, 200, response.body)
      assert.equal(f.captured.length, 2)
      assert.deepEqual(f.captured[0].body, expectedHistory)
      assert.deepEqual(f.captured[1].body, recoveredHistory)
      assert.equal(f.rows().filter(row => row.event === 'repair_applied').length, 1)
      assert.equal(f.rows().filter(row => row.event === 'session_compat_enabled').length, 1)
      const diagnostics = JSON.stringify(f.rows())
      for (const secret of ['fixture ciphertext', 'fixture prompt', 'fixture query', 'fixture-only-not-a-key', 'ws_fixture', 'rs_fixture']) {
        assert.ok(!diagnostics.includes(secret), '诊断不得泄露内容、凭据或条目编号')
      }
    })
  }
  for (const [code, sends, status] of [
    ['rate_limit_exceeded', 2, 200], ['server_error', 2, 200],
    ['invalid_api_key', 1, 400], ['insufficient_quota', 1, 402], ['context_length_exceeded', 1, 400],
  ]) {
    test(`Codex ${carrier}/${code} 不被缺推理文案误判`, async t => {
      const f = await fixture(t, (body, count) => count === 1 ? historyError(missingReasoning, carrier, code) : validateSearch(body))
      const response = await f.request()
      // 三种载体按同一具体原因恢复；HTTP 永久错误仍保留上游原状态/信封。
      assert.equal(response.status, carrier === 'http' && sends === 1 ? 400 : status, response.body)
      assert.equal(f.captured.length, sends)
      assert.ok(f.captured.every(({ body }) => JSON.stringify(body) === JSON.stringify(expectedHistory)))
      assert.ok(!f.rows().some(row => ['item_error', 'repair_applied', 'session_compat_enabled', 'codex_resource_retry'].includes(row.event)))
    })
  }
}

for (const remaining of [[], [{ type: 'reasoning', id: 'rs_other', encrypted_content: 'other ciphertext' }], [{ type: 'reasoning', encrypted_content: 'no id ciphertext' }]]) {
  test(`原请求已缺配套 rs 时不补造推理，明确报错后移除搜索引用（${remaining[0]?.id ?? remaining[0]?.type ?? '无推理'}）`, async t => {
    const body = { input: [expectedHistory.input[0], ...remaining, expectedHistory.input[2]] }
    const raw = ` \n${JSON.stringify(body)}\n `
    const f = await fixture(t, validateSearch)
    const response = await f.request(raw)
    assert.equal(response.status, 200, response.body)
    assert.equal(f.captured.length, 2)
    assert.equal(f.captured[0].raw, raw, '正常首发不主动删除搜索依赖')
    assert.deepEqual(f.captured[1].body, { input: recoveredHistory.input })
    assert.equal(f.rows().filter(row => row.event === 'repair_applied').length, 1)
  })
}

test('多个不相邻推理与搜索项全部保留，未知搜索形状也不拆推理编号', async t => {
  const body = structuredClone(history)
  body.input.push(
    { type: 'message', role: 'assistant', content: 'fixture search summary' },
    { type: 'reasoning', id: 'rs_other', summary: [], encrypted_content: 'other ciphertext' },
    { type: 'web_search_call', id: 'ws_other', action: { type: 'future_action' } },
  )
  const expected = structuredClone(body)
  delete expected.input[0].id
  const f = await fixture(t, (_, count) => count === 1 ? historyError('encrypted content could not be verified') : success)
  assert.equal((await f.request(body)).status, 200)
  assert.equal(f.captured.length, 2)
  assert.deepEqual(f.captured[0].body, expected)
  assert.equal(f.captured[1].raw, f.captured[0].raw)
})

for (const [name, headers, config] of [
  ['Claude', { 'x-relay-client': 'claude' }, {}],
  ['冲突标记', { 'x-relay-client': 'codex', 'user-agent': 'claude-desktop/1.0' }, {}],
  ['未知标记', { 'x-relay-client': 'unknown', 'user-agent': 'codex_cli_rs/0.100.0' }, {}],
  ['Codex 兼容关闭', {}, { PROXY_CODEX_RESOURCE_RETRY: '0' }],
]) {
  test(`${name} 不新增缺配套推理恢复策略`, async t => {
    const f = await fixture(t, () => historyError(missingReasoning), config)
    const response = await f.request(history, headers)
    assert.equal(response.status, 400)
    assert.equal(f.captured.length, 1)
    assert.ok(!f.rows().some(row => ['item_error', 'repair_applied', 'codex_resource_retry'].includes(row.event)))
    if (name !== 'Codex 兼容关闭') assert.equal(f.captured[0].raw, JSON.stringify(history))
  })
}

test('成功正文引用同一错误不会触发恢复', async t => {
  const responseBody = { output: [{ type: 'message', role: 'assistant', content: [{ type: 'output_text', text: missingReasoning }] }] }
  const f = await fixture(t, () => ({ status: 200, body: responseBody }))
  const response = await f.request()
  assert.equal(response.status, 200)
  assert.deepEqual(JSON.parse(response.body), responseBody)
  assert.equal(f.captured.length, 1)
  assert.ok(!f.rows().some(row => row.event === 'item_error'))
})

test('错误分类 SSE诊断指纹不把trace编号当作故障原因', async t => {
  const f = await fixture(t, (_, count) => count === 1
    ? historyError('encrypted content could not be verified. [trace_id=balance_unavailable_deployment_502]', 'sse') : success)
  assert.equal((await f.request()).status, 200)
  const failure = f.rows().find(row => row.event === 'precommit_failure')
  assert.ok(failure)
  assert.equal(failure.hints, 'encrypted', '诊断应与真实原因一致，而不是提取编号里的关键词')
  assert.equal(failure.retryable, false)
})

const streamEvent = obj => `event: ${obj.type}\ndata: ${JSON.stringify(obj)}\n\n`
const streamRateLimit = streamEvent({
  type: 'response.failed',
  response: { status: 'failed', error: { code: 'rate_limit_exceeded', message: 'fixture token rate limit exceeded' } },
})
const streamRecovery = streamEvent({ type: 'response.output_text.delta', delta: 'fixture recovered' })
  + streamEvent({ type: 'response.completed', response: { status: 'completed' } })

const emptyStreamItems = [
  ['空推理', { type: 'reasoning', id: 'rs_empty', summary: [] }],
  ['空消息', { type: 'message', id: 'msg_empty', role: 'assistant', status: 'in_progress', content: [] }],
]

const emptyStreamEvents = [
  ...emptyStreamItems.map(([name, item]) => [name, { type: 'response.output_item.added', output_index: 0, item }]),
  ['空正文块', { type: 'response.content_part.added', output_index: 0, content_index: 0,
    part: { type: 'output_text', text: '', annotations: [] } }],
  ['空拒绝块', { type: 'response.content_part.added', output_index: 0, content_index: 0,
    part: { type: 'refusal', refusal: '' } }],
]

for (const [name, event] of emptyStreamEvents) {
  for (const split of [false, true]) {
    test(`SSE ${name}占位后限流仍在网关内恢复（${split ? '分包' : '同包'}）`, async t => {
      const empty = streamEvent(event)
      const f = await fixture(t, async (_, count, req, res) => {
        if (count > 1) return { status: 200, type: 'text/event-stream', body: streamRecovery }
        if (!split) return { status: 200, type: 'text/event-stream', body: empty + streamRateLimit }
        res.writeHead(200, { 'content-type': 'text/event-stream', connection: 'close' })
        res.write(empty)
        await delay(20)
        res.end(streamRateLimit)
      })
      const response = await f.request()
      assert.equal(response.status, 200)
      assert.equal(response.body, streamRecovery, '空占位和失败事件都不能漏给客户端')
      assert.equal(f.captured.length, 2)
      assert.equal(f.captured[0].raw, f.captured[1].raw, '纯限流不得改写重发的请求体')
    })
  }
}

const searchStart = streamEvent({ type: 'response.output_item.added', output_index: 0,
  item: { type: 'web_search_call', id: 'ws_fixture', status: 'in_progress' } })

for (const [name, prefix] of [
  ['开始', searchStart],
  ['无状态开始', streamEvent({ type: 'response.output_item.added', item: { type: 'web_search_call' } })],
  ['搜索中', streamEvent({ type: 'response.output_item.added', item: { type: 'web_search_call', status: 'searching' } })],
  ['进度通知', searchStart
    + streamEvent({ type: 'response.web_search_call.in_progress', item_id: 'ws_fixture', output_index: 0 })
    + streamEvent({ type: 'response.web_search_call.searching', item_id: 'ws_fixture', output_index: 0 })],
]) {
  for (const split of [false, true]) {
    test(`Codex SSE 网页搜索${name}后限流仍在网关内恢复（${split ? '分包' : '同包'}）`, async t => {
      const f = await fixture(t, async (_, count, req, res) => {
        if (count > 1) return { status: 200, type: 'text/event-stream', body: streamRecovery }
        if (!split) return { status: 200, type: 'text/event-stream', body: prefix + streamRateLimit }
        res.writeHead(200, { 'content-type': 'text/event-stream', connection: 'close' })
        res.write(prefix)
        await delay(20)
        res.end(streamRateLimit)
      })
      const response = await f.request({ ...history, stream: true })
      assert.equal(response.status, 200)
      assert.equal(response.body, streamRecovery, '搜索刚开始的失败流不得漏给客户端')
      assert.equal(f.captured.length, 2)
      assert.equal(f.captured[0].raw, f.captured[1].raw, '纯限流不得改写重发的请求体')
    })
  }
}

test('Codex SSE 搜索失败结束不是成功结果，后续限流仍内部恢复', async t => {
  const failed = streamEvent({ type: 'response.output_item.done', output_index: 0,
    item: { type: 'web_search_call', id: 'ws_fixture', status: 'failed' } })
  const f = await fixture(t, (_, count) => ({ status: 200, type: 'text/event-stream',
    body: count === 1 ? searchStart + failed + streamRateLimit : streamRecovery }))
  const response = await f.request({ ...history, stream: true })
  assert.equal(response.body, streamRecovery)
  assert.equal(f.captured.length, 2)
})

for (const [name, complete] of [
  ['完成通知', { type: 'response.web_search_call.completed', item_id: 'ws_fixture', output_index: 0 }],
  ['完整结果', { type: 'response.output_item.done', output_index: 0,
    item: { type: 'web_search_call', id: 'ws_fixture', status: 'completed', action: { type: 'search', query: 'fixture query' } } }],
]) {
  test(`Codex SSE 搜索${name}后限流不重放且保留完整字节`, async t => {
    const stream = searchStart + streamEvent(complete) + streamRateLimit
    const f = await fixture(t, () => ({ status: 200, type: 'text/event-stream', body: stream }))
    const response = await f.request({ ...history, stream: true })
    assert.equal(response.status, 200)
    assert.equal(response.body, stream, '搜索已完成时不能丢弃结果重放')
    assert.equal(f.captured.length, 1)
  })
}

for (const reason of ['content_filter', 'max_output_tokens']) {
  test(`Codex SSE 搜索后 incomplete/${reason} 原样返回，不当空回重试`, async t => {
    const stream = searchStart + streamEvent({ type: 'response.incomplete', response: {
      status: 'incomplete', incomplete_details: { reason }, output: [],
    } })
    const f = await fixture(t, () => ({ status: 200, type: 'text/event-stream', body: stream }))
    const response = await f.request({ ...history, stream: true })
    assert.equal(response.status, 200)
    assert.equal(response.body, stream, '必须保留上游明确的未完成原因')
    assert.equal(f.captured.length, 1)
  })
}

for (const [name, headers, path] of [
  ['Claude', { 'x-relay-client': 'claude' }, '/v1/responses'],
  ['ZCode', { 'x-relay-client': 'zcode' }, '/v1/responses'],
  ['未知客户端', { 'x-relay-client': 'fixture_unknown' }, '/v1/responses'],
  ['冲突客户端标记', { 'x-relay-client': 'codex', 'user-agent': 'claude-code/fixture' }, '/v1/responses'],
  ['非 Responses 端点', { 'x-relay-client': 'codex' }, '/v1/messages'],
]) {
  test(`${name} 的搜索开始仍保留原交付边界`, async t => {
    const stream = searchStart + streamRateLimit
    const f = await fixture(t, () => ({ status: 200, type: 'text/event-stream', body: stream }))
    const response = await f.request({ ...history, stream: true }, headers, { path })
    assert.equal(response.body, stream)
    assert.equal(f.captured.length, 1)
  })
}

for (const [name, config] of [
  ['历史兼容关闭', { PROXY_CODEX_RESOURCE_RETRY: '0' }],
  ['historyMode off', { PROXY_POLICY_JSON: JSON.stringify({ schemaVersion: 1,
    profiles: { codex: { historyMode: 'off' } } }) }],
]) {
  test(`Codex 搜索缓冲不依赖${name}`, async t => {
    const f = await fixture(t, (_, count) => ({ status: 200, type: 'text/event-stream',
      body: count === 1 ? searchStart + streamRateLimit : streamRecovery }), config)
    const response = await f.request({ ...history, stream: true })
    assert.equal(response.body, streamRecovery)
    assert.equal(f.captured.length, 2)
  })
}

test('Codex SSE 搜索成功流的全部事件原字节保留', async t => {
  const stream = streamEvent({ type: 'response.created', response: { status: 'in_progress', output: [] } })
    + searchStart
    + streamEvent({ type: 'response.web_search_call.searching', item_id: 'ws_fixture', output_index: 0 })
    + streamEvent({ type: 'response.web_search_call.completed', item_id: 'ws_fixture', output_index: 0 })
    + streamEvent({ type: 'response.output_item.done', item: { type: 'web_search_call', id: 'ws_fixture',
      status: 'completed', action: { type: 'search', query: 'fixture query' } } })
    + streamRecovery
  const f = await fixture(t, () => ({ status: 200, type: 'text/event-stream', body: stream }))
  const response = await f.request({ ...history, stream: true })
  assert.equal(response.body, stream)
  assert.equal(f.captured.length, 1)
})

for (const [name, progress] of [
  ['正文', { type: 'response.output_text.delta', delta: 'fixture visible text' }],
  ['客户端工具', { type: 'response.output_item.added', item: { type: 'custom_tool_call', name: 'fixture_tool', call_id: 'call_fixture', input: '' } }],
  ['搜索完成', { type: 'response.web_search_call.completed', item_id: 'ws_fixture', output_index: 0 }],
]) {
  test(`Codex SSE 搜索后${name}立即交付，不等流结束`, async t => {
    const prefix = searchStart + streamEvent(progress)
    let upstreamResponse
    const f = await fixture(t, (_, count, req, res) => {
      upstreamResponse = res
      res.writeHead(200, { 'content-type': 'text/event-stream', connection: 'close' })
      res.write(prefix)
    })
    const response = await fetch(`${f.url}/v1/responses`, {
      method: 'POST', signal: AbortSignal.timeout(3000),
      headers: { authorization: 'Bearer fixture-only-not-a-key', 'content-type': 'application/json', 'x-relay-client': 'codex' },
      body: JSON.stringify({ ...history, stream: true }),
    })
    const reader = response.body.getReader()
    let received = ''
    while (received.length < prefix.length) {
      const chunk = await reader.read()
      assert.equal(chunk.done, false, '真正内容出现后必须在流结束前可读')
      received += Buffer.from(chunk.value).toString('utf8')
    }
    assert.equal(received, prefix)
    assert.equal(upstreamResponse.writableEnded, false)
    upstreamResponse.end(streamRateLimit)
    for (;;) {
      const chunk = await reader.read()
      if (chunk.done) break
      received += Buffer.from(chunk.value).toString('utf8')
    }
    assert.equal(received, prefix + streamRateLimit)
    assert.equal(f.captured.length, 1, '真实输出后的错误不得重放')
  })
}

for (const headers of [{ 'x-relay-client': 'claude' }, { 'x-relay-client': 'zcode' }]) {
  test(`${headers['x-relay-client']} 搜索流与旧核心逐字一致`, { skip: !process.env.AG_RELAY_HISTORY_BASELINE }, async t => {
    const results = []
    for (const script of [resolve(process.env.AG_RELAY_HISTORY_BASELINE), core]) {
      const f = await fixture(t, () => ({ status: 200, type: 'text/event-stream', body: searchStart + streamRateLimit }), {}, script)
      results.push({ response: await f.request({ ...history, stream: true }, headers), sent: f.captured.map(x => x.raw) })
    }
    assert.deepEqual(results[1], results[0])
  })
}

test('Codex SSE 慢搜索沿用原流式等待，不受首内容短超时截断', { skip: !process.env.AG_RELAY_HISTORY_BASELINE }, async t => {
  const stream = searchStart + streamRecovery
  const observations = []
  for (const script of [resolve(process.env.AG_RELAY_HISTORY_BASELINE), core]) {
    const f = await fixture(t, async (_, count, req, res) => {
      res.writeHead(200, { 'content-type': 'text/event-stream', connection: 'close' })
      res.write(searchStart)
      await delay(500)
      if (!res.destroyed) res.end(streamRecovery)
    }, { PROXY_PEEK_IDLE_MS: '150', PROXY_PEEK_TOTAL_MS: '300',
      PROXY_STREAM_IDLE_MS: '1500', PROXY_STREAM_PROGRESS_MS: '3000' }, script)
    const response = await f.request({ ...history, stream: true })
    observations.push({ response, attempts: f.captured.length })
  }
  assert.deepEqual(observations[0], { response: { status: 200, body: stream }, attempts: 1 })
  assert.deepEqual(observations[1], observations[0], '缓冲不能使旧版能成功的慢搜索被提前取消')
})

test('Codex SSE 搜索进度不能无限刷新等待窗口', async t => {
  const f = await fixture(t, (_, count, req, res) => {
    res.writeHead(200, { 'content-type': 'text/event-stream', connection: 'close' })
    res.write(searchStart)
    const heartbeat = setInterval(() => {
      if (!res.destroyed) res.write(searchStart)
    }, 40)
    res.once('close', () => clearInterval(heartbeat))
  }, { PROXY_MAX_ATTEMPTS: '1', PROXY_STREAM_IDLE_MS: '1000', PROXY_STREAM_PROGRESS_MS: '250' })
  const response = await f.request({ ...history, stream: true }, {}, { signal: AbortSignal.timeout(3000) })
  assert.equal(response.status, 429)
  assert.equal(f.captured.length, 1)
})

test('Codex SSE 搜索等待中取消不再重发', async t => {
  let sent
  const started = new Promise(resolve => { sent = resolve })
  const f = await fixture(t, (_, count, req, res) => {
    res.writeHead(200, { 'content-type': 'text/event-stream', connection: 'close' })
    res.write(searchStart)
    sent()
  })
  const controller = new AbortController()
  const pending = f.request({ ...history, stream: true }, {}, { signal: controller.signal })
  const rejected = assert.rejects(pending, { name: 'AbortError' })
  await Promise.race([started, delay(3000).then(() => { throw new Error('假上游未收到搜索请求') })])
  controller.abort()
  await rejected
  await delay(100)
  assert.equal(f.captured.length, 1)
})

test('Codex SSE 搜索失败仍受常规尝试上限约束', async t => {
  const f = await fixture(t, () => ({ status: 200, type: 'text/event-stream', body: searchStart + streamRateLimit }),
    { PROXY_MAX_ATTEMPTS: '3' })
  const response = await f.request({ ...history, stream: true })
  assert.equal(response.status, 429)
  assert.equal(f.captured.length, 3)
  assert.ok(!response.body.includes('event: response.output_item.added'))
})

for (const [name, event] of [
  ['正文', { type: 'response.output_text.delta', delta: 'fixture delivered text' }],
  ['思考', { type: 'response.reasoning_summary_text.delta', delta: 'fixture delivered reasoning' }],
  ['带正文消息', { type: 'response.output_item.added', item: { type: 'message', content: [{ type: 'output_text', text: 'fixture delivered message' }] } }],
  ['带摘要推理', { type: 'response.output_item.added', item: { type: 'reasoning', summary: [{ type: 'summary_text', text: 'fixture summary' }] } }],
  ['加密推理', { type: 'response.output_item.added', item: { type: 'reasoning', encrypted_content: 'fixture ciphertext', summary: [] } }],
  ['拒绝内容', { type: 'response.content_part.added', part: { type: 'refusal', refusal: 'fixture refusal' } }],
  ['普通工具', { type: 'response.output_item.added', item: { type: 'function_call', name: 'fixture_lookup', call_id: 'call_fixture', arguments: '' } }],
  ['自定义工具', { type: 'response.output_item.added', item: { type: 'custom_tool_call', name: 'fixture_custom', call_id: 'call_fixture', input: '' } }],
  ['已完成网页搜索', { type: 'response.output_item.added', item: { type: 'web_search_call', id: 'ws_fixture', status: 'completed', action: { type: 'search', query: 'fixture query' } } }],
  ['未知搜索状态', { type: 'response.output_item.added', item: { type: 'web_search_call', status: 'fixture_future_status' } }],
  ['未知工具', { type: 'response.output_item.added', item: { type: 'fixture_future_tool', value: 'fixture data' } }],
  ['未知载荷', { type: 'response.content_part.added', part: { type: 'fixture_future_content', value: 'fixture data' } }],
]) {
  for (const searchFirst of [false, true]) {
    test(`SSE ${searchFirst ? '搜索缓冲后' : ''}已交付${name}后限流保持原字节且不重放`, async t => {
      const stream = (searchFirst ? searchStart : '') + streamEvent(event) + streamRateLimit
      const f = await fixture(t, () => ({ status: 200, type: 'text/event-stream', body: stream }))
      const response = await f.request({ ...history, stream: true })
      assert.equal(response.status, 200)
      assert.equal(response.body, stream)
      assert.equal(f.captured.length, 1, '已有内容、已完成搜索或客户端工具开始后不得重发整个请求')
    })
  }
}

test('SSE 空占位后正常输出保留完整流字节', async t => {
  const stream = emptyStreamEvents.map(([, event]) => streamEvent(event)).join('') + streamRecovery
  const f = await fixture(t, () => ({ status: 200, type: 'text/event-stream', body: stream }))
  const response = await f.request()
  assert.equal(response.body, stream, '只推迟交付，不能删除成功流中的占位事件')
  assert.equal(f.captured.length, 1)
})

test('已交付 SSE 内容后出现缺推理错误不重放', async t => {
  const stream = `event: response.output_text.delta\ndata: ${JSON.stringify({ type: 'response.output_text.delta', delta: 'fixture visible text' })}\n\ndata: ${JSON.stringify({ error: { type: 'invalid_request_error', message: missingReasoning } })}\n\n`
  const f = await fixture(t, () => ({ status: 200, type: 'text/event-stream', body: stream }))
  const response = await f.request()
  assert.equal(response.status, 200)
  assert.equal(response.body, stream)
  assert.equal(f.captured.length, 1)
})

test('客户端在历史重试等待期间取消，不再发送下一次', { timeout: 10000 }, async t => {
  const f = await fixture(t, () => historyError(missingReasoning), { PROXY_DEPLOY_DELAY_MS: '500' })
  const controller = new AbortController()
  const pending = f.request(history, {}, { signal: controller.signal })
  const rejected = assert.rejects(pending, { name: 'AbortError' })
  const deadline = Date.now() + 3000
  while (f.captured.length === 0) {
    assert.ok(Date.now() < deadline, '假上游未收到首发请求')
    await delay(5)
  }
  controller.abort()
  await rejected
  await delay(650)
  assert.equal(f.captured.length, 1)
})

test('非 Responses 端点不套用新历史恢复', async t => {
  const f = await fixture(t, () => historyError(missingReasoning))
  const response = await f.request(history, {}, { path: '/v1/messages' })
  assert.equal(response.status, 400)
  assert.equal(f.captured.length, 1)
  assert.equal(f.captured[0].raw, JSON.stringify(history))
  assert.ok(!f.rows().some(row => row.event === 'item_error'))
})

test('关闭首发剥 id 后仍只在明确错误后剥离', async t => {
  const f = await fixture(t, (body, count) => count === 1 ? historyError(missingReasoning) : validateSearch(body), { PROXY_CODEX_STRIP_IDS: '0' })
  const raw = ` \n${JSON.stringify(history)}\n `
  assert.equal((await f.request(raw)).status, 200)
  assert.equal(f.captured.length, 2)
  assert.equal(f.captured[0].raw, raw)
  assert.deepEqual(f.captured[1].body, recoveredHistory)
})

for (const [name, extra, item] of [
  ['previous_response_id', { previous_response_id: 'resp_server_only' }],
  ['conversation', { conversation: { id: 'conv_server_only' } }],
  ['item_reference', {}, { type: 'item_reference', id: 'msg_server_only' }],
  ['compaction', {}, { type: 'compaction', encrypted_content: 'opaque fixture' }],
  ['unknown_item', {}, { type: 'future_tool', id: 'opaque_fixture' }],
  ['missing_tool_result', {}, { type: 'function_call', call_id: 'call_orphan', name: 'fixture', arguments: '{}' }],
]) {
  test(`Codex ${name} 不能被盲目剥离，保留字节并遵守五次上限`, async t => {
    const body = { ...structuredClone(history), ...extra }
    delete body.input[0].id
    if (item) body.input.push(item)
    const raw = ` \n${JSON.stringify(body)}\n `
    const f = await fixture(t, () => historyError('The requested item was created under a different Azure OpenAI resource'))
    assert.equal((await f.request(raw)).status, 400)
    assert.equal(f.captured.length, 5)
    assert.ok(f.captured.every(sent => sent.raw === raw))
    assert.ok(!f.rows().some(row => ['repair_applied', 'session_compat_enabled'].includes(row.event)))
  })
}

for (const [name, action, withId] of [
  ['open_page', { type: 'open_page', url: 'https://example.invalid/fixture', extra: { id: 'nested_keep' } }, true],
  ['find_in_page', { type: 'find_in_page', url: 'https://example.invalid/fixture', pattern: 'fixture' }, true],
  ['already_idless', { type: 'search', query: 'fixture query' }, false],
]) {
  test(`Codex ${name} 的完整搜索内容在降级后保留`, async t => {
    const body = structuredClone(history)
    body.input[2].action = action
    if (!withId) delete body.input[2].id
    const f = await fixture(t, (_, count) => count === 1 ? historyError('encrypted content could not be verified') : success)
    assert.equal((await f.request(body)).status, 200)
    assert.equal(f.captured.length, 2)
    assert.deepEqual(f.captured[1].body, { model: 'fixture-model', store: false, input: [recoveredHistory.input[0], { type: 'web_search_call', status: 'completed', action }] })
  })
}

test('Codex 降级完整保留普通和自定义工具的配对及嵌套载荷', async t => {
  const tools = [
    { type: 'function_call', call_id: 'call_function', name: 'fixture_lookup', arguments: '{"id":"nested_arg"}' },
    { type: 'function_call_output', call_id: 'call_function', output: 'fixture function result' },
    { type: 'custom_tool_call', call_id: 'call_custom', name: 'fixture_custom', input: '{"encrypted_content":"nested_keep"}' },
    { type: 'custom_tool_call_output', call_id: 'call_custom', output: [{ type: 'input_text', text: 'fixture custom result' }] },
  ]
  const body = { ...structuredClone(history), input: [...history.input, ...tools.map((item, index) => ({ id: `item_${index}`, ...item }))] }
  const f = await fixture(t, (_, count) => count === 1 ? historyError(missingReasoning) : success)
  assert.equal((await f.request(body)).status, 200)
  assert.deepEqual(f.captured[1].body, { model: 'fixture-model', store: false, input: [...recoveredHistory.input, ...tools] })
})

for (const temporary of [false, true]) {
  test(`Codex 搜索降级后最多再发三次且请求体冻结（${temporary ? '混合503' : '历史错误'}）`, async t => {
    const f = await fixture(t, (_, count) => count === 1 || !temporary
      ? historyError(missingReasoning) : { status: 503, body: { error: { code: 'server_error', message: 'fixture unavailable' } } })
    assert.equal((await f.request()).status, 503)
    assert.equal(f.captured.length, 4)
    assert.deepEqual(f.captured[1].body, recoveredHistory)
    assert.ok(f.captured.slice(1).every(sent => sent.raw === f.captured[1].raw))
    assert.equal(f.rows().filter(row => row.event === 'repair_applied').length, 1)
  })
}

// 生产回归：会话记忆的预清理覆盖了整个 Codex 会话，若仍按「修复版」限制三次发送，
// 纯限流也只能试 3 次（实测一天 83 次失败，配置的 110 次形同虚设）。
test('Codex 会话记忆预清理不占用三次发送上限，纯限流按通用预算重试', async t => {
  const rateLimited = { status: 200, type: 'text/event-stream',
    body: `data: ${JSON.stringify({ error: { code: 'rate_limit_exceeded', message: 'fixture rate limit' } })}\n\n` }
  const seed = { ...history, input: history.input.filter(item => item.type !== 'web_search_call') }
  // 计数：1 历史错误、2 修复后成功（登记会话）、3 该会话第 2 次请求成功；
  // 第 3 次请求从 4 起连续 4 次限流，第 8 次才成功。
  const f = await fixture(t, (body, count) => {
    if (count === 1) return { status: 400, body: { error: { type: 'invalid_request_error', message: 'encrypted content could not be verified' } } }
    if (count <= 3) return success
    return count <= 7 ? rateLimited : success
  })
  const headers = { 'x-relay-session-id': 'fixture-rate-session' }
  assert.equal((await f.request(seed, headers)).status, 200)
  assert.equal((await f.request(history, headers)).status, 200)
  const response = await f.request(history, headers)
  assert.equal(response.status, 200, '预清理后的请求不应被三次上限截断')
  assert.equal(f.captured.length, 8, '第 3 次请求应实际发送 5 次（首发 + 4 次限流重试）')
  assert.ok(f.rows().some(row => row.event === 'session_compat_applied' && row.removedReasoning === 1),
    '该请求确实做过会话语义预清理')
  assert.ok(!f.rows().some(row => row.event === 'codex_resource_exhausted'),
    '没有历史错误证据时不得按资源上限收手')
})

for (const change of ['session', 'model', 'credentials']) {
  test(`Codex 兼容模式不污染新的 ${change}`, async t => {
    const f = await fixture(t, (_, count) => count === 1 ? historyError(missingReasoning) : success)
    const headers = { 'x-relay-session-id': 'fixture-session-a' }
    assert.equal((await f.request(history, headers)).status, 200)
    const next = structuredClone(history)
    const nextHeaders = { ...headers }
    if (change === 'session') nextHeaders['x-relay-session-id'] = 'fixture-session-b'
    if (change === 'model') next.model = 'fixture-other-model'
    if (change === 'credentials') nextHeaders.authorization = 'Bearer fixture-other-not-a-key'
    assert.equal((await f.request(next, nextHeaders)).status, 200)
    assert.equal(f.captured.length, 3)
    assert.deepEqual(f.captured[2].body, { ...expectedHistory, model: next.model })
  })
}

test('Claude 的旧兼容记忆也不会使 Codex 未报错就降级', async t => {
  const body = { ...history, input: history.input.slice(0, 2) }
  const f = await fixture(t, (_, count) => count === 1 ? historyError('The requested item was created under a different Azure OpenAI resource') : success)
  const session = { 'x-relay-session-id': 'fixture-shared-session' }
  assert.equal((await f.request(body, { ...session, 'x-relay-client': 'claude' })).status, 200)
  assert.equal((await f.request(body, session)).status, 200)
  assert.equal(f.captured.length, 3)
  assert.deepEqual(f.captured[2].body, { model: 'fixture-model', store: false, input: [expectedHistory.input[0], { type: 'reasoning', summary: [], encrypted_content: 'fixture ciphertext' }] })
})

test('Codex 兼容缓存满时不会挤掉 Claude 的已有记录', async t => {
  const body = { ...history, input: history.input.slice(0, 2) }
  const f = await fixture(t, (_, count) => count === 1 || count === 3 ? historyError('The requested item was created under a different Azure OpenAI resource') : success, { PROXY_SESSION_COMPAT_MAX_KEYS: '1' })
  const legacyHeaders = { 'x-relay-client': 'claude', 'x-relay-session-id': 'fixture-legacy' }
  assert.equal((await f.request(body, legacyHeaders)).status, 200)
  assert.equal((await f.request(history, { 'x-relay-session-id': 'fixture-codex' })).status, 200)
  assert.equal((await f.request(body, legacyHeaders)).status, 200)
  assert.equal(f.captured.length, 5)
  assert.deepEqual(f.captured[4].body, { model: 'fixture-model', store: false, input: [expectedHistory.input[0]] })
})

for (const scenario of ['normal', 'azure', 'reasoning', 'empty_shape', 'empty_reasoning']) {
  test(`Claude 修改前后差分：${scenario}`, { skip: !process.env.AG_RELAY_HISTORY_BASELINE }, async t => {
    const raw = ` \n${JSON.stringify({ ...history, input: history.input.slice(0, 2) })}\n `
    const observations = []
    for (const script of [resolve(process.env.AG_RELAY_HISTORY_BASELINE), core]) {
      const f = await fixture(t, (_, count) => {
        if (scenario === 'azure' && count === 1) return historyError('The requested item was created under a different Azure OpenAI resource')
        if (scenario === 'reasoning' && count === 1) return historyError('encrypted content could not be verified')
        if (count <= (scenario === 'empty_shape' ? 2 : scenario === 'empty_reasoning' ? 6 : 0)) return { status: 200, type: 'text/event-stream', body: '' }
        return success
      }, { PROXY_DEPLOY_MAX_ATTEMPTS: '8' }, script)
      const headers = { 'x-relay-client': 'claude', 'x-relay-session-id': 'fixture-differential' }
      const responses = [await f.request(raw, headers), await f.request(raw, headers)]
      const rows = f.rows()
      observations.push({ responses, sent: f.captured.map(x => x.raw), repairs: rows.filter(x => x.event === 'repair_applied').map(({ kind, removedIds, removedReasoning }) => ({ kind, removedIds, removedReasoning })), enabled: rows.filter(x => x.event === 'session_compat_enabled').length })
    }
    assert.deepEqual(observations[1], observations[0], 'Claude 响应、请求字节和恢复策略必须与修改前一致')
  })
}

for (const explicit of [false, true]) {
  test(`Codex 已登记会话后来出现搜索也首发降级（${explicit ? '显式会话' : '资源血缘'}）`, async t => {
    const seed = { ...history, input: history.input.filter(item => item.type !== 'web_search_call') }
    const f = await fixture(t, (body, count) => count === 1
      ? { status: 400, body: { error: { type: 'invalid_request_error', message: 'encrypted content could not be verified' } } }
      : validateSearch(body))
    const headers = explicit ? { 'x-relay-session-id': 'fixture-session' } : {}
    assert.equal((await f.request(seed, headers)).status, 200)
    assert.equal(f.captured.length, 2)
    const response = await f.request(history, headers)
    assert.equal(response.status, 200, response.body)
    assert.equal(f.captured.length, 3, '搜索进入已登记会话后也应一次发送成功')
    assert.deepEqual(f.captured[2].body, recoveredHistory)
    assert.equal(f.rows().filter(row => row.event === 'repair_applied').length, 1, '后续首发使用记忆，不重新触发错误修复')
    assert.equal(f.rows().filter(row => row.event === 'session_compat_enabled').length, 1)
    assert.ok(f.rows().some(row => row.event === 'session_compat_applied' && row.removedReasoning === 1))
  })
}

// 2026-11-01：上游明确要求“不带旧加密推理的新会话”。不属于某一家客户端或模型：
// 只有同时出现两条短语（200/400/409）才进入有界修复；普通 503/429 不得改请求体。
const accountReasoningUnavailable = 'the upstream account bound to this conversation is unavailable; start a new conversation without previous encrypted reasoning items (request id: fixture) [trace_id=fixture]'
const accountHistory = {
  model: 'fixture-model', store: false, prompt_cache_key: 'account-cache-fixture',
  client_metadata: { session_id: 'fixture-session' },
  input: [
    { type: 'message', id: 'msg_fixture', role: 'user', content: 'fixture prompt' },
    { type: 'reasoning', id: 'rs_fixture', encrypted_content: 'fixture ciphertext', summary: [{ type: 'summary_text', text: 'kept summary' }] },
    { type: 'function_call', id: 'fc_fixture', call_id: 'call_fixture', name: 'fixture', arguments: '{"value":1}' },
    { type: 'function_call_output', id: 'fco_fixture', call_id: 'call_fixture', output: 'fixture result' },
  ],
}
const accountTools = ['message', 'function_call', 'function_call_output']
const claudeHeaders = { 'x-relay-client': 'claude', 'user-agent': 'claude-cli/fixture', 'session-id': 'fixture-session' }
const codexHeaders = { 'x-relay-client': 'codex', 'user-agent': 'codex_cli_rs/fixture', 'session-id': 'fixture-session' }

for (const carrier of ['http', 'json', 'sse']) {
  test(`账号绑定推理不可用（${carrier}）：只删旧推理后重试，保留消息与工具`, async t => {
    const f = await fixture(t, (_, count) => count === 1
      ? historyError(accountReasoningUnavailable, carrier) : success)
    const response = await f.request(accountHistory, claudeHeaders)
    assert.equal(response.status, 200, response.body)
    assert.equal(f.captured.length, 2)
    assert.equal(f.captured[0].body.input.some(item => item.type === 'reasoning'), true)
    assert.deepEqual(f.captured[1].body.input.map(item => item.type), accountTools)
    assert.deepEqual(f.captured[1].body.client_metadata, accountHistory.client_metadata)
    assert.equal(f.captured[1].body.prompt_cache_key, accountHistory.prompt_cache_key)
    assert.equal(f.captured[1].body.input[0].id, 'msg_fixture', 'Claude 保留消息编号')
    assert.equal(f.captured[1].body.input[1].call_id, 'call_fixture')
    assert.equal(f.captured[1].headers['user-agent'], 'claude-cli/fixture')
    assert.equal(f.captured[1].headers['session-id'], 'fixture-session')
    assert.equal(f.captured[1].headers.authorization, 'Bearer fixture-only-not-a-key')
  })
}

test('账号绑定推理不可用：Codex 同样适用，不按客户端或模型写死', async t => {
  const f = await fixture(t, (_, count) => count === 1
    ? historyError(accountReasoningUnavailable) : success)
  const response = await f.request(accountHistory, codexHeaders)
  assert.equal(response.status, 200, response.body)
  assert.equal(f.captured.length, 2)
  assert.deepEqual(f.captured[1].body.input.map(item => item.type), accountTools)
  assert.equal(f.captured[1].body.input[0].id, undefined, 'Codex 按既有规则去掉可选编号')
  assert.equal(f.captured[1].body.input[1].call_id, 'call_fixture')
  assert.equal(f.captured[1].headers['user-agent'], 'codex_cli_rs/fixture')
})

test('账号绑定推理不可用：两次重试后停止，不无限重发', async t => {
  const f = await fixture(t, () => historyError(accountReasoningUnavailable))
  const response = await f.request(accountHistory, claudeHeaders)
  assert.equal(response.status, 400)
  assert.equal(f.captured.length, 3, '原始一次 + 两次重试')
  for (const item of f.captured.slice(1)) {
    assert.deepEqual(item.body.input.map(entry => entry.type), accountTools)
  }
  assert.deepEqual(f.captured[1].body, f.captured[2].body, '重试使用同一份已删推理的请求')
})

test('账号绑定推理不可用：普通 503 与 429 不得改写请求体', async t => {
  for (const reply of [
    { status: 503, body: { error: { type: 'server_error', message: 'Service temporarily unavailable' } } },
    { status: 429, body: { error: { code: 'rate_limit_exceeded', type: 'too_many_requests', message: 'token rate limit exceeded' } } },
  ]) {
    const f = await fixture(t, () => reply)
    const response = await f.request(accountHistory, claudeHeaders)
    assert.ok([429, 503].includes(response.status), response.body)
    assert.equal(f.captured.length, 5, '普通错误仍按既有重试预算')
    for (const item of f.captured) assert.deepEqual(item.body, accountHistory)
  }
})

for (const carrier of ['http', 'json', 'sse']) {
  for (const message of [
    'the upstream account bound to this conversation is unavailable',
    'start a new conversation without previous encrypted reasoning items',
  ]) {
    test(`账号绑定推理不可用：单短语不触发修复 ${carrier}/${message.slice(0, 12)}`, async t => {
      const f = await fixture(t, () => historyError(message, carrier))
      await f.request(accountHistory, claudeHeaders)
      for (const item of f.captured) assert.deepEqual(item.body, accountHistory)
      assert.equal(f.rows().some(row => row.event === 'repair_applied'), false)
    })
  }
}

test('账号绑定推理不可用：409后混合限流沿原预算且不记忆删推理', async t => {
  const replies = [
    { ...historyError(accountReasoningUnavailable), status: 409 },
    { status: 429, body: { error: { code: 'rate_limit_exceeded', message: 'token rate limit exceeded' } } },
    { status: 503, body: { error: { code: 'server_error', message: 'Service temporarily unavailable' } } },
    success,
  ]
  const f = await fixture(t, (_, count) => replies[count - 1] ?? success)
  assert.equal((await f.request(accountHistory, claudeHeaders)).status, 200)
  assert.equal(f.captured.length, 4, '普通限流不挤占账号错误的两次重试计数')
  const repaired = { ...accountHistory, input: accountHistory.input.filter(item => item.type !== 'reasoning') }
  for (const item of f.captured.slice(1)) assert.deepEqual(item.body, repaired)
  assert.equal(f.rows().some(row => row.event === 'session_compat_enabled'), false)
  assert.equal((await f.request(accountHistory, claudeHeaders)).status, 200)
  assert.deepEqual(f.captured[4].body, accountHistory, '下一请求不能自动删推理')
})

// 编辑前工作区基线包含尚未提交的账号修复；与发行基线的既有差分分开验证。
for (const carrier of ['http', 'json', 'sse']) {
  for (const [name, message, code] of [
    ['未知错误', 'fixture unclassified rejection', 'fixture_unknown'],
    ['永久错误', 'invalid api key', 'invalid_api_key'],
    ['临时限流', 'token rate limit exceeded', 'rate_limit_exceeded'],
    ['部署缺失', 'No deployment to match this request', undefined],
    ['账号推理', accountReasoningUnavailable, undefined],
    ['拒绝历史改写', 'The requested item was created under a different Azure OpenAI resource', undefined],
  ]) {
    test(`恢复编排工作区差分 ${carrier}/${name}`, { skip: !process.env.AG_RELAY_WORKSPACE_BASELINE }, async t => {
      const { prompt_cache_key, ...body } = accountHistory
      if (name === '拒绝历史改写') body.previous_response_id = 'resp_fixture'
      const results = []
      for (const script of [resolve(process.env.AG_RELAY_WORKSPACE_BASELINE), core]) {
        const f = await fixture(t, (_, count) => count <= 3 ? historyError(message, carrier, code) : success, {}, script)
        const response = await f.request(body, claudeHeaders)
        results.push({ response, sent: f.captured.map(item => ({ raw: item.raw,
          authorization: item.headers.authorization, ua: item.headers['user-agent'], session: item.headers['session-id'] })) })
      }
      assert.deepEqual(results[1], results[0], '整理不能改变原恢复结果、发送次数或请求字节')
    })
  }
}

// 2026-10-21 线上事故：Windows 上 rename 覆盖被占用的 .1 返回 EPERM，原实现每写一条都轮转、
// 失败又被静默吞掉，日志在 1 MiB 后停了 6 小时。这里固定两个行为：到限仍能继续写，
// 且归档不可用时也不会静默停摆。核心把容量下限钳在 4096，测试用略高于它的预算。
const diagnosticsLimit = 5000
const diagnosticOptions = () => ({
  PROXY_DIAGNOSTICS_MAX_BYTES: String(diagnosticsLimit), PROXY_STATUS_INTERVAL_MS: '100000',
})
const diagnosticTimes = file => readFileSync(file, 'utf8').trim().split('\n').map(line => JSON.parse(line).time)

test('诊断到限后轮转并继续写入，归档与当前档都保留', async t => {
  const f = await fixture(t, () => success, diagnosticOptions())
  for (let i = 0; i < 16; i++) assert.equal((await f.request()).status, 200)
  const live = f.diagnostics
  assert.ok(statSync(live).size <= diagnosticsLimit, '当前档不得超过预算')
  const archived = statSync(live + '.1').size
  assert.ok(archived > 0 && archived <= diagnosticsLimit, '.1 必须存在且不超预算')
  assert.ok(diagnosticTimes(live + '.1').every(time => time), '归档必须是完整 JSON 行')
  const liveTimes = diagnosticTimes(live)
  assert.ok(liveTimes.every(time => time), '当前档必须是完整 JSON 行')
  assert.ok(liveTimes.length > 0, '轮转后仍应继续写入')
  // 归档的最后一条必须早于当前档的第一条：说明没有把新内容挤掉
  assert.ok(diagnosticTimes(live + '.1').at(-1) <= liveTimes[0], '归档与当前档时间必须连续')
})

test('归档无法替换时截断续写，诊断不会静默停摆', async t => {
  const f = await fixture(t, () => success, diagnosticOptions())
  const live = f.diagnostics
  for (let i = 0; i < 8; i++) assert.equal((await f.request()).status, 200)
  // 用目录占住 .1 的位置：删不掉、也 rename 不过去，模拟 Windows 下归档被占用。
  // 连续失败几次后走截断兜底，写入必须恢复。
  rmSync(live + '.1', { force: true })
  mkdirSync(live + '.1')
  try {
    for (let i = 0; i < 24; i++) assert.equal((await f.request()).status, 200)
    // 缓存路由摘要也在请求结束后才写入；等最后一条诊断落盘再读，避免与追加写入抢同一行。
    await delay(50)
    const written = readFileSync(live, 'utf8')
    assert.ok(written.length > 0, '归档不可用时仍必须写入当前档')
    assert.ok(statSync(live).size <= diagnosticsLimit, '截断兜底后当前档仍须有界')
    assert.ok(diagnosticTimes(live).every(time => time), '当前档必须是完整 JSON 行')
    // 截断发生过，说明恢复不是运气：写入量必须远超单档容量
    assert.ok(diagnosticTimes(live).length < 60, '应发生截断而不是把全部记录堆在内存之外')
  } finally {
    rmSync(live + '.1', { recursive: true, force: true })
  }
})
