// 只在 HTTP 网关→假上游和 /_relay/status 接缝验证策略，不读取核心私有函数。
import assert from 'node:assert/strict'
import test from 'node:test'
import http from 'node:http'
import { spawn } from 'node:child_process'
import { once } from 'node:events'
import { mkdtempSync, readFileSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { setTimeout as delay } from 'node:timers/promises'

const core = fileURLToPath(new URL('../src-tauri/resources/gateway/relay-retry-proxy.mjs', import.meta.url))
const runtime = process.platform === 'win32'
  ? fileURLToPath(new URL('../src-tauri/binaries/node-x86_64-pc-windows-msvc.exe', import.meta.url)) : process.execPath
const success = { status: 200, body: { ok: true } }
const missingReasoning = "Item 'ws_policy' of type 'web_search_call' was provided without its required 'reasoning' item: 'rs_policy'."
const history = {
  model: 'fixture-model', store: false,
  input: [
    { type: 'message', id: 'msg_policy', role: 'user', content: 'policy fixture prompt' },
    { type: 'reasoning', id: 'rs_policy', summary: [], encrypted_content: 'policy fixture ciphertext' },
    { type: 'web_search_call', id: 'ws_policy', status: 'completed', action: { type: 'search', query: 'policy fixture query' } },
  ],
}

// 产品默认值来自已批准的配置契约；只缩短本测试的重试参数。
function policy(codex = {}, general = {}) {
  const defaults = {
    maxAttempts: 5, baseDelayMs: 10, maxDelayMs: 10, totalBudgetMs: 10000, connectTimeoutMs: 2000,
    historyMode: 'on_error', historyConfirmations: 1,
    stripItemIds: true, rememberSession: true, repairStatus: true, retryHistory: true,
    emptyResponseRepair: false, emptyResponseDropReasoning: false,
    emptyResponseNormalizeAfter: 2, emptyResponseReasoningAfter: 6,
  }
  return {
    schemaVersion: 1,
    profiles: {
      codex: { ...defaults, ...codex },
      general: {
        ...defaults, stripItemIds: false, retryHistory: false,
        emptyResponseRepair: true, emptyResponseDropReasoning: true, ...general,
      },
    },
  }
}

const recoveredHistory = {
  model: 'fixture-model', store: false,
  input: [
    { type: 'message', role: 'user', content: 'policy fixture prompt' },
    { type: 'web_search_call', status: 'completed', action: { type: 'search', query: 'policy fixture query' } },
  ],
}
const crossResource = 'The requested item was created under a different Azure OpenAI resource'
const staleReasoning = 'encrypted content could not be verified'
const encryptedHistoryError = 'The encrypted content for item rs_00cbda8800eaf361006ad97abe00688196b46d51ab11f628fa could not be verified. Reason: Encrypted content could not be decrypted or parsed. (request id: 2026102203531550247527f9nrqKAP) [trace_id=905d77a03b5a684794bb9e7c2b652611]'
const toolHistory = {
  model: 'fixture-model',
  input: [
    { type: 'message', role: 'user', content: 'classification fixture prompt' },
    { type: 'reasoning', id: 'rs_classification', encrypted_content: 'classification fixture ciphertext' },
    { type: 'function_call', call_id: 'call_classification', name: 'fixture', arguments: '{"value":402}' },
    { type: 'function_call_output', call_id: 'call_classification', output: 'classification fixture result' },
  ],
}
const recoveredToolHistory = {
  model: 'fixture-model', input: [toolHistory.input[0], toolHistory.input[2], toolHistory.input[3]],
}

function classificationError(message, carrier = 'http', code, type = 'invalid_request_error', status = 400) {
  const body = { error: { message, type, ...(code === undefined ? {} : { code }) } }
  return {
    status: carrier === 'http' ? status : 200,
    type: carrier === 'sse' ? 'text/event-stream' : 'application/json',
    body: carrier === 'sse' ? `data: ${JSON.stringify(body)}\n\n` : body,
  }
}

function historyError(message = missingReasoning) {
  return classificationError(message)
}

async function listen(server) {
  server.listen(0, '127.0.0.1')
  await once(server, 'listening')
  const port = server.address().port
  assert.notEqual(port, 8787, '测试不得使用真实网关默认端口')
  return port
}

const legacyPolicy = Symbol('不设置新策略环境变量')

async function fixture(t, respond, settings = policy(), extraEnv = {}, startupOnly = false) {
  const dir = mkdtempSync(join(tmpdir(), 'ag-relay-policy-'))
  const captured = []
  let upstreamError
  const upstream = http.createServer(async (req, res) => {
    try {
      const chunks = []
      for await (const chunk of req) chunks.push(chunk)
      const raw = Buffer.concat(chunks).toString('utf8')
      const body = JSON.parse(raw)
      captured.push({ raw, body })
      const reply = await respond(body, captured.length, req)
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
        // 只清理本测试持有的子进程，绝不通过真实端口、PID 搜索或进程名杀进程。
        if (child.exitCode === null && child.signalCode === null) child.kill()
        await Promise.race([closed, delay(3000).then(() => { throw new Error('策略测试隔离核心未退出') })])
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
  // 不继承用户 API 凭据、NODE_OPTIONS 或其真实网关配置。
  const env = Object.fromEntries(Object.entries(process.env).filter(([key]) => /^(SystemRoot|WINDIR|TEMP|TMP)$/i.test(key)))
  Object.assign(env, {
    PROXY_HOST: '127.0.0.1', PROXY_PORT: String(port), PROXY_UPSTREAM: `http://127.0.0.1:${upstreamPort}`,
    PROXY_DIAGNOSTICS_FILE: join(dir, 'diagnostics.jsonl'), PROXY_VERBOSE: '0', PROXY_SUPERVISED: '1',
    PROXY_MAX_ATTEMPTS: '5', PROXY_DEPLOY_MAX_ATTEMPTS: '5', PROXY_BASE_DELAY_MS: '10',
    PROXY_MAX_DELAY_MS: '10', PROXY_DEPLOY_DELAY_MS: '10', PROXY_TOTAL_BUDGET_MS: '10000',
    PROXY_MIN_RETRY_WINDOW_MS: '100', PROXY_CONNECT_TIMEOUT_MS: '2000',
    ...(settings === legacyPolicy ? {} : { PROXY_POLICY_JSON: JSON.stringify(settings) }), ...extraEnv,
  })
  child = spawn(runtime, [core], { cwd: dir, env, windowsHide: true, stdio: ['ignore', 'ignore', 'pipe', 'ipc'] })
  closed = new Promise(resolve => child.once('close', resolve))
  let stderr = ''
  child.stderr.on('data', chunk => { stderr = (stderr + chunk).slice(-4096) })
  await new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error(`策略测试隔离核心就绪超时：${stderr}`)), 5000)
    child.once('error', error => { clearTimeout(timer); reject(error) })
    child.once('exit', code => { clearTimeout(timer); reject(new Error(`策略测试隔离核心退出 ${code}：${stderr}`)) })
    child.once('message', message => {
      clearTimeout(timer)
      if (message.type === 'ready') resolve()
      else reject(new Error('策略测试隔离核心未报告 ready'))
    })
  })
  // 无效配置测试只观察持有子进程的启动结果，绝不发 HTTP 探测请求。
  if (startupOnly) assert.fail('无效策略意外到达 ready：必须在监听前拒绝')
  // IPC 是自己持有的进程通道。绑定失败不访问端口，ready 后核对 PID 才发送业务请求。
  const url = `http://127.0.0.1:${port}`
  const getStatus = async () => {
    const response = await fetch(`${url}/_relay/status`, { signal: AbortSignal.timeout(2000) })
    assert.equal(response.status, 200)
    const status = await response.json()
    assert.equal(status.pid, child.pid)
    return status
  }
  await getStatus()
  return {
    captured, status: getStatus,
    diagnostics: () => readFileSync(join(dir, 'diagnostics.jsonl'), 'utf8').trim().split('\n').filter(Boolean).map(JSON.parse),
    async request(body = history, headers = {}, options = {}) {
      const response = await fetch(`${url}${options.path ?? '/v1/responses'}`, {
        method: 'POST', signal: options.signal ?? AbortSignal.timeout(12000),
        headers: {
          authorization: 'Bearer policy-fixture-only-not-a-key', 'content-type': 'application/json',
          'x-relay-client': 'codex', ...headers,
        },
        body: typeof body === 'string' ? body : JSON.stringify(body),
      })
      return { status: response.status, body: await response.text() }
    },
  }
}

const bindingMessage = 'session binding invalidated: the model this session is bound to can no longer serve it; restart the task with a new session id [trace_id=fixture code=30013]'
const bindingHistory = {
  ...history, prompt_cache_key: 'fixture-cache', client_metadata: { session_id: 'fixture-session', thread_id: 'fixture-thread' },
  input: [
    { ...history.input[0], status: 'completed' },
    { ...history.input[1], summary: [{ type: 'summary_text', text: 'retained visible reasoning' }] },
    { type: 'custom_tool_call', id: 'ct_fixture', call_id: 'call_fixture', name: 'fixture', input: '{"id":"nested-kept"}' },
    { type: 'custom_tool_call_output', id: 'cto_fixture', call_id: 'call_fixture', output: 'retained tool result' },
    history.input[2],
  ],
}
const bindingIdsOnly = { ...bindingHistory, input: bindingHistory.input.map(({ id, ...item }) => item) }
const bindingHeaders = {
  'user-agent': 'codex_cli_rs/fixture', originator: 'codex_cli_rs', 'session-id': 'fixture-session',
  'thread-id': 'fixture-thread', 'x-codex-window-id': 'fixture-window', 'x-codex-turn-metadata': '{"turn":"fixture"}',
  'x-relay-session-id': 'fixture-compatibility-key',
}
const bindingError = (carrier = 'http', code = 'session_binding_invalidated', message = bindingMessage) =>
  classificationError(message, carrier, code, 'invalid_request_error', 409)

for (const carrier of ['http', 'json', 'sse']) {
  test(`会话绑定 ${carrier} 错误后仅清编号并保留身份和推理`, async t => {
    const seenHeaders = []
    const f = await fixture(t, (_, count, req) => {
      seenHeaders.push({ ...req.headers })
      return count === 1 ? bindingError(carrier) : success
    }, policy({ stripItemIds: false }))
    assert.equal((await f.request(bindingHistory, bindingHeaders)).status, 200)
    assert.equal(f.captured.length, 2)
    assert.deepEqual(f.captured[0].body, bindingHistory)
    assert.deepEqual(f.captured[1].body, bindingIdsOnly)
    for (const [name, value] of Object.entries(bindingHeaders).filter(([name]) => name !== 'x-relay-session-id')) {
      assert.equal(seenHeaders[0][name], value)
      assert.equal(seenHeaders[1][name], value, `${name} 不得删改`)
    }
    assert.equal(seenHeaders[1].authorization, seenHeaders[0].authorization)
    assert.equal((await f.request(bindingHistory, bindingHeaders)).status, 200)
    assert.deepEqual(f.captured[2].body, bindingHistory, '不能记入会在下次删除推理的旧兼容缓存')
    const diagnostics = f.diagnostics()
    const repaired = diagnostics.find(row => row.event === 'repair_applied' && row.kind === 'session_binding_invalidated')
    assert.equal(repaired?.removedReasoning, 0)
    assert.equal(repaired?.removedIds, 5)
    assert.equal(diagnostics.some(row => row.event === 'session_compat_enabled'), false)
  })
  for (const failure of ['binding', 'history', 'status']) {
    test(`会话绑定 ${carrier} 修复后仍有 ${failure} 错误不升级删推理`, async t => {
      const f = await fixture(t, (_, count) => count === 1 || failure === 'binding'
        ? bindingError(carrier) : classificationError(failure === 'status' ? "Unknown parameter: 'input[0].status'." : staleReasoning, carrier), policy({ stripItemIds: false }))
      const result = await f.request(bindingHistory)
      assert.equal(result.status, carrier === 'http' && failure === 'binding' ? 409 : 400)
      assert.equal(f.captured.length, 2)
      assert.deepEqual(f.captured[1].body, bindingIdsOnly)
      assert.equal(f.diagnostics().some(row => row.event === 'session_compat_enabled'), false)
    })
  }
}

for (const [name, extraHeaders, settings, path] of [
  ['Claude', { 'x-relay-client': 'claude', 'user-agent': 'claude-code/fixture' }, policy({ stripItemIds: false }), '/v1/responses'],
  ['未知客户端', { 'x-relay-client': 'unknown' }, policy({ stripItemIds: false }), '/v1/responses'],
  ['身份冲突', { 'x-relay-client': 'codex', 'user-agent': 'claude-code/fixture' }, policy({ stripItemIds: false }), '/v1/responses'],
  ['Messages', {}, policy({ stripItemIds: false }), '/v1/messages'],
  ['关闭历史恢复', {}, policy({ stripItemIds: false, historyMode: 'off' }), '/v1/responses'],
]) {
  test(`会话绑定 不改变 ${name} 行为`, async t => {
    const f = await fixture(t, () => bindingError(), settings)
    assert.equal((await f.request(bindingHistory, extraHeaders, { path })).status, 409)
    assert.equal(f.captured.length, 1)
    assert.deepEqual(f.captured[0].body, bindingHistory)
  })
}

for (const [name, body] of [
  ['服务端指针', { ...bindingHistory, previous_response_id: 'resp_fixture' }],
  ['服务端会话', { ...bindingHistory, conversation: { id: 'conv_fixture' } }],
  ['引用项', { ...bindingHistory, input: [...bindingHistory.input, { type: 'item_reference', id: 'ref_fixture' }] }],
  ['压缩密文', { ...bindingHistory, input: [...bindingHistory.input, { type: 'compaction', encrypted_content: 'opaque' }] }],
  ['不完整工具', { ...bindingHistory, input: bindingHistory.input.filter(item => item.type !== 'custom_tool_call_output') }],
  ['只带编号的推理', { ...bindingHistory, input: bindingHistory.input.map(item => item.type === 'reasoning' ? { type: 'reasoning', id: 'rs_reference', summary: [] } : item) }],
  ['只带编号的空消息', { ...bindingHistory, input: bindingHistory.input.map(item => item.type === 'message' ? { ...item, content: [] } : item) }],
  ['没有可删编号', bindingIdsOnly],
]) {
  test(`会话绑定 ${name} 拒绝改写`, async t => {
    const f = await fixture(t, () => bindingError(), policy({ stripItemIds: false }))
    assert.equal((await f.request(body)).status, 409)
    assert.equal(f.captured.length, 1)
    assert.deepEqual(f.captured[0].body, body)
  })
}

for (const [name, code, message] of [
  ['仅具名错误码', 'session_binding_invalidated', 'provider error'],
  ['仅明确文案及数字码', 30013, bindingMessage],
]) {
  test(`会话绑定 识别${name}`, async t => {
    const f = await fixture(t, (_, count) => count === 1 ? bindingError('http', code, message) : success, policy({ stripItemIds: false }))
    assert.equal((await f.request(bindingHistory)).status, 200)
    assert.deepEqual(f.captured[1].body, bindingIdsOnly)
  })
}

test('会话绑定 不按单独数字码或无关冲突误修复', async t => {
  const f = await fixture(t, () => bindingError('http', '30013', 'unrelated conflict'), policy({ stripItemIds: false }))
  assert.equal((await f.request(bindingHistory)).status, 409)
  assert.equal(f.captured.length, 1)
})

for (const status of [401, 402, 403, 429]) {
  test(`会话绑定 不覆盖 ${status} 的明确含义`, async t => {
    const f = await fixture(t, (_, count) => count === 1
      ? classificationError(bindingMessage, 'http', 'session_binding_invalidated', 'invalid_request_error', status) : success,
    policy({ stripItemIds: false }))
    const result = await f.request(bindingHistory)
    assert.equal(result.status, status === 429 ? 200 : status)
    assert.ok(f.captured.every(row => JSON.stringify(row.body) === JSON.stringify(bindingHistory)))
  })
}

test('会话绑定 首发既有清理后仅再去推理和搜索编号', async t => {
  const f = await fixture(t, (_, count) => count === 1 ? bindingError() : success)
  assert.equal((await f.request(bindingHistory, bindingHeaders)).status, 200)
  assert.deepEqual(f.captured[0].body.input.filter(item => item.id).map(item => item.type), ['reasoning', 'web_search_call'])
  assert.deepEqual(f.captured[1].body, bindingIdsOnly)
})

test('会话绑定 尊重确认次数且修复后限流仍保留完整推理', async t => {
  const f = await fixture(t, (_, count) => count < 3 ? bindingError()
    : count === 3 ? classificationError('rate limit', 'http', 'rate_limit_exceeded', 'rate_limit_error', 429) : success,
  policy({ stripItemIds: false, historyConfirmations: 2 }))
  assert.equal((await f.request(bindingHistory)).status, 200)
  assert.equal(f.captured.length, 4)
  assert.deepEqual(f.captured.map(row => row.body), [bindingHistory, bindingHistory, bindingIdsOnly, bindingIdsOnly])
})

test('会话绑定 常规最后一次失败仍只追加一次编号修复', async t => {
  const f = await fixture(t, (_, count) => count === 1 ? bindingError() : success,
    policy({ stripItemIds: false, maxAttempts: 1 }))
  assert.equal((await f.request(bindingHistory)).status, 200)
  assert.equal(f.captured.length, 2)
  assert.deepEqual(f.captured[1].body, bindingIdsOnly)
})

test('会话绑定 等待修复时取消不再发送', async t => {
  let seen
  const received = new Promise(resolve => { seen = resolve })
  const f = await fixture(t, () => { seen(); return bindingError() }, policy({ stripItemIds: false }), { PROXY_DEPLOY_DELAY_MS: '500' })
  const controller = new AbortController()
  const pending = f.request(bindingHistory, {}, { signal: controller.signal })
  await received
  await delay(20)
  controller.abort()
  await assert.rejects(pending)
  await delay(80)
  assert.equal(f.captured.length, 1)
})

test('会话绑定 内容交付后不重放', async t => {
  const stream = `data: ${JSON.stringify({ type: 'response.output_text.delta', delta: 'already delivered' })}\n\n${bindingError('sse').body}`
  const f = await fixture(t, () => ({ status: 200, type: 'text/event-stream', body: stream }), policy({ stripItemIds: false }))
  const result = await f.request(bindingHistory)
  assert.equal(result.status, 200)
  assert.equal(result.body, stream)
  assert.equal(f.captured.length, 1)
})

for (const client of ['claude', 'codex']) {
  test(`错误分类 ${client} 加密历史报错中的请求编号不阻止修复`, async t => {
    const f = await fixture(t, (_, count) => count === 1 ? historyError(encryptedHistoryError) : success,
      policy({ stripItemIds: false, rememberSession: false }, { rememberSession: false }))
    const response = await f.request(toolHistory, { 'x-relay-client': client })
    assert.equal(response.status, 200, response.body)
    assert.equal(f.captured.length, 2)
    assert.deepEqual(f.captured[0].body, toolHistory)
    assert.deepEqual(f.captured[1].body, recoveredToolHistory, '只移除失效推理，保留消息与完整工具配对')
  })
}

for (const carrier of ['http', 'json', 'sse']) {
  test(`错误分类 ${carrier} 编号与URL噪声不改变历史恢复`, async t => {
    const noise = '(request id: 502) [trace_id=gateway_balance_unauthorized_content-blocked] https://example.invalid/503/504/payment-required'
    const f = await fixture(t, (_, count) => count === 1
      ? classificationError(`${staleReasoning}. ${noise}`, carrier) : success,
    policy({ stripItemIds: false, rememberSession: false }, { rememberSession: false }))
    const response = await f.request(toolHistory, { 'x-relay-client': 'claude' })
    assert.equal(response.status, 200, response.body)
    assert.equal(f.captured.length, 2)
    assert.deepEqual(f.captured[1].body, recoveredToolHistory)
  })
}

for (const carrier of ['http', 'json', 'sse']) {
  test(`错误分类 ${carrier} 具体临时错误码原体重试而不删历史`, async t => {
    const raw = ` \n${JSON.stringify(toolHistory)}\n `
    const reply = classificationError(`${staleReasoning}. [trace_id=balance_502]`, carrier, 'server_error')
    const f = await fixture(t, (_, count) => count === 1 ? reply : success,
      policy({ stripItemIds: false, rememberSession: false }, { rememberSession: false }))
    const response = await f.request(raw, { 'x-relay-client': 'claude' })
    assert.equal(response.status, 200, response.body)
    assert.equal(f.captured.length, 2)
    assert.ok(f.captured.every(request => request.raw === raw), '具体临时错误必须原体重试，不触发历史修复')
  })
}

test('错误分类 HTTP错误信封外的调试字段不把临时故障变成永久错误', async t => {
  const raw = ` \n${JSON.stringify(toolHistory)}\n `
  const reply = {
    status: 503,
    body: { error: { code: 'server_error', message: 'temporary outage' }, debug: { note: 'balance invalid_request_error unauthorized' } },
  }
  const f = await fixture(t, (_, count) => count === 1 ? reply : success)
  const response = await f.request(raw, { 'x-relay-client': 'claude' })
  assert.equal(response.status, 200, response.body)
  assert.equal(f.captured.length, 2)
  assert.ok(f.captured.every(request => request.raw === raw))
})

for (const [name, message, code] of [
  ['裸gateway', `${staleReasoning}. The gateway rejected this history`, undefined],
  ['裸balance', `${staleReasoning}. Preserve the balance of this history`, undefined],
  ['单词子串', `${staleReasoning}. flaggedness unauthorizedness`, undefined],
  ['伪错误码', staleReasoning, 'prefix_invalid_api_key_suffix'],
  ['非字符串字段', staleReasoning, { nested: 'invalid_api_key' }],
]) {
  test(`错误分类 ${name} 不是明确故障原因，仍允许历史修复`, async t => {
    const reply = classificationError(message, 'json', code)
    const f = await fixture(t, (_, count) => count === 1 ? reply : success)
    const response = await f.request(toolHistory, { 'x-relay-client': 'claude' })
    assert.equal(response.status, 200, response.body)
    assert.equal(f.captured.length, 2)
    assert.deepEqual(f.captured[1].body, recoveredToolHistory)
  })
}

for (const carrier of ['http', 'json', 'sse']) {
  test(`错误分类 ${carrier} 明确永久原因优先于部署和历史文案`, async t => {
    const reply = classificationError(`Insufficient balance; no deployment to match this model; ${staleReasoning}`, carrier, 'insufficient_quota')
    const f = await fixture(t, () => reply)
    const response = await f.request(toolHistory, { 'x-relay-client': 'claude' })
    assert.equal(response.status, carrier === 'http' ? 400 : 402, response.body)
    assert.equal(f.captured.length, 1)
    assert.deepEqual(f.captured[0].body, toolHistory)
  })
}

for (const status of [401, 402, 403]) {
  test(`错误分类 真实HTTP ${status} 不被暂时错误码或字段修复文案覆盖`, async t => {
    const body = { input: [{ type: 'message', role: 'user', content: 'fixture', status: 'completed' }] }
    const reply = classificationError("Unknown parameter: 'input[0].status'.", 'http', 'server_error', undefined, status)
    const f = await fixture(t, () => reply)
    const response = await f.request(body)
    assert.equal(response.status, status, response.body)
    assert.equal(f.captured.length, 1)
    assert.deepEqual(f.captured[0].body, body)
  })
}

test('错误分类 仅第一句加密条目校验失败也可识别，条目编号不参与判断', async t => {
  const message = 'The encrypted content for item rs_gateway_balance_502 could not be verified. [trace_id=503]'
  const f = await fixture(t, (_, count) => count === 1 ? historyError(message) : success)
  const response = await f.request(toolHistory, { 'x-relay-client': 'claude' })
  assert.equal(response.status, 200, response.body)
  assert.equal(f.captured.length, 2)
  assert.deepEqual(f.captured[1].body, recoveredToolHistory)
})

test('错误分类 明确上下文超限文案不能被部署缺失覆盖', async t => {
  const reply = classificationError('maximum context length exceeded; no deployment to match this model', 'json')
  const f = await fixture(t, () => reply)
  const response = await f.request(toolHistory)
  assert.equal(response.status, 400, response.body)
  assert.equal(f.captured.length, 1)
})

for (const carrier of ['http', 'json', 'sse']) {
  test(`错误分类 ${carrier} 旧配置的鉴权错误与明确历史错误分别处理`, async t => {
    const replies = [classificationError(staleReasoning, carrier, 'invalid_api_key'), classificationError(staleReasoning, carrier), success]
    const f = await fixture(t, (_, count) => replies[count - 1], legacyPolicy)
    const failed = await f.request(toolHistory, { 'x-relay-client': 'claude' })
    assert.equal(failed.status, 400, failed.body)
    assert.equal(f.captured.length, 1)
    assert.deepEqual(f.captured[0].body, toolHistory)
    assert.equal((await f.request(toolHistory, { 'x-relay-client': 'claude' })).status, 200)
    assert.equal(f.captured.length, 3)
    assert.deepEqual(f.captured[2].body, recoveredToolHistory)
  })
}

test('错误分类 401/402/403/408/429/502/503/504仅在编号中出现不改变恢复', async t => {
  const tokens = ['401', '402', '403', '408', '429', '502', '503', '504']
  const f = await fixture(t, (_, count) => count % 2
    ? historyError(`${staleReasoning}. (request id: ${tokens[(count - 1) / 2]})`) : success,
  policy({ rememberSession: false }, { rememberSession: false }))
  for (const token of tokens) {
    assert.equal((await f.request(toolHistory, { 'x-relay-client': 'claude' })).status, 200, token)
  }
  assert.equal(f.captured.length, 16)
  assert.ok(f.captured.filter((_, index) => index % 2).every(request => JSON.stringify(request.body) === JSON.stringify(recoveredToolHistory)))
})

for (const [name, reply] of [
  ['无错误信封的400', { status: 400, body: { detail: 'no deployment invalid_request_error balance 502', output: { code: 'invalid_api_key' } } }],
  ['字段类型错误的400', { status: 400, body: { error: { code: ['invalid_api_key'], type: { name: 'server_error' }, message: { text: 'no deployment' } } } }],
  ['成功JSON', { status: 200, body: { output: [{ type: 'message', content: 'invalid_api_key balance 402 502 no deployment' }] } }],
  ['成功纯文本', { status: 200, type: 'text/plain', body: 'invalid_api_key balance 402 502 no deployment' }],
  ['成功SSE', { status: 200, type: 'text/event-stream', body: 'data: {"type":"response.output_text.delta","delta":"invalid_api_key balance 402 502 no deployment"}\n\ndata: {"type":"response.completed"}\n\n' }],
]) {
  test(`错误分类 ${name} 无关字段与正文逐字节保留`, async t => {
    const f = await fixture(t, () => reply)
    const response = await f.request(toolHistory)
    assert.equal(response.status, reply.status, response.body)
    assert.equal(response.body, typeof reply.body === 'string' ? reply.body : JSON.stringify(reply.body))
    assert.equal(f.captured.length, 1)
  })
}

test('错误分类 明确状态表达是暂时原因而不是编号', async t => {
  const reply = classificationError(`${staleReasoning}. HTTP status: 502`, 'http')
  const f = await fixture(t, (_, count) => count === 1 ? reply : success)
  const response = await f.request(toolHistory, { 'x-relay-client': 'claude' })
  assert.equal(response.status, 200, response.body)
  assert.equal(f.captured.length, 2)
  assert.ok(f.captured.every(request => JSON.stringify(request.body) === JSON.stringify(toolHistory)))
})

test('错误分类 字段修复保留参数下标并忽略trace编号', async t => {
  const body = { input: [{ type: 'message', role: 'user', content: 'fixture', status: 'completed' }] }
  const reply = historyError("Unknown parameter: 'input[0].status'. [trace_id=balance_502]")
  const f = await fixture(t, (_, count) => count === 1 ? reply : success)
  const response = await f.request(body)
  assert.equal(response.status, 200, response.body)
  assert.equal(f.captured.length, 2)
  assert.deepEqual(f.captured[1].body, { input: [{ type: 'message', role: 'user', content: 'fixture' }] })
})

for (const carrier of ['http', 'json', 'sse']) {
  test(`错误分类 ${carrier} 部署不存在仍换通道且不改历史`, async t => {
    const reply = classificationError('The requested deployment does not exist; no deployment to match this model.', carrier)
    const f = await fixture(t, (_, count) => count === 1 ? reply : success)
    const response = await f.request(toolHistory, { 'x-relay-client': 'claude' })
    assert.equal(response.status, 200, response.body)
    assert.equal(f.captured.length, 2)
    assert.ok(f.captured.every(request => JSON.stringify(request.body) === JSON.stringify(toolHistory)))
  })
}

// 生产回归：中文站点把「该模型在该分组没有通道」说成「无可用渠道」，原分类只认英文，
// 于是落进通用 503 重试，把 Codex 后台辅助请求拖满 110 次、约 200 秒才放弃。
test('错误分类 中文无可用渠道按通道抖动快速换道并收手', async t => {
  const reply = { status: 503, body: { error: { message: '当前分组 core 下模型 gpt-5.6-terra 无可用渠道', type: 'new_api_error' } } }
  const f = await fixture(t, () => reply)
  const started = Date.now()
  const response = await f.request(toolHistory, { 'x-relay-client': 'codex' })
  const elapsed = Date.now() - started
  assert.equal(response.status, 429, '通道抖动耗尽按可重试的错误返回：' + response.body)
  assert.ok(f.captured.length <= 8, `应在通道抖动预算内收手，实际发送 ${f.captured.length} 次`)
  assert.ok(elapsed < 5000, `不应拖满通用重试预算，实际 ${elapsed}ms`)
  assert.ok(!f.captured.some(request => JSON.stringify(request.body).includes('"id"')),
    '通道抖动不得改写或删除历史')
})

for (const code of ['content-filter', 'flagged', 'moderation']) {
  test(`错误分类 仅${code}错误码的审核失败仍及时返回`, async t => {
    const reply = classificationError('provider error', 'sse', code)
    const f = await fixture(t, () => reply)
    const response = await f.request(toolHistory)
    assert.equal(response.status, 400, response.body)
    assert.equal(f.captured.length, 1)
  })
}

test('错误分类 Claude跨资源原因不被附带Codex缺推理句式阻断', async t => {
  const reply = historyError(`${crossResource}. ${missingReasoning}`)
  const f = await fixture(t, (_, count) => count === 1 ? reply : success)
  const response = await f.request(toolHistory, { 'x-relay-client': 'claude' })
  assert.equal(response.status, 200, response.body)
  assert.equal(f.captured.length, 2)
  assert.deepEqual(f.captured[1].body, recoveredToolHistory)
})

for (const carrier of ['http', 'json', 'sse']) {
  test(`错误分类 ${carrier} 具体code优先于备用type`, async t => {
    const reply = classificationError('provider error', carrier, 'server_error', 'authentication_error')
    const f = await fixture(t, (_, count) => count === 1 ? reply : success)
    const response = await f.request(toolHistory)
    assert.equal(response.status, 200, response.body)
    assert.equal(f.captured.length, 2)
  })
}

test('policy 两组重试参数独立：Codex 两次、general 四次并回报实际设置', async t => {
  const settings = policy({ maxAttempts: 2 }, { maxAttempts: 4 })
  const f = await fixture(t, () => ({ status: 503, body: { error: { code: 'server_error', message: 'fixture unavailable' } } }), settings)
  const request = { model: 'fixture-model', input: 'policy retry fixture' }
  assert.equal((await f.request(request)).status, 503)
  assert.equal(f.captured.length, 2, 'Codex 必须遵守自己的两次发送上限，而非旧 env 五次')
  assert.equal((await f.request(request, { 'x-relay-client': 'claude' })).status, 503)
  assert.equal(f.captured.length, 6, 'general 必须独立使用四次上限，而非继承 Codex 两次')
  const status = await f.status()
  assert.deepEqual(status.policy?.settings, settings, '状态回执必须是实际生效的完整双 profile 设置')
  assert.match(status.policy?.revision ?? '', /^[a-f0-9]{64}$/, '状态回执须提供策略修订号，不在测试中复制哈希实现')
})

for (const confirmations of [1, 2, 3, 4]) {
  test(`policy 历史确认 ${confirmations}：门槛前逐字节保留，到门槛才整组降级`, async t => {
    const raw = ` \n${JSON.stringify(history)}\n `
    const f = await fixture(t, (_, count) => count <= confirmations ? historyError() : success,
      policy({ historyConfirmations: confirmations, stripItemIds: false, rememberSession: false }))
    assert.equal((await f.request(raw)).status, 200)
    assert.equal(f.captured.length, confirmations + 1)
    for (const request of f.captured.slice(0, confirmations)) assert.equal(request.raw, raw)
    assert.deepEqual(f.captured[confirmations].body, recoveredHistory)
  })
}

test('policy 历史确认掺入限流：限流不计数，也不清除当前请求已有证据', async t => {
  const raw = ` \n${JSON.stringify(history)}\n `
  const replies = [historyError(), {
    status: 429, body: { error: { code: 'rate_limit_exceeded', message: missingReasoning } },
  }, historyError(), success]
  const f = await fixture(t, (_, count) => replies[count - 1] ?? success,
    policy({ historyConfirmations: 2, stripItemIds: false, rememberSession: false }))
  assert.equal((await f.request(raw)).status, 200)
  assert.equal(f.captured.length, 4)
  for (const request of f.captured.slice(0, 3)) assert.equal(request.raw, raw)
  assert.deepEqual(f.captured[3].body, recoveredHistory)
})

test('policy 历史确认分类型：跨资源与失效推理分别累计', async t => {
  const raw = ` \n${JSON.stringify(history)}\n `
  const replies = [historyError(crossResource), historyError(staleReasoning), historyError(crossResource), success]
  const f = await fixture(t, (_, count) => replies[count - 1] ?? success,
    policy({ historyConfirmations: 2, stripItemIds: false, rememberSession: false }))
  assert.equal((await f.request(raw)).status, 200)
  assert.equal(f.captured.length, 4)
  for (const request of f.captured.slice(0, 3)) assert.equal(request.raw, raw)
  assert.deepEqual(f.captured[3].body, recoveredHistory)
})

test('policy 提前整组降级：Codex 搜索与普通、自定义工具内容完整保留', async t => {
  const tools = [
    { type: 'function_call', call_id: 'call_policy_function', name: 'fixture_lookup', arguments: '{"id":"nested_keep"}' },
    { type: 'function_call_output', call_id: 'call_policy_function', output: 'policy function result' },
    { type: 'custom_tool_call', call_id: 'call_policy_custom', name: 'fixture_custom', input: '{"encrypted_content":"nested_keep"}' },
    { type: 'custom_tool_call_output', call_id: 'call_policy_custom', output: [{ type: 'input_text', text: 'policy custom result' }] },
  ]
  const action = { type: 'search', queries: ['first query', 'second query'], sources: [{ type: 'url', url: 'https://example.invalid/policy', id: 'source_keep' }] }
  const body = structuredClone(history)
  body.input[2].action = action
  body.input.push(...tools.map((item, index) => ({ id: `tool_policy_${index}`, ...item })))
  const f = await fixture(t, () => success, policy({ historyMode: 'always' }))
  assert.equal((await f.request(body)).status, 200)
  assert.equal(f.captured.length, 1, '提前模式不需要先制造一次上游错误')
  assert.deepEqual(f.captured[0].body, {
    model: 'fixture-model', store: false,
    input: [
      { type: 'message', role: 'user', content: 'policy fixture prompt' },
      { type: 'web_search_call', status: 'completed', action }, ...tools,
    ],
  })
})

for (const [name, extra, item] of [
  ['previous_response_id', { previous_response_id: 'resp_policy_server_only' }],
  ['conversation', { conversation: { id: 'conv_policy_server_only' } }],
  ['item_reference', {}, { type: 'item_reference', id: 'msg_policy_server_only' }],
  ['compaction', {}, { type: 'compaction', encrypted_content: 'opaque policy fixture' }],
  ['未知项', {}, { type: 'future_tool', id: 'opaque_policy_fixture' }],
  ['工具缺结果', {}, { type: 'function_call', call_id: 'call_policy_orphan', name: 'fixture', arguments: '{}' }],
  ['孤立工具结果', {}, { type: 'function_call_output', call_id: 'call_policy_orphan', output: 'fixture result' }],
  ['非推理密文', {}, { type: 'message', role: 'assistant', content: 'fixture text', encrypted_content: 'must keep' }],
  ['未知搜索动作', {}, { type: 'web_search_call', id: 'ws_unknown', status: 'completed', action: { type: 'future_action' } }],
]) {
  test(`policy 提前校验失败 ${name}：包括 message.id 在内整个原始 body 逐字节保留`, async t => {
    const body = { ...structuredClone(history), ...extra }
    if (item) body.input.push(item)
    const raw = ` \n${JSON.stringify(body, null, 2)}\n `
    const f = await fixture(t, () => success,
      policy({ historyMode: 'always', stripItemIds: true, rememberSession: true }))
    assert.equal((await f.request(raw)).status, 200)
    assert.equal(f.captured.length, 1)
    assert.equal(f.captured[0].raw, raw, '拒绝整组提前降级后，不能再经 strip/cache 路径局部改写')
  })
}

test('policy general 提前模式不套 Codex 搜索专用降级变体', async t => {
  const raw = ` \n${JSON.stringify(history)}\n `
  const f = await fixture(t, () => success, policy({}, { historyMode: 'always', stripItemIds: false }))
  assert.equal((await f.request(raw, { 'x-relay-client': 'claude' })).status, 200)
  assert.equal(f.captured.length, 1)
  assert.equal(f.captured[0].raw, raw, 'general 无法证明搜索可完整重放时拒绝整组提前变更')
})

for (const [name, outputs] of [['缺工具结果', 0], ['重复工具结果', 2]]) {
  test(`policy general 提前${name}：拒绝不完整配对并保留整个原始 body`, async t => {
    const output = { type: 'function_call_output', call_id: 'call_general_policy', output: 'fixture result' }
    const body = {
      model: 'fixture-model', input: [
        { type: 'message', id: 'msg_general_policy', role: 'user', content: 'fixture request' },
        { type: 'reasoning', id: 'rs_general_policy', encrypted_content: 'fixture ciphertext' },
        { type: 'function_call', id: 'fc_general_policy', call_id: 'call_general_policy', name: 'fixture', arguments: '{}' },
        ...Array.from({ length: outputs }, () => ({ ...output })),
      ],
    }
    const raw = ` \n${JSON.stringify(body)}\n `
    const f = await fixture(t, () => success, policy({}, { historyMode: 'always' }))
    assert.equal((await f.request(raw, { 'x-relay-client': 'claude' })).status, 200)
    assert.equal(f.captured.length, 1)
    assert.equal(f.captured[0].raw, raw, 'general 提前模式也须验证一一完整配对，不能部分删除历史')
  })
}

for (const rememberSession of [false, true]) {
  test(`policy 会话记忆 ${rememberSession ? 'on' : 'off'}：只在允许时改变同会话下一次首发`, async t => {
    const raw = ` \n${JSON.stringify(history)}\n `
    const f = await fixture(t, (_, count) => count === 1 ? historyError() : success,
      policy({ stripItemIds: false, rememberSession }))
    const headers = { 'x-relay-session-id': 'policy-memory-fixture' }
    assert.equal((await f.request(raw, headers)).status, 200)
    assert.equal(f.captured.length, 2)
    assert.equal(f.captured[0].raw, raw)
    assert.deepEqual(f.captured[1].body, recoveredHistory)
    assert.equal((await f.request(raw, headers)).status, 200)
    assert.equal(f.captured.length, 3)
    if (rememberSession) assert.deepEqual(f.captured[2].body, recoveredHistory)
    else assert.equal(f.captured[2].raw, raw, '记忆关闭不能经缓存提前剥离下一请求')
  })
}

for (const stripItemIds of [false, true]) {
  test(`policy history off 与首发编号清理 ${stripItemIds ? 'on' : 'off'} 互相独立`, async t => {
    const raw = ` \n${JSON.stringify(history)}\n `
    const f = await fixture(t, () => success, policy({ historyMode: 'off', stripItemIds }))
    assert.equal((await f.request(raw)).status, 200)
    assert.equal(f.captured.length, 1)
    if (!stripItemIds) assert.equal(f.captured[0].raw, raw)
    else assert.deepEqual(f.captured[0].body, {
      model: 'fixture-model', store: false,
      input: [recoveredHistory.input[0], history.input[1], history.input[2]],
    }, '只清可选编号，搜索关联的推理及编号仍保留')
  })
}

test('policy Codex history off 不得误落到 general 空响应形状修复', async t => {
  const body = { model: 'fixture-model', input: history.input.slice(0, 2) }
  const raw = ` \n${JSON.stringify(body)}\n `
  const f = await fixture(t, (_, count) => count <= 2
    ? { status: 200, type: 'text/event-stream', body: '' } : success,
  policy({ historyMode: 'off', stripItemIds: false }))
  assert.equal((await f.request(raw)).status, 200)
  assert.equal(f.captured.length, 3)
  assert.ok(f.captured.every(request => request.raw === raw), 'Codex 身份不能被 history 开关抹掉')
})

for (const retryHistory of [false, true]) {
  test(`policy 保留历史重试 ${retryHistory ? 'on' : 'off'}：不能改写时遵守独立开关`, async t => {
    const raw = ` \n${JSON.stringify({ ...history, previous_response_id: 'resp_retry_policy' })}\n `
    const f = await fixture(t, () => historyError(crossResource),
      policy({ maxAttempts: 3, stripItemIds: false, retryHistory }))
    assert.equal((await f.request(raw)).status, retryHistory ? 503 : 400)
    assert.equal(f.captured.length, retryHistory ? 3 : 1)
    assert.ok(f.captured.every(request => request.raw === raw))
  })
}

for (const client of ['codex', 'claude']) {
  for (const repairStatus of [false, true]) {
    for (const carrier of ['http', 'json', 'sse']) {
      test(`policy status 精确拒绝 ${client}/${carrier}/${repairStatus ? 'on' : 'off'}`, async t => {
        const body = { input: [{ type: 'message', role: 'user', content: 'status fixture', status: 'completed' }] }
        const raw = ` \n${JSON.stringify(body)}\n `
        const error = { error: { type: 'invalid_request_error', message: "Unknown parameter: 'input[0].status'." } }
        const reply = {
          status: carrier === 'http' ? 400 : 200,
          type: carrier === 'sse' ? 'text/event-stream' : 'application/json',
          body: carrier === 'sse' ? `data: ${JSON.stringify(error)}\n\n` : error,
        }
        const profile = { historyMode: 'off', repairStatus }
        const f = await fixture(t, (_, count) => count === 1 ? reply : success, policy(profile, profile))
        const response = await f.request(raw, { 'x-relay-client': client })
        assert.equal(response.status, repairStatus || carrier !== 'http' ? 200 : 400)
        assert.equal(f.captured[0].raw, raw)
        if (repairStatus) {
          assert.equal(f.captured.length, 2)
          assert.deepEqual(f.captured[1].body, {
            input: [{ type: 'message', role: 'user', content: 'status fixture' }],
          })
        } else {
          // 关闭字段修复不重新定义旧错误信封/通用重试，只禁止任何尝试偷偷删除字段。
          assert.ok(f.captured.every(request => request.raw === raw))
          if (carrier === 'http') assert.equal(f.captured.length, 1)
        }
      })
    }
  }
}

test('policy status 非精确拒绝不删除字段', async t => {
  const raw = ' {"input":[{"type":"message","role":"user","content":"status fixture","status":"completed"}]} '
  const f = await fixture(t, () => historyError('The status is invalid for this account'),
    policy({ repairStatus: true }))
  assert.equal((await f.request(raw)).status, 400)
  assert.equal(f.captured.length, 1)
  assert.equal(f.captured[0].raw, raw)
})

for (const source of ['empty', 'history']) {
  test(`policy general 提前拒绝不被 ${source} 旁路：整次请求保持工具原始字节`, async t => {
    const raw = ' \n{"input":[{"type":"message","id":"msg_keep","role":"user","content":"fixture"},{"type":"reasoning","id":"rs_keep","encrypted_content":"fixture"},{"type":"function_call","id":"fc_keep","call_id":"call_keep","name":"fixture","arguments":"{}"}]}\n '
    const f = await fixture(t, (_, count) => source === 'empty'
      ? (count <= 2 ? { status: 200, type: 'text/event-stream', body: '' } : success)
      : (count === 1 ? historyError(crossResource) : success), policy({}, { historyMode: 'always' }))
    const response = await f.request(raw, { 'x-relay-client': 'claude' })
    if (source === 'empty') {
      assert.equal(response.status, 200)
      assert.equal(f.captured.length, 3)
    }
    assert.ok(f.captured.length >= 1)
    for (const request of f.captured) assert.equal(request.raw, raw, '提前整组拒绝之后，空响应与明确历史错误均不能部分改写')
  })
}

for (const [name, overrides, normalizeAfter, reasoningAfter] of [
  ['默认2/6', {}, 2, 6],
  ['自定义1/3', { emptyResponseNormalizeAfter: 1, emptyResponseReasoningAfter: 3 }, 1, 3],
  ['history off仍独立2/6', { historyMode: 'off' }, 2, 6],
]) {
  test(`policy general 空响应两级修复 ${name}：只在对应门槛后改变请求`, async t => {
    const body = { input: [
      { type: 'message', id: 'msg_shape', role: 'user', content: 'shape fixture', status: 'completed' },
      { type: 'reasoning', id: 'rs_shape', summary: [], encrypted_content: 'shape ciphertext' },
    ] }
    const normalized = { input: [
      { role: 'user', content: 'shape fixture', status: 'completed' },
      { type: 'reasoning', summary: [], encrypted_content: 'shape ciphertext' },
    ] }
    const dropped = { input: [{ role: 'user', content: 'shape fixture', status: 'completed' }] }
    const raw = ` \n${JSON.stringify(body)}\n `
    const f = await fixture(t, (_, count) => count <= reasoningAfter
      ? { status: 200, type: 'text/event-stream', body: '' } : success,
    policy({}, { maxAttempts: 8, rememberSession: false, ...overrides }))
    assert.equal((await f.request(raw, { 'x-relay-client': 'claude' })).status, 200)
    assert.equal(f.captured.length, reasoningAfter + 1)
    for (const request of f.captured.slice(0, normalizeAfter)) assert.equal(request.raw, raw)
    for (const request of f.captured.slice(normalizeAfter, reasoningAfter)) assert.deepEqual(request.body, normalized)
    assert.deepEqual(f.captured[reasoningAfter].body, dropped)
  })
}

for (const [name, overrides, alreadyNormalized] of [
  ['一级关闭', { emptyResponseRepair: false, emptyResponseDropReasoning: false }, false],
  ['二级关闭', { emptyResponseDropReasoning: false }, false],
  ['一级没有实际改动', {}, true],
]) {
  test(`policy general 空响应 ${name}：不能绕过开关或空转升级删除推理`, async t => {
    const normalized = { input: [
      { role: 'user', content: 'shape fixture' },
      { type: 'reasoning', encrypted_content: 'shape ciphertext' },
    ] }
    const body = alreadyNormalized ? normalized : { input: [
      { type: 'message', id: 'msg_shape', role: 'user', content: 'shape fixture' },
      { type: 'reasoning', id: 'rs_shape', encrypted_content: 'shape ciphertext' },
    ] }
    const raw = ` \n${JSON.stringify(body)}\n `
    const f = await fixture(t, (_, count) => count <= 6
      ? { status: 200, type: 'text/event-stream', body: '' } : success,
    policy({}, { maxAttempts: 8, ...overrides }))
    assert.equal((await f.request(raw, { 'x-relay-client': 'claude' })).status, 200)
    assert.equal(f.captured.length, 7)
    for (const [index, request] of f.captured.entries()) {
      if (name === '二级关闭' && index >= 2) assert.deepEqual(request.body, normalized)
      else assert.equal(request.raw, raw)
    }
  })
}

for (const [name, invalid] of [
  ['顶层null', null], ['顶层数组', []], ['顶层错误类型', 'invalid'],
  ['版本null', { schemaVersion: null }], ['未知版本', { schemaVersion: 2 }],
  ['未知顶层字段', { futureSetting: true }], ['profiles为null', { profiles: null }],
  ['未知profile', { profiles: { other: {} } }], ['codex为null', { profiles: { codex: null } }],
  ['未知profile字段', policy({ futureSetting: true })], ['整数是字符串', policy({ maxAttempts: '5' })],
  ['布尔是数字', policy({ rememberSession: 1 })], ['布尔是null', policy({ stripItemIds: null })],
  ['模式不存在', policy({ historyMode: 'automatic' })], ['次数为零', policy({ maxAttempts: 0 })],
  ['确认超Codex上限', policy({ historyConfirmations: 5 })],
  ['确认超发送上限', policy({ maxAttempts: 2, historyConfirmations: 3 })],
  ['间隔倒挂', policy({ baseDelayMs: 20, maxDelayMs: 10 })],
  ['阶段倒挂', policy({}, { emptyResponseNormalizeAfter: 6, emptyResponseReasoningAfter: 2 })],
  ['二级开启一级关闭', policy({}, { emptyResponseRepair: false, emptyResponseDropReasoning: true })],
  ['Codex不支持empty模块', policy({ emptyResponseRepair: true })],
  ['general不支持strip模块', policy({}, { stripItemIds: true })],
  ['general不支持历史保留重试模块', policy({}, { retryHistory: true })],
]) {
  test(`policy invalid 启动前拒绝：${name}`, async t => {
    await assert.rejects(fixture(t, () => { assert.fail('无效配置不能访问假上游') }, invalid, {}, true), /Invalid PROXY_POLICY_JSON/)
  })
}

test('policy invalid 启动前拒绝：无法解析的 JSON', async t => {
  await assert.rejects(fixture(t, () => { assert.fail('无效配置不能访问假上游') }, policy(),
    { PROXY_POLICY_JSON: '{not-json' }, true), /Invalid PROXY_POLICY_JSON/)
})

test('policy 新env完全未设置：保留旧env的首发编号开关', async t => {
  const raw = ` \n${JSON.stringify(history)}\n `
  const f = await fixture(t, () => success, legacyPolicy, { PROXY_CODEX_STRIP_IDS: '0' })
  assert.equal((await f.request(raw)).status, 200)
  assert.equal(f.captured.length, 1)
  assert.equal(f.captured[0].raw, raw)
})

test('policy Codex history off：明确历史错误也不得剥离推理或任何条目编号', async t => {
  const raw = ` \n${JSON.stringify(history)}\n `
  const f = await fixture(t, (_, count) => count === 1 ? historyError() : success,
    policy({ historyMode: 'off', stripItemIds: false, rememberSession: true }))
  await f.request(raw)
  assert.ok(f.captured.length >= 1, '假上游必须实际收到请求')
  for (const [index, request] of f.captured.entries()) {
    assert.equal(request.raw, raw, `historyMode=off 时第 ${index + 1} 次请求必须逐字节保留原始 body`)
  }
})
