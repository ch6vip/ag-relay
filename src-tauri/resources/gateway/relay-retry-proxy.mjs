#!/usr/bin/env node
/**
 * relay-retry-proxy —— 中转站「HTTP 200 但实际失败」本地重试代理
 *
 * 解决两类问题（都源于同一个根因）：
 *   A. 中转站把上游错误（限流/过载/路由失败）包在 HTTP 200 的响应体或 SSE 流里返回。
 *      ZCode 只按 HTTP 状态码判断是否重试，于是这类错误落进兜底分支：
 *      reason=unknown / retryable=false —— 一次都不重试，直接报错。
 *   B. 中转站返回「空回应」：HTTP 200、流正常结束，但整段没有任何内容。
 *
 * 本代理：
 *   1. 在本地先自己对这两类失败做指数退避重试；
 *   2. 本地重试仍失败时，改写成一个真正的 429（带 retry-after），
 *      交给 ZCode 自身的重试机制继续兜底。
 *   3. 缓冲开场、已知空占位及 Codex 未完成搜索；真实内容、搜索完成或客户端工具开始即下发；
 *      下发前错误可处理，下发后错误只能透传，不能保证整条流成功。
 *
 * 用法：
 *   node relay-retry-proxy.mjs
 * 然后把 ZCode 中该中转站的 baseURL 改为 http://127.0.0.1:8787/v1
 */

import http from 'node:http';
import { createHash, randomUUID } from 'node:crypto';
import { appendFileSync, mkdirSync, readFileSync, renameSync, rmSync, statSync, truncateSync } from 'node:fs';
import { dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createFileLogger } from './bounded-file-log.mjs';

const num = (v, d) => { const n = Number(v); return Number.isFinite(n) && n > 0 ? n : d; };

const cfg = {
  statusIntervalMs: Math.max(100, num(process.env.PROXY_STATUS_INTERVAL_MS, 20000)),
  host: process.env.PROXY_HOST || '127.0.0.1',
  port: num(process.env.PROXY_PORT, 8787),
  upstream: (process.env.PROXY_UPSTREAM || 'https://agentrouter.org').replace(/\/+$/, ''),
  maxAttempts: num(process.env.PROXY_MAX_ATTEMPTS, 5),
  // 「找不到部署」「连接超时」这类通道抖动错误重试更狠：它们本质是碰运气换一条上游通道，
  // 多试几次命中好通道的概率明显更高。耗尽后还会转 429 让 ZCode 再兜一层。
  deployMaxAttempts: num(process.env.PROXY_DEPLOY_MAX_ATTEMPTS, 8),
  deployDelayMs: num(process.env.PROXY_DEPLOY_DELAY_MS, 800),  // 通道抖动用更短间隔，快速换通道
  // 响应头等待与重试准入预算；已经下发的流使用独立进展超时。
  // 不按总耗时截断仍在生成正文、推理或工具参数的长请求。
  totalBudgetMs: num(process.env.PROXY_TOTAL_BUDGET_MS, 500000),
  baseDelayMs: num(process.env.PROXY_BASE_DELAY_MS, 2000),
  maxDelayMs: num(process.env.PROXY_MAX_DELAY_MS, 30000),
  peekIdleMs: num(process.env.PROXY_PEEK_IDLE_MS, 45000),   // 单次读取最长等待
  peekTotalMs: num(process.env.PROXY_PEEK_TOTAL_MS, 120000), // 确认首个内容的最长总时长
  // 已经下发给客户端之后，两条 SSE 事件之间允许的最长静默。
  // 上游连接虽然还开着但不再发数据时，若不设这个上限，就会一直挂到 undici 的
  // 默认 bodyTimeout（300 秒）才报错 —— 这就是「报错一下要等非常久」的主因。
  streamIdleMs: num(process.env.PROXY_STREAM_IDLE_MS, 90000),
  // Heartbeats keep the transport alive but do not demonstrate generation progress.
  streamProgressMs: num(process.env.PROXY_STREAM_PROGRESS_MS, 180000),
  // Responses completion events may contain the entire generated output.
  sseBufferMaxBytes: num(process.env.PROXY_SSE_BUFFER_MAX_BYTES, 64 * 1024 * 1024),
  // 等待上游响应头的基础时长。拥堵时上游可能 150–230 秒才回首个字节，
  // 默认给 220 秒，避免把本来能到的请求提前掐断；可在桌面策略里单独调整。
  connectTimeoutMs: num(process.env.PROXY_CONNECT_TIMEOUT_MS, 220000),
  // 请求体越大（上下文越长），预填充越慢；每多 1MB 再额外放宽这么多。
  connectTimeoutPerMbMs: num(process.env.PROXY_CONNECT_TIMEOUT_PER_MB_MS, 30000),
  // 重试前至少要有这么多剩余预算，否则这次重试没意义（只会白等一轮）。
  minRetryWindowMs: num(process.env.PROXY_MIN_RETRY_WINDOW_MS, 30000),
  sessionCompat: process.env.PROXY_SESSION_COMPAT !== '0',
  codexResourceRetry: process.env.PROXY_CODEX_RESOURCE_RETRY !== '0',
  // Codex 会把本地生成的 item id 一起回传（UUIDv7 / review_rollout_user / resp_..._msg 等），
  // 上游按前缀校验会直接 400。对 Codex 的 /v1/responses 在首次发送前剥离可选 id 句柄；
  // web_search_call 的编号是必需引用，item_reference 的 id 就是载荷，二者都保留。
  codexStripIds: process.env.PROXY_CODEX_STRIP_IDS !== '0',
  sessionCompatTtlMs: num(process.env.PROXY_SESSION_COMPAT_TTL_MS, 86400000),
  sessionCompatMaxKeys: Math.floor(num(process.env.PROXY_SESSION_COMPAT_MAX_KEYS, 4096)),
  diagnosticsFile: process.env.PROXY_DIAGNOSTICS_FILE || fileURLToPath(new URL('./relay-diagnostics.jsonl', import.meta.url)),
  // 每份诊断档的容量。实测每条约 260 字节、高峰约 1800 条/小时，8 MiB 约覆盖一周。
  diagnosticsMaxBytes: Math.max(4096, Math.floor(num(process.env.PROXY_DIAGNOSTICS_MAX_BYTES, 8 * 1024 * 1024))),
  verbose: process.env.PROXY_VERBOSE !== '0',
  extraRetryCodes: (process.env.PROXY_EXTRA_RETRY_CODES || '')
    .split(',').map((s) => s.trim().toLowerCase()).filter(Boolean),
};

// Strategy settings are an immutable launch snapshot. Without the optional envelope,
// standalone users keep their existing environment defaults and compatibility gates.
function defaultPolicyProfile(codex) {
  return {
    maxAttempts: cfg.maxAttempts, baseDelayMs: cfg.baseDelayMs,
    maxDelayMs: cfg.maxDelayMs, totalBudgetMs: cfg.totalBudgetMs,
    connectTimeoutMs: cfg.connectTimeoutMs,
    historyMode: 'on_error', historyConfirmations: 1,
    stripItemIds: codex && cfg.codexStripIds, rememberSession: cfg.sessionCompat,
    repairStatus: true, retryHistory: codex && cfg.codexResourceRetry,
    emptyResponseRepair: !codex, emptyResponseDropReasoning: !codex,
    emptyResponseNormalizeAfter: 2, emptyResponseReasoningAfter: 6,
  };
}

function compilePolicy(raw) {
  const reject = (field) => { throw new Error(`Invalid PROXY_POLICY_JSON: ${field}`); };
  const object = (value, keys, field) => {
    if (!value || typeof value !== 'object' || Array.isArray(value)
        || Object.keys(value).some(key => !keys.includes(key))) reject(field);
  };
  let value;
  try { value = JSON.parse(raw); } catch { reject('invalid JSON'); }
  object(value, ['schemaVersion', 'profiles'], 'settings');
  if (value.schemaVersion !== undefined && value.schemaVersion !== 1) reject('schemaVersion');
  const profiles = value.profiles === undefined ? {} : value.profiles;
  object(profiles, ['codex', 'general'], 'profiles');
  const result = { schemaVersion: 1, profiles: {} };
  for (const name of ['codex', 'general']) {
    const defaults = defaultPolicyProfile(name === 'codex');
    const overrides = profiles[name] === undefined ? {} : profiles[name];
    object(overrides, Object.keys(defaults), `profiles.${name}`);
    const profile = { ...defaults, ...overrides };
    const integer = (key, min, max) => {
      if (!Number.isSafeInteger(profile[key]) || profile[key] < min || profile[key] > max) reject(`${name}.${key}`);
    };
    integer('maxAttempts', 1, 1000);
    integer('baseDelayMs', 1, 300000);
    integer('maxDelayMs', 1, 300000);
    integer('totalBudgetMs', 1000, 3600000);
    integer('connectTimeoutMs', 1000, 600000);
    if (profile.connectTimeoutMs > profile.totalBudgetMs) reject(`${name}.connectTimeoutMs`);
    integer('historyConfirmations', 1, name === 'codex' ? 4 : 1000);
    integer('emptyResponseNormalizeAfter', 1, 1000);
    integer('emptyResponseReasoningAfter', 1, 1000);
    if (!['off', 'on_error', 'always'].includes(profile.historyMode)) reject(`${name}.historyMode`);
    for (const key of ['stripItemIds', 'rememberSession', 'repairStatus', 'retryHistory',
      'emptyResponseRepair', 'emptyResponseDropReasoning']) {
      if (typeof profile[key] !== 'boolean') reject(`${name}.${key}`);
    }
    if (profile.baseDelayMs > profile.maxDelayMs) reject(`${name}.delayRange`);
    if (profile.historyConfirmations > profile.maxAttempts) reject(`${name}.historyConfirmations`);
    if (profile.emptyResponseReasoningAfter <= profile.emptyResponseNormalizeAfter
        || (profile.emptyResponseDropReasoning && !profile.emptyResponseRepair)) reject(`${name}.emptyResponseStages`);
    if (name === 'codex' && (profile.emptyResponseRepair || profile.emptyResponseDropReasoning)
        || name === 'general' && (profile.stripItemIds || profile.retryHistory)) reject(`${name}.unsupportedModule`);
    result.profiles[name] = Object.freeze(profile);
  }
  Object.freeze(result.profiles);
  return Object.freeze(result);
}

const hasPolicySettings = process.env.PROXY_POLICY_JSON !== undefined;
const policySettings = hasPolicySettings ? compilePolicy(process.env.PROXY_POLICY_JSON) : {
  schemaVersion: 1, profiles: { codex: defaultPolicyProfile(true), general: defaultPolicyProfile(false) },
};
const policyRevision = createHash('sha256').update(JSON.stringify(policySettings)).digest('hex');

const startedAt = new Date().toISOString();
const loadedVersion = createHash('sha256').update(readFileSync(fileURLToPath(import.meta.url))).digest('hex');
const activeRequests = new Map();
const supervisorEnabled = process.env.PROXY_SUPERVISED === '1' && typeof process.send === 'function';
let restarting = false;
let draining = false;
let drainExitCode = 0;
let closing = false;
let restartError = null;
// 一次有界、安全停机尝试。超时或调用者断连时撤销，不保留延迟退出任务。
let safeShutdown = null;
// 可取消的两阶段等待：排空后仍存活，由同一操作显式确认退出。
let drainWait = null;
// 只保留最近 128 个操作的封闭终态：旧 start 不续预算，取消确认可安全重试。
const finishedDrainOperations = new Map();

function clearDrainWait(attempt = drainWait, outcome = 'cancelled') {
  if (!attempt || drainWait !== attempt) return;
  clearTimeout(attempt.budgetTimer);
  clearTimeout(attempt.leaseTimer);
  finishedDrainOperations.set(attempt.operation, outcome);
  if (finishedDrainOperations.size > 128) finishedDrainOperations.delete(finishedDrainOperations.keys().next().value);
  drainWait = null;
  draining = false;
  // 到期/实际停机撤销尚未送完的 commit，不能留下成功确认后又继续接单。
  if (attempt.commitRes && !attempt.commitRes.writableFinished) attempt.commitRes.destroy();
}

function expireDrainWait(attempt = drainWait) {
  if (!attempt || drainWait !== attempt) return;
  const now = performance.now();
  if (now >= attempt.deadline || now >= attempt.leaseDeadline) clearDrainWait(attempt);
}

function renewDrainLease(attempt) {
  clearTimeout(attempt.leaseTimer);
  attempt.leaseDeadline = performance.now() + attempt.leaseMs;
  attempt.leaseTimer = setTimeout(() => clearDrainWait(attempt), attempt.leaseMs);
}

function requestSnapshot(state) {
  const now = Date.now();
  return {
    requestId: state.requestId, path: state.path, phase: state.phase, attempt: state.attempt,
    elapsedMs: now - state.started, bytesReceived: state.bytesReceived,
    eventCount: state.eventCount, reasoningEvents: state.reasoningEvents,
    textEvents: state.textEvents, toolEvents: state.toolEvents,
    sinceLastByteMs: state.lastByte == null ? null : now - state.lastByte,
    sinceLastProgressMs: state.lastProgress == null ? null : now - state.lastProgress,
  };
}

function progressKind(obj) {
  const type = String(obj.type || '');
  const block = obj.content_block || obj.delta;
  if (obj.item?.type === 'reasoning' || type.includes('reasoning') ||
      block?.type === 'thinking_delta' || block?.type === 'thinking') return 'reasoning';
  if (obj.item?.type === 'function_call' || type.includes('function_call') ||
      block?.type === 'input_json_delta' || block?.type === 'tool_use') return 'tool';
  for (const choice of Array.isArray(obj.choices) ? obj.choices : []) {
    const delta = choice?.delta;
    if (nonEmptyStr(delta?.reasoning_content) || nonEmptyStr(delta?.reasoning)) return 'reasoning';
    if (Array.isArray(delta?.tool_calls) && delta.tool_calls.length) return 'tool';
  }
  return 'text';
}

// Classification sees the cause, not opaque identifiers or diagnostic URLs. Keep
// item prefixes and parameter paths so precise history/status errors stay recognizable.
function errorCauseText(message) {
  return String(message || '')
    .replace(/\[\s*(?:request|trace|response|item)[_ -]?id\s*[:=][^\]\r\n]*\]|\(\s*(?:request|trace|response|item)[_ -]?id\s*[:=][^)\r\n]*\)/gi, ' ')
    .replace(/\bhttps?:\/\/[^\s<>"'\])]+/gi, ' ')
    .replace(/\b(?:request|trace|response|item)[_ -]?id\s*[:=]\s*(?:"[^"\r\n]*"|'[^'\r\n]*'|[^\s)\],;]+)/gi, ' ')
    .replace(/\b(rs|ws|msg|resp|fc|call)_[a-z0-9_.:-]+/gi, '$1_opaque');
}

// 明确的暂时性错误码 → 值得重试
// 注意：不含 insufficient_quota —— 配额耗尽属于确定性错误，重试无意义。
const RETRYABLE_CODES = new Set([
  'rate_limit_exceeded', 'too_many_requests', 'server_error',
  'internal_server_error', 'overloaded', 'overloaded_error', 'service_unavailable',
  'temporarily_unavailable', 'timeout', 'upstream_error', 'capacity',
  ...cfg.extraRetryCodes,
]);

// 没有 code 时按文案兜底判断是否暂时性
// 注意 "no deployment / deployment to match"：模型名本身是有效的（在服务商 /v1/models
// 里能查到），报这个错说明上游部分通道缺少该部署，属于偶发路由问题 —— 重试有机会命中好通道。
const RETRYABLE_TEXT =
  /\b(?:rate[ _-]?limit(?:ed| exceeded)?|too many requests|overload(?:ed)?|temporar(?:y|ily)|try again|timeout|timed out|unavailable|capacity|internal (?:server )?error|bad gateway|gateway (?:error|timeout)|no deployment|deployment to match)\b|\b(?:HTTP(?: status(?: code)?)?|status(?: code)?|error(?: code)?)\s*[:=]?\s*(?:502|503|504)\b/i;

// Exact provider codes are evidence; code substrings and opaque identifiers are not.
const PERMANENT_CODES = new Set([
  'invalid_api_key', 'incorrect_api_key', 'unauthorized', 'authentication_error',
  'permission_denied', 'insufficient_permissions', 'account_disabled', 'account_deactivated',
  'context_length_exceeded', 'model_not_found', 'invalid_model', 'unsupported_model',
  'content_blocked', 'content-blocked', 'content_filter', 'content_filter_error',
  'content-filter', 'contentfilter', 'flagged', 'moderation',
  'sensitive_words', 'request_too_large',
]);
const QUOTA_CODES = new Set(['insufficient_quota', 'quota_exceeded', 'quota_exhausted', 'insufficient_balance', 'payment_required']);
const PERMANENT_TEXT =
  /\b(?:invalid api key|incorrect api key|invalid_api_key|unauthorized|authentication failed|authentication_error|insufficient permissions|permission denied|account (?:is )?(?:deactivated|disabled|suspended))\b/i;

function isPermanentError(info) {
  return !!info && (PERMANENT_CODES.has(String(info.code || info.type || '').toLowerCase())
    || PERMANENT_TEXT.test(errorCauseText(info.message)));
}

// 「找不到部署 / 上游没有该模型部署」——通道抖动，重试换通道即可，值得多试几次。
// 中文站点常把同一件事说成「无可用渠道 / 无可用模型」：模型名在该分组下没有通道，
// 属于通道问题而非限流。以前只认英文，中文文案会落进通用 503 重试，白白跑满
// 110 次、约 200 秒才放弃（实测 Codex 后台辅助请求整晚如此），现按通道抖动快速收手。
const DEPLOYMENT_TEXT = /\b(?:no deployment|deployment to match)\b|无可用渠道|无可用模型|无可用线路|模型不存在/i;
function isDeploymentError(info) {
  return !!info && DEPLOYMENT_TEXT.test(errorCauseText(info.message));
}

/**
 * Responses 的推理项可能绑定上游资源；协议由客户端/网关配置决定，
 * 不能根据 ZCode / Claude / Codex 的名称推断协议。
 * 删除不可用 reasoning 会损失推理连续性，并非无损修复。
 */
const REASONING_ITEM_TEXT =
  /\bencrypted content (?:for item ['"]?rs_[^'"\s]+['"]? )?could not be (?:verified|decrypted|parsed)\b|\bencrypted content (?:could not|failed)\b|\bitems are not persisted when .?store.? is set to false\b|\bitem with id ['"]rs_[^'"\r\n]+['"] not found\b/i;

function isReasoningItemError(text) {
  return REASONING_ITEM_TEXT.test(String(text || ''));
}

// Only admitted by the strict Codex branch; other clients keep their existing policy.
const MISSING_SEARCH_REASONING_TEXT =
  /\bitem ['"]ws_[^'"\r\n]+['"] of type ['"]web_search_call['"] was provided without its required ['"]reasoning['"] item:\s*['"]rs_[^'"\r\n]+['"]/i;

// 上游会在消息末尾附加 "[trace_id=...]"，所以必须用子串/正则匹配，
// 不能用全等比较 —— 否则永远匹配不上，修复逻辑形同虚设。
const AZURE_RESOURCE_TEXT =
  /\bthe requested item was created under a different (?:azure|\*\*\*) openai resource\b/i;
function isAzureResourceError(message) {
  return AZURE_RESOURCE_TEXT.test(String(message || ''));
}

/** Allowlisted shape only: never copy arbitrary type/role/store values from input. */
function summarizeResponsesBody(bodyBuf) {
  try {
    const obj = JSON.parse(bodyBuf.toString('utf8'));
    if (!obj || typeof obj !== 'object' || Array.isArray(obj)) return { inputType: 'non_object' };
    const types = {};
    const allowed = new Set(['message', 'reasoning', 'function_call', 'function_call_output', 'item_reference']);
    let withId = 0;
    if (Array.isArray(obj.input)) {
      for (const it of obj.input) {
        const object = it && typeof it === 'object' && !Array.isArray(it);
        const t = !object ? 'non_object' : allowed.has(it.type) ? it.type
          : it.type === undefined && typeof it.role === 'string' ? 'message_no_type' : 'other';
        types[t] = (types[t] || 0) + 1;
        if (object && Object.hasOwn(it, 'id')) withId++;
      }
    }
    return {
      inputType: Array.isArray(obj.input) ? 'array' : typeof obj.input === 'string' ? 'string' : 'other',
      inputLength: Array.isArray(obj.input) ? obj.input.length : null,
      itemsWithId: withId,
      types,
      previousResponseId: obj.previous_response_id != null,
      conversation: obj.conversation != null,
    };
  } catch { return { inputType: 'invalid_json' }; }
}

// Independent of verbose/raw logs. One writer per file; current + one backup,
// each bounded. Synchronous short writes keep rotation ordered across requests.
let diagnosticsWarning = false;
// 归档本轮换连续失败几次后改为原地截断：宁可丢旧记录，也不能让诊断从此停止写入。
// 2026-10-21 实测：Windows 上 rename 覆盖被占用的 .1 会 EPERM，原实现只 rename、
// 失败又被 catch 静默吞掉，日志在 1 MiB 后整整停了 6 小时。
const DIAGNOSTICS_TRUNCATE_AFTER = 3;
let diagnosticsRotationFailures = 0;
function rotateDiagnostics(file) {
  try {
    rmSync(file + '.1', { force: true });   // Windows 不允许 rename 覆盖已存在的目标
    renameSync(file, file + '.1');
    diagnosticsRotationFailures = 0;
    return;
  } catch { /* 落到下面的截断兜底 */ }
  diagnosticsRotationFailures++;
  if (diagnosticsRotationFailures < DIAGNOSTICS_TRUNCATE_AFTER) throw new Error('diagnostics rotate failed');
  truncateSync(file, 0);
  diagnosticsRotationFailures = 0;
}
function safeDiagnostic(record) {
  try {
    const line = JSON.stringify({ time: new Date().toISOString(), ...record }) + '\n';
    const bytes = Buffer.byteLength(line);
    if (bytes > cfg.diagnosticsMaxBytes) return;
    mkdirSync(dirname(cfg.diagnosticsFile), { recursive: true });
    let size = 0;
    try { size = statSync(cfg.diagnosticsFile).size; }
    catch (e) { if (e.code !== 'ENOENT') throw e; }
    if (size + bytes > cfg.diagnosticsMaxBytes) rotateDiagnostics(cfg.diagnosticsFile);
    appendFileSync(cfg.diagnosticsFile, line, { mode: 0o600 });
  } catch {
    // Do not print filesystem paths, exceptions, or request data on failure.
    if (!diagnosticsWarning) console.error('safe diagnostics unavailable; requests will continue');
    diagnosticsWarning = true;
  }
}

/**
 * 启动时自愈：容量上限调大（如 1 MiB → 8 MiB）后还要能继续写。
 * live 已经写满时主动补做一次轮转，把旧 live 归到 .1；若这一步也做不成（文件被占用等），
 * 就截断 live 继续写 —— 宁可丢旧记录，也不能让诊断从此停摆。
 */
function healDiagnosticsRotation() {
  try {
    const file = cfg.diagnosticsFile;
    let size = 0;
    try { size = statSync(file).size; }
    catch (e) { if (e.code === 'ENOENT') return; throw e; }
    if (size < cfg.diagnosticsMaxBytes) return;
    try {
      rmSync(file + '.1', { force: true });
      renameSync(file, file + '.1');
    } catch {
      truncateSync(file, 0);
    }
  } catch { /* 自愈失败不影响转发，写入路径还有自己的兜底 */ }
}

/**
 * Conservative stateless replay. Only inline message/function/custom-tool items have optional
 * item ids removed; never recurse into content, arguments, output, or call_id.
 * Confirmed Codex history errors may opt into stateless web-search replay: remove
 * reasoning and search handles together, preserving the inline search payload.
 * Dropping reasoning is lossy; handle-only recovery retains its payloads.
 * Neither fallback is portable Responses validation.
 * Server-side history cannot be reconstructed or proven complete by this proxy.
 *
 * options:
 *   stripItemIds      – drop inline item handles; includes search handles in the Codex fallback
 *   stripReasoningIds – drop the id handle on reasoning items but keep the items
 *   stripMessageType  – drop the redundant `type: "message"` tag (role/content stay)
 *   dropReasoning     – remove reasoning items entirely (lossy)
 *   allowUnchanged    – return the original body instead of erroring when nothing changed
 *   codexHistory      – validate complete inline tool/search history for Codex (default off)
 *
 * Ordinary optional-id/type removals preserve inline content. The Codex fallback
 * additionally sacrifices old reasoning continuity and resource-bound search linkage.
 */
function repairResponsesItems(bodyBuf, options = {}) {
  const {
    stripItemIds = false, stripReasoningIds = false, stripMessageType = false,
    dropReasoning = false, allowUnchanged = false, codexHistory = false, requireCompleteTools = false,
  } = options;
  // Removing both handle sets may retain the complete reasoning payload; only
  // the explicit binding-error recovery opts into that combination.
  const statelessSearch = codexHistory && stripItemIds && (dropReasoning || stripReasoningIds);
  const handlesOnly = codexHistory && stripReasoningIds && !dropReasoning;
  const completeTools = codexHistory || requireCompleteTools;
  let obj;
  try { obj = JSON.parse(bodyBuf.toString('utf8')); }
  catch { return { error: 'request is not valid JSON' }; }
  if (!obj || typeof obj !== 'object' || Array.isArray(obj)) return { error: 'request is not a JSON object' };
  if (obj.previous_response_id != null && obj.previous_response_id !== '') {
    return { error: 'previous_response_id references server-side history; complete inline history cannot be verified' };
  }
  if (obj.conversation != null) return { error: 'conversation references server-side history' };
  if (!Array.isArray(obj.input)) return { error: 'input is not a complete inline item array' };
  // Never remove reasoning while retaining resource-bound search handles.
  if (codexHistory && dropReasoning && !statelessSearch && obj.input.some(item => item?.type === 'web_search_call')) {
    return { error: 'web_search_call may depend on reasoning; refusing to discard its history' };
  }

  let removedReasoning = 0;
  let removedIds = 0;
  let removedTypes = 0;
  const input = [];
  const calls = new Map();
  const completedCalls = new Set();
  for (const item of obj.input) {
    if (!item || typeof item !== 'object' || Array.isArray(item)) return { error: 'unsupported inline input item' };
    if (item.type === 'item_reference') return { error: 'item_reference requires server-side history; refusing to discard it' };
    if (item.type === 'reasoning') {
      if (dropReasoning) { removedReasoning++; continue; }
      if (handlesOnly && Object.hasOwn(item, 'id') && !nonEmptyStr(item.encrypted_content)) {
        return { error: 'reasoning handle lacks inline encrypted content; refusing to discard its reference' };
      }
      if (stripReasoningIds && Object.hasOwn(item, 'id')) {
        delete item.id;
        removedIds++;
      }
      input.push(item);
      continue;
    }
    if (item.type === 'message' || (item.type === undefined && typeof item.role === 'string')) {
      if (!['user', 'assistant', 'system', 'developer'].includes(item.role)
          || !(typeof item.content === 'string' || Array.isArray(item.content))) {
        return { error: 'message does not contain inline role/content' };
      }
      if (handlesOnly && Object.hasOwn(item, 'id')
          && (typeof item.content === 'string' ? !item.content.trim() : !item.content.length)) {
        return { error: 'empty message may reference server-side content; refusing to discard its handle' };
      }
      if (stripMessageType && Object.hasOwn(item, 'type')) {
        delete item.type;
        removedTypes++;
      }
    } else if (item.type === 'function_call') {
      if (!nonEmptyStr(item.call_id) || !nonEmptyStr(item.name) || typeof item.arguments !== 'string'
          || calls.has(item.call_id)) return { error: 'function_call is incomplete or has a duplicate call_id' };
      calls.set(item.call_id, 'function_call');
    } else if (item.type === 'function_call_output') {
      if (!nonEmptyStr(item.call_id) || calls.get(item.call_id) !== 'function_call'
          || (completeTools && completedCalls.has(item.call_id))
          || !(typeof item.output === 'string' || Array.isArray(item.output))) {
        return { error: 'function_call_output lacks a preceding inline function_call or complete output' };
      }
      if (completeTools) completedCalls.add(item.call_id);
    } else if (item.type === 'custom_tool_call') {
      if (!nonEmptyStr(item.call_id) || !nonEmptyStr(item.name) || typeof item.input !== 'string'
          || calls.has(item.call_id)) return { error: 'custom_tool_call is incomplete or has a duplicate call_id' };
      calls.set(item.call_id, 'custom_tool_call');
    } else if (item.type === 'custom_tool_call_output') {
      if (!nonEmptyStr(item.call_id) || calls.get(item.call_id) !== 'custom_tool_call'
          || (completeTools && completedCalls.has(item.call_id))
          || !(typeof item.output === 'string' || Array.isArray(item.output))) {
        return { error: 'custom_tool_call_output lacks a preceding inline custom_tool_call or complete output' };
      }
      if (completeTools) completedCalls.add(item.call_id);
    } else if (codexHistory && item.type === 'web_search_call') {
      const action = item.action;
      if ((!statelessSearch && !nonEmptyStr(item.id)) || !['in_progress', 'searching', 'completed', 'failed'].includes(item.status)
          || !action || typeof action !== 'object' || Array.isArray(action)
          || !(action.type === 'search' && (nonEmptyStr(action.query)
                || Array.isArray(action.queries) && action.queries.length > 0 && action.queries.every(nonEmptyStr))
            || action.type === 'open_page' && nonEmptyStr(action.url)
            || action.type === 'find_in_page' && nonEmptyStr(action.url) && nonEmptyStr(action.pattern))) {
        return { error: 'web_search_call lacks a supported complete inline action' };
      }
      // The tested Codex route accepts an id-less search in the lossy fallback.
      // Preserve all action extensions; never manufacture call_id or convert to text.
    } else {
      return { error: 'unsupported or reference-only input item; safe replay cannot be verified' };
    }
    if (Object.hasOwn(item, 'encrypted_content')) {
      return { error: 'encrypted_content on a non-reasoning item cannot be safely removed' };
    }
    if (stripItemIds && (!(codexHistory && item.type === 'web_search_call') || statelessSearch) && Object.hasOwn(item, 'id')) {
      delete item.id;
      removedIds++;
    }
    input.push(item);
  }
  if (completeTools && completedCalls.size !== calls.size) {
    return { error: 'inline tool history is incomplete; a call lacks its output' };
  }
  if (!input.length) return { error: 'no inline message or function history would remain' };
  if (!removedReasoning && !removedIds && !removedTypes) {
    if (allowUnchanged) return { body: bodyBuf, removedReasoning: 0, removedIds: 0, removedTypes: 0 };
    return { error: 'no safely removable resource-bound items or item ids' };
  }
  obj.input = input;
  return { body: Buffer.from(JSON.stringify(obj), 'utf8'), removedReasoning, removedIds, removedTypes };
}

// Optional resource handles only. web_search_call ids are required references and
// item_reference carries its id as payload, so neither may be touched.
const CODEX_STRIPPABLE_ID_TYPES = new Set([
  'message', 'reasoning', 'function_call', 'function_call_output',
  'custom_tool_call', 'custom_tool_call_output',
]);

/**
 * Proactive Codex sanitizer: drop optional item-id handles before the first upstream
 * attempt. Content, call_id, tool output and reasoning encrypted_content are never
 * modified; the reasoning items themselves are kept.
 */
function stripCodexItemIds(bodyBuf) {
  let obj;
  try { obj = JSON.parse(bodyBuf.toString('utf8')); }
  catch { return null; }
  if (!obj || typeof obj !== 'object' || Array.isArray(obj)) return null;
  if (!Array.isArray(obj.input)) return null;
  // Search ids can reference reasoning ids implicitly; the dependency is not exposed
  // in the input, so preserve every reasoning handle when search history is present.
  const hasWebSearch = obj.input.some(item => item?.type === 'web_search_call');
  let removedIds = 0;
  for (const item of obj.input) {
    if (!item || typeof item !== 'object' || Array.isArray(item)) continue;
    const type = item.type === undefined && typeof item.role === 'string' ? 'message' : item.type;
    if (!CODEX_STRIPPABLE_ID_TYPES.has(type) || (hasWebSearch && type === 'reasoning')) continue;
    if (Object.hasOwn(item, 'id')) {
      delete item.id;
      removedIds++;
    }
  }
  if (!removedIds) return null;
  return { body: Buffer.from(JSON.stringify(obj), 'utf8'), removedIds };
}

// Store only scoped hashes, never credentials, session names, or reasoning payloads.
const compatibilityKeys = new Map();
// Codex fallback entries must neither affect nor evict the other clients' entries.
const codexCompatibilityKeys = new Map();
const fingerprint = (value) => createHash('sha256').update(value).digest('hex');
// Cache affinity is not conversation identity. Keep only scoped hashes and generated keys.
const cacheRouteKeys = new Map();
// Anthropic 与 Codex 两条通道都观察到同一份 prompt_cache_key 被持续拒绝；
// 冲突或未知客户端、无缓存键的请求保持原样。
function cacheRouteIdentity(bodyBuf, headers) {
  const explicit = String(headers['x-relay-client'] || '').trim().toLowerCase();
  const ua = String(headers['user-agent'] || '').toLowerCase();
  const claude = explicit === 'claude' || (!explicit && ua.includes('claude'));
  if (!claude && !strictCodex(headers)) return null;
  let obj;
  try { obj = JSON.parse(bodyBuf.toString('utf8')); } catch { return null; }
  if (!obj || typeof obj !== 'object' || Array.isArray(obj) || !nonEmptyStr(obj.prompt_cache_key)) return null;
  return fingerprint(JSON.stringify([cfg.upstream, headers.authorization || '',
    headers['x-api-key'] || '', headers['api-key'] || '', obj.model || '', obj.prompt_cache_key]));
}
function withCacheRouteKey(bodyBuf, key) {
  const obj = JSON.parse(bodyBuf.toString('utf8'));
  obj.prompt_cache_key = key;
  return Buffer.from(JSON.stringify(obj), 'utf8');
}

function compatibilityIdentity(bodyBuf, headers, enabled = cfg.sessionCompat) {
  if (!enabled) return null;
  let obj;
  try { obj = JSON.parse(bodyBuf.toString('utf8')); } catch { return null; }
  if (!obj || typeof obj !== 'object' || Array.isArray(obj)) return null;
  const session = headers['x-relay-session-id'];
  const explicit = typeof session === 'string' && session.length > 0 && session.length <= 512 ? session : '';
  const scope = fingerprint(JSON.stringify([
    cfg.upstream, headers.authorization || '', headers['x-api-key'] || '',
    headers['api-key'] || '', obj.model || '', explicit,
  ]));
  if (explicit) return { keys: [fingerprint(scope + ':session')], source: 'explicit_session' };
  const keys = [];
  if (Array.isArray(obj.input)) {
    for (const item of obj.input) {
      if (!item || typeof item !== 'object' || Array.isArray(item)) continue;
      if (!['reasoning', 'message', 'function_call', 'function_call_output'].includes(item.type) &&
          !(item.type === undefined && typeof item.role === 'string')) continue;
      for (const field of item.type === 'reasoning' ? ['id', 'encrypted_content'] : ['id']) {
        if (nonEmptyStr(item[field])) keys.push(fingerprint(scope + ':' + field + ':' + item[field]));
      }
    }
  }
  return keys.length ? { keys: [...new Set(keys)], source: 'resource_lineage' } : null;
}

function pruneCompatibility(cache) {
  const now = Date.now();
  for (const [key, expiry] of cache) if (expiry <= now) cache.delete(key);
}

function knownCompatibility(identity, cache) {
  pruneCompatibility(cache);
  return !!identity && identity.keys.some((key) => cache.has(key));
}

function rememberCompatibility(identity, cache) {
  if (!identity) return false;
  pruneCompatibility(cache);
  for (const key of identity.keys) {
    cache.delete(key);
    cache.set(key, Date.now() + cfg.sessionCompatTtlMs);
    while (cache.size > cfg.sessionCompatMaxKeys) {
      cache.delete(cache.keys().next().value);
    }
  }
  return true;
}

/**
 * 配额/余额类错误（HTTP 402 或特定文案）：
 * 例如 "Budget pool quota has been exhausted" —— 账户或共享池没额度了。
 * 重试在单个请求内毫无意义（池子不会几秒内回血），必须快速失败并让用户看到真实原因，
 * 好去换模型/换号/等池子恢复。若当成可重试错误处理，只会白白拖慢并烧掉请求数。
 */
const QUOTA_TEXT =
  /\b(?:budget pool|quota has been exhausted|quota exhausted|insufficient_quota|insufficient quota|out of quota|exceeded your current quota|no available channel|payment required|insufficient balance|balance (?:is )?(?:insufficient|exhausted|too low))\b|余额不足|额度不足|配额/i;

function isQuotaError(info) {
  return !!info && (QUOTA_CODES.has(String(info.code || info.type || '').toLowerCase())
    || QUOTA_TEXT.test(errorCauseText(info.message)));
}

const DETERMINISTIC_BODY_TEXT =
  /\b(?:sensitive words|sensitive_words|content[-_ ]?(?:blocked|filter)|flagged|moderation|model not found|(?:model|resource|item)\s+(?:['"][^'"\r\n]+['"]\s+)?does not exist|invalid model|unsupported model|not supported|too large|context length|context_length_exceeded|maximum context|exceed\w*[^.\r\n]*\bmax(?:imum)?[^.\r\n]*\btokens?)\b|内容审核|违规/i;

function isDeterministicError(info) {
  return !!info && (isPermanentError(info) || isQuotaError(info)
    || DETERMINISTIC_BODY_TEXT.test(errorCauseText(info.message)));
}

// 日志：默认只打到控制台；显式启用的普通文件日志也受总容量约束。
const fileLog = createFileLogger(process.env);

const log = (...a) => {
  const line = `${new Date().toISOString()} ${a.map((x) => (typeof x === 'string' ? x : JSON.stringify(x))).join(' ')}`;
  if (cfg.verbose) console.log(line);
  fileLog(line);
};
const snippet = (b) => String(b).replace(/\s+/g, ' ').slice(0, 160);

/**
 * 按请求体大小决定「等待上游响应头」允许多久。
 * 小请求 30s（快速失败、换通道重试很划算）；
 * 大请求即长上下文，预填充本来就慢，按每 MB 放宽，上限为总预算。
 */
function connectTimeoutFor(bodyBytes, totalBudgetMs = cfg.totalBudgetMs, connectTimeoutMs = cfg.connectTimeoutMs) {
  const mb = Math.max(0, bodyBytes) / (1024 * 1024);
  const scaled = connectTimeoutMs + mb * cfg.connectTimeoutPerMbMs;
  return Math.max(1000, Math.min(totalBudgetMs, scaled));
}

/**
 * undici 抛的 `fetch failed` 本身没有信息量，真正的原因藏在 error.cause 里
 * （ECONNRESET / ETIMEDOUT / ENOTFOUND / UND_ERR_SOCKET ...）。
 * 逐层展开，方便判断是中转站抖动、DNS 问题还是本机网络问题。
 */
function describeFetchError(e) {
  const parts = [];
  let cur = e;
  for (let depth = 0; cur && depth < 5; depth++) {
    const bits = [cur.name, cur.code, cur.message].filter((x) => typeof x === 'string' && x && x !== parts.at(-1));
    if (bits.length) parts.push(bits.join('/'));
    cur = cur.cause;
  }
  return parts.join(' <- ') || 'unknown fetch error';
}

// 只在需要重试时创建计时器；取消立即结束等待，不等下一轮才收尾。
const sleep = (ms, signal) => new Promise((resolve) => {
  if (signal.aborted) return resolve();
  const finish = () => {
    clearTimeout(timer);
    signal.removeEventListener('abort', finish);
    resolve();
  };
  const timer = setTimeout(finish, ms);
  signal.addEventListener('abort', finish, { once: true });
});
const backoffMs = (attempt, policy = cfg) =>
  Math.min(policy.maxDelayMs, Math.round(policy.baseDelayMs * 2 ** (attempt - 1) * (1 + Math.random() * 0.3)));

function joinUrl(origin, path) {
  const o = origin.replace(/\/+$/, '');
  const p = path.startsWith('/') ? path : `/${path}`;
  if (o.endsWith('/v1') && p.startsWith('/v1/')) return o + p.slice(3);
  return o + p;
}

// ── 错误识别 ────────────────────────────────────────────────────────────────

/**
 * 是否为「终止事件」—— 流正常结束的标志。
 * 缺少它说明上游把流截断了（CC Switch 会报
 * "Responses upstream stream ended before a terminal event"）。
 */
const RESPONSES_TERMINAL = new Set(['response.completed', 'response.failed', 'response.incomplete']);

/**
 * 上游「静默丢弃」：HTTP 200，只回显 response.created（约 180KB，含 tools 回显）就关流，
 * 既不报错也不生成内容。这类失败没有可识别的错误文案，状态修复与推理项修复都不会触发，
 * 早先只能落进泛化重试 —— 用同一份请求体反复重试，指数退避光睡觉就要约 30 秒。
 */
const EMPTY_STREAM_DROP = 'empty response (stream ended with no content)';
function isTerminalEvent(obj) {
  if (!obj || typeof obj !== 'object') return false;
  const t = obj.type;
  if (typeof t === 'string') {
    if (RESPONSES_TERMINAL.has(t)) return true;
    if (t === 'message_stop') return true;            // Anthropic
  }
  // OpenAI Chat Completions：出现 finish_reason 即已终结
  if (Array.isArray(obj.choices) && obj.choices.some((c) => c && c.finish_reason)) return true;
  return false;
}

function errorInfoFromJson(obj) {
  if (!obj || typeof obj !== 'object' || Array.isArray(obj)) return null;
  const eventError = obj.type === 'error' || obj.type === 'response.failed';
  const e = obj.error || (eventError ? obj.response?.error : null);
  if (!eventError && (!e || typeof e !== 'object' || Array.isArray(e))) return null;
  const field = value => typeof value === 'string' && value.trim() ? value : undefined;
  return {
    code: field(e?.code) || (eventError ? field(obj.code) : undefined),
    type: field(e?.type),
    message: field(e?.message) || (eventError ? field(obj.message) : undefined) || 'provider error',
  };
}

function isRetryableError(info) {
  if (!info) return false;
  if (RETRYABLE_CODES.has(String(info.code || info.type || '').toLowerCase())) return true;
  return RETRYABLE_TEXT.test(errorCauseText(info.message));
}

// Every error carrier supplies the same evidence. Transport status is authoritative
// for auth/payment; generic envelope types and unrelated JSON are not error causes.
function classifyError(info, status = 200) {
  const message = errorCauseText(info?.message);
  const evidence = info ? { code: info.code, type: info.type, message } : null;
  const quota = status === 402 || isQuotaError(evidence);
  const permanent = quota || status === 401 || status === 403 || isDeterministicError(evidence);
  const deployment = !permanent && isDeploymentError(evidence);
  const retryable = !permanent && (deployment || status === 408 || status === 429 || status >= 500 || isRetryableError(info));
  let kind = permanent ? (quota ? 'quota' : 'permanent') : deployment ? 'deployment' : retryable ? 'retryable' : 'unknown';
  const sessionBindingInvalidated = kind === 'unknown' && [200, 400, 409].includes(status)
    && (String(info?.code || '').toLowerCase() === 'session_binding_invalidated'
      || /\bsession binding invalidated\b[\s\S]*\bnew session id\b/i.test(message));
  let historyKind = null;
  let missingSearchReasoning = false;
  let statusIndex = null;
  if (kind === 'unknown' && info && (status === 400 || status === 200)) {
    const statusMatch = /^Unknown parameter: ['"]input\[(\d+)\]\.status['"]\.(?:\s*\[trace_id=[^\]\r\n]+\])?\s*$/.exec(message);
    if (statusMatch) {
      kind = 'status';
      statusIndex = Number(statusMatch[1]);
    } else {
      const azure = isAzureResourceError(message);
      const reasoning = isReasoningItemError(message);
      missingSearchReasoning = !azure && !reasoning && MISSING_SEARCH_REASONING_TEXT.test(message);
      if (azure || reasoning || missingSearchReasoning) {
        kind = 'history';
        historyKind = azure ? 'azure_resource_mismatch' : 'stale_reasoning_item';
      }
    }
  }
  return { kind, info, message, permanent, quota, deployment, retryable, historyKind, missingSearchReasoning, statusIndex, sessionBindingInvalidated };
}

// 诊断用：把上游错误压成有界的指纹（错误码 + 命中的关键词），绝不落盘原文。
// 上游错误文案会随中转站改动，指纹让我们能判断「某个已知类别换了说法」还是「新类别」。
const ERROR_HINTS = ['different', 'resource', 'masked', 'persisted', 'encrypted', 'not found',
  'rate', 'overload', 'capacity', 'quota', 'channel', 'balance', 'timeout', 'unavailable', 'deployment'];
function errorFingerprint(info) {
  if (!info) return null;
  const message = errorCauseText(info.message).toLowerCase();
  const code = String(info.code || '');
  return {
    code: /^[A-Za-z0-9_.-]{1,64}$/.test(code) ? code : (code ? 'other' : 'none'),
    hints: ERROR_HINTS.filter((h) => message.includes(h)).join(','),
    masked: message.includes('***'),
  };
}

const nonEmptyStr = (v) => typeof v === 'string' && v.trim().length > 0;

// 诊断用：请求体结构画像。只记**已知字段名**与**有界枚举**，绝不记字段值。
// 起因：实测同一时间窗口内 ZCode 的请求平均 1.0 次尝试，Codex 的 8.3 次，
// 但把 item id 和 reasoning 都摘干净之后 Codex 仍被限流 5~21 次 —— 说明触发条件
// 在请求体的其它字段或客户端指纹头里。这个画像用来做「两边到底差在哪」的 diff。
// 字段名走白名单、未知字段只记个数；所有取值都映射到固定枚举，不接受自由字符串。
const KNOWN_BODY_KEYS = new Set(['model', 'input', 'stream', 'store', 'include', 'instructions',
  'tools', 'tool_choice', 'parallel_tool_calls', 'reasoning', 'text', 'metadata', 'previous_response_id',
  'conversation', 'max_output_tokens', 'prompt_cache_key', 'temperature', 'top_p', 'truncation',
  'service_tier', 'safety_identifier', 'prompt', 'background', 'max_tool_calls', 'response_format',
  'stop', 'seed', 'logprobs', 'top_logprobs', 'frequency_penalty', 'presence_penalty']);
const KNOWN_INCLUDE = new Set(['reasoning.encrypted_content', 'reasoning.summary',
  'message.output_text.logprobs', 'file_search_call.results', 'web_search_call.action.sources',
  'code_interpreter_call.outputs', 'computer_call_output.output.image_url']);
const EFFORT_TOKEN = /^[a-z]{1,16}$/;
function clientClass(headers) {
  const explicit = String(headers['x-relay-client'] || '').trim().toLowerCase();
  if (['codex', 'zcode', 'claude'].includes(explicit)) return explicit;
  const ua = String(headers['user-agent'] || '').toLowerCase();
  if (!ua) return 'none';
  if (ua.includes('zcode')) return 'zcode';
  if (ua.includes('claude')) return 'claude';
  if (/^codex(?:_cli_rs|_vscode|_desktop)?(?:\/|\s|$)/.test(ua.trim())) return 'codex';
  return 'other';
}
// Recovery eligibility must not inherit the diagnostic classifier's explicit-header precedence.
// Recognized non-Codex markers, including a conflicting UA, keep the existing paths.
function strictCodex(headers) {
  const explicit = String(headers['x-relay-client'] || '').trim().toLowerCase();
  const ua = String(headers['user-agent'] || '').trim().toLowerCase();
  if ((explicit && explicit !== 'codex') || /claude|zcode/.test(ua)) return false;
  return explicit === 'codex' || /^codex(?:_cli_rs|_vscode|_desktop)?(?:\/|\s|$)/.test(ua);
}

function requestProfile(bodyBuf, headers) {
  let obj;
  try { obj = JSON.parse(bodyBuf.toString('utf8')); } catch { return { body: 'invalid_json' }; }
  if (!obj || typeof obj !== 'object' || Array.isArray(obj)) return { body: 'non_object' };
  const present = Object.keys(obj);
  const bool = (v) => (typeof v === 'boolean' ? String(v) : 'other');
  const include = !Object.hasOwn(obj, 'include') ? 'absent'
    : !Array.isArray(obj.include) ? 'other'
    : (obj.include.map((v) => (KNOWN_INCLUDE.has(v) ? v : 'other')).join(',') || 'empty');
  return {
    client: clientClass(headers),
    keys: present.filter((k) => KNOWN_BODY_KEYS.has(k)).sort().join(','),
    unknownKeys: present.filter((k) => !KNOWN_BODY_KEYS.has(k)).length,
    store: !Object.hasOwn(obj, 'store') ? 'absent' : bool(obj.store),
    include,
    instructions: !Object.hasOwn(obj, 'instructions') ? 'absent'
      : typeof obj.instructions === 'string' ? `len=${obj.instructions.length}` : 'other',
    tools: !Object.hasOwn(obj, 'tools') ? 'absent' : Array.isArray(obj.tools) ? obj.tools.length : 'other',
    inputItems: Array.isArray(obj.input) ? obj.input.length : 'other',
    previousResponseId: obj.previous_response_id != null,
    parallelToolCalls: !Object.hasOwn(obj, 'parallel_tool_calls') ? 'absent' : bool(obj.parallel_tool_calls),
    reasoningEffort: !Object.hasOwn(obj, 'reasoning') ? 'absent'
      : (typeof obj.reasoning?.effort === 'string' && EFFORT_TOKEN.test(obj.reasoning.effort)
        ? obj.reasoning.effort : 'other'),
  };
}

function responsePartHasContent(part) {
  if (part?.type === 'output_text') return nonEmptyStr(part.text);
  if (part?.type === 'refusal') return nonEmptyStr(part.refusal);
  return true; // 未知载荷保守交付，不能把可能的真实输出当作空占位重放。
}

/**
 * 该事件是否携带「真实内容」。只有它出现，才认为上游确实开始正常回复。
 *
 * 必须同时支持两种协议，否则会把另一种协议的健康流误判成「空响应」：
 *   - OpenAI 风格：choices[].delta.content / tool_calls
 *   - Anthropic 风格：content_block_delta（text_delta / thinking_delta / input_json_delta）
 */
function contentBearing(obj, codexResponses = false) {
  if (!obj || typeof obj !== 'object') return false;

  // ── OpenAI / OpenAI 兼容 ──
  for (const choice of Array.isArray(obj.choices) ? obj.choices : []) {
    if (!choice) continue;
    const d = choice.delta ?? choice.message ?? {};
    if (nonEmptyStr(d.content)) return true;
    if (nonEmptyStr(d.reasoning_content) || nonEmptyStr(d.reasoning)) return true;
    if (Array.isArray(d.tool_calls) && d.tool_calls.length) return true;
    // finish_reason 单独出现（无内容）不算内容 —— 那正是「空回应」的特征
  }

  const t = String(obj.type || '');

  // ── Anthropic 风格 ──
  if (t === 'content_block_delta') {
    const d = obj.delta || {};
    // text_delta.text / thinking_delta.thinking / input_json_delta.partial_json
    if (nonEmptyStr(d.text) || nonEmptyStr(d.thinking) || nonEmptyStr(d.partial_json)) return true;
    if (d.type === 'text_delta' || d.type === 'thinking_delta' || d.type === 'input_json_delta') return true;
    return false;
  }
  if (t === 'content_block_start') {
    const cb = obj.content_block || {};
    if (nonEmptyStr(cb.text) || nonEmptyStr(cb.thinking)) return true;
    // tool_use / server_tool_use 刚开块时就算有内容在路上了
    if (cb.type === 'tool_use' || cb.type === 'server_tool_use' || cb.type === 'web_search_tool_result') return true;
    return false;
  }
  // 这些是 Anthropic 的边界/结束事件，本身不含内容
  if (t === 'message_start' || t === 'message_delta' || t === 'message_stop'
      || t === 'content_block_stop' || t === 'ping') return false;

  // ── OpenAI Responses 风格 ──
  if (t === 'response.completed') return true;
  if (t.endsWith('.delta') && nonEmptyStr(obj.delta)) return true;
  if (t === 'response.output_item.added' && obj.item?.type === 'reasoning') {
    const item = obj.item;
    return nonEmptyStr(item.encrypted_content) || [item.summary, item.content].some((parts) =>
      parts != null && (!Array.isArray(parts) || parts.some((part) =>
        !['summary_text', 'reasoning_text'].includes(part?.type) || nonEmptyStr(part.text))));
  }
  if (t === 'response.output_item.added' && obj.item?.type === 'message') {
    const parts = obj.item.content;
    return parts != null && (!Array.isArray(parts) || parts.some(responsePartHasContent));
  }
  if (t === 'response.content_part.added') return responsePartHasContent(obj.part);
  // 失败搜索没有可交付结果，留给后续响应错误走原有分类与重试。
  if (codexResponses && ['response.output_item.added', 'response.output_item.done'].includes(t)
      && obj.item?.type === 'web_search_call' && obj.item.status === 'failed') return false;
  // 搜索已完成就交付，不能为隐藏后续限流而重新执行整轮搜索。
  if (codexResponses && (t === 'response.web_search_call.completed'
      || (t === 'response.output_item.done' && obj.item?.type === 'web_search_call'))) return true;
  // Codex 的只读搜索开始不等于已有结果；客户端工具和未知状态仍保守交付。
  if (codexResponses && t === 'response.output_item.added' && obj.item?.type === 'web_search_call') {
    return obj.item.status != null && !['in_progress', 'searching'].includes(obj.item.status);
  }
  if (t === 'response.output_item.added') return true;

  if (nonEmptyStr(obj.content) || nonEmptyStr(obj.text)) return true;

  return false;
}

/** 非流式响应是否为「空回应」 */
function isEmptyCompletion(obj) {
  if (!obj || typeof obj !== 'object') return false;

  // ── Anthropic 风格：{ content: [{type:'text',text:'...'}, ...] } ──
  if (Array.isArray(obj.content) && obj.choices === undefined) {
    if (obj.content.length === 0) return true;
    const hasContent = obj.content.some((b) =>
      nonEmptyStr(b?.text) || nonEmptyStr(b?.thinking)
      || b?.type === 'tool_use' || b?.type === 'server_tool_use');
    return !hasContent;
  }

  // ── OpenAI / OpenAI 兼容 ──
  const c = obj.choices?.[0];
  if (!c) return false;
  const m = c.message ?? c.delta ?? {};
  if (Array.isArray(m.tool_calls) && m.tool_calls.length) return false;
  if (nonEmptyStr(m.reasoning_content) || nonEmptyStr(m.reasoning)) return false;
  return !nonEmptyStr(m.content);
}

/** 解析一段 SSE 原始文本里的 data: 负载 */
function ssePayload(rawEvent) {
  const data = [];
  for (const line of rawEvent.split(/\r?\n/)) {
    if (line.startsWith('data:')) data.push(line.slice(5).trimStart());
  }
  if (data.length === 0) return null;               // 注释 / 心跳
  const joined = data.join('\n');
  if (joined.trim() === '[DONE]') return { done: true };
  if (joined.startsWith('{')) {
    try { return { obj: JSON.parse(joined) }; } catch { return { obj: null }; }
  }
  try { return { obj: JSON.parse(joined) }; } catch { return { obj: null }; }
}

async function readWithTimeout(reader, ms) {
  let timer;
  try {
    return await Promise.race([
      reader.read(),
      new Promise((_, reject) => { timer = setTimeout(() => reject(new Error('IDLE_TIMEOUT')), ms); }),
    ]);
  } finally { clearTimeout(timer); }
}

// ── 响应输出 ────────────────────────────────────────────────────────────────

function passthrough(res, status, headers, buf) {
  const out = {};
  for (const [k, v] of headers.entries()) {
    if (['content-encoding', 'content-length', 'transfer-encoding', 'connection', 'x-relay-request-id'].includes(k.toLowerCase())) continue;
    out[k] = v;
  }
  res.writeHead(status, out);
  res.end(buf);
}

/**
 * 本地重试已耗尽：改写为 ZCode 认得的可重试状态。
 * attempts 为实际发起过的尝试次数（预算/连接失败会提前收手，可能小于 cfg.maxAttempts），
 * 只有拿不到真实次数时才回退到配置上限。type/code 保持可重试标识，不改变客户端重试行为。
 */
function sendRetryable(res, status, reason, attempts, policy = cfg) {
  if (res.headersSent) { try { res.end(); } catch { /* ignore */ } return; }
  const used = Number.isSafeInteger(attempts) && attempts > 0 ? attempts : policy.maxAttempts;
  const payload = JSON.stringify({
    error: {
      message: `relay-retry-proxy: local retry exhausted after ${used} attempts (${reason})`,
      type: 'too_many_requests',
      code: 'rate_limit_exceeded',
    },
  });
  res.writeHead(status, {
    'content-type': 'application/json; charset=utf-8',
    'retry-after': String(Math.max(1, Math.round(policy.baseDelayMs / 1000))),
    'content-length': Buffer.byteLength(payload),
  });
  res.end(payload);
}

/** 不可重试的错误：把真实原因回给 ZCode。status 用 402(配额)/400(其余) 便于区分。 */
function sendPermanent(res, reason, status = 400) {
  if (res.headersSent) { try { res.end(); } catch { /* ignore */ } return; }
  const payload = JSON.stringify({
    error: { message: `relay-retry-proxy: 不可重试的错误（${reason}）`, type: 'invalid_request_error', code: 'invalid_request_error' },
  });
  res.writeHead(status, {
    'content-type': 'application/json; charset=utf-8',
    'content-length': Buffer.byteLength(payload),
  });
  res.end(payload);
}

// ── 流式：先窥探，确认有真实内容再下发 ──────────────────────────────────────

function completedReasoningProgress(obj, seen) {
  if (obj.type !== 'response.output_item.done' || obj.item?.type !== 'reasoning') return false;
  const item = obj.item;
  const parts = [];
  if (nonEmptyStr(item.encrypted_content)) parts.push(['encrypted_content', item.encrypted_content]);
  for (const [field, type] of [['summary', 'summary_text'], ['content', 'reasoning_text']]) {
    if (!Array.isArray(item[field])) continue;
    for (const part of item[field]) {
      if (part?.type === type && nonEmptyStr(part.text)) parts.push([field, part.text]);
    }
  }
  if (!parts.length) return false;
  const contentKey = 'content:' + fingerprint(JSON.stringify(parts));
  const idKey = nonEmptyStr(item.id) ? 'id:' + fingerprint(item.id) : null;
  const repeated = seen.has(contentKey) || (idKey && seen.has(idKey));
  // Keep the deduplication set bounded without evicting old entries that could replay.
  if (seen.size + (idKey ? 2 : 1) > 65536) return false;
  seen.add(contentKey);
  if (idKey) seen.add(idKey);
  return !repeated;
}

function generationProgress(obj, seenReasoning) {
  if (!obj || typeof obj !== 'object') return false;
  if (completedReasoningProgress(obj, seenReasoning)) return true;
  for (const choice of Array.isArray(obj.choices) ? obj.choices : []) {
    const delta = choice?.delta;
    if (!delta) continue;
    if ([delta.content, delta.reasoning_content, delta.reasoning].some(nonEmptyStr)) return true;
    if (Array.isArray(delta.tool_calls) && delta.tool_calls.some((call) => nonEmptyStr(call?.function?.name) || nonEmptyStr(call?.function?.arguments))) return true;
  }
  if (obj.type === 'content_block_delta') {
    return [obj.delta?.text, obj.delta?.thinking, obj.delta?.partial_json].some(nonEmptyStr);
  }
  if (obj.type === 'content_block_start') {
    const block = obj.content_block;
    return [block?.text, block?.thinking, block?.name].some(nonEmptyStr);
  }
  if (typeof obj.type === 'string' && obj.type.startsWith('response.') && obj.type.endsWith('.delta')) {
    return nonEmptyStr(obj.delta);
  }
  if (obj.type === 'response.output_item.added') {
    return obj.item?.type === 'function_call' && nonEmptyStr(obj.item?.name);
  }
  return false;
}

async function peekAndCommit(upstream, res, onCommit = () => {}, state, codexResponses = false) {
  const reader = upstream.body.getReader();
  const decoder = new TextDecoder();
  let pending = '';
  const buffered = [];
  let bufferedBytes = 0;
  let committed = false;
  let bufferedSearch = false;
  let sawTerminal = false;
  let terminalKind = null;   // 固定枚举，诊断用：区分正常完成与 failed / incomplete
  let idleAfterCommit = false;
  let stallReason = null;
  let lastProgress = Date.now();
  const deadline = lastProgress + cfg.peekTotalMs;
  let streamError = null;
  let reachedEof = false;
  let stopReading = false;
  const seenReasoning = new Set();

  const commit = () => {
    committed = true;
    lastProgress = Date.now();
    onCommit();
    state.phase = 'stream_waiting';
    res.writeHead(200, { 'content-type': 'text/event-stream; charset=utf-8', 'cache-control': 'no-cache' });
    for (const chunk of buffered) res.write(chunk);
    buffered.length = 0;
  };
  // kind 是机器可读的失败类别，调用方据此决定要不要换通道 / 做形状修复。
  // 不要用格式化后的 reason 字符串去匹配：那种判断很脆，之前就因此把 peek 超时
  // 误当成「通道抖动」而放宽了尝试次数，覆盖了用户配置的上限。
  const failure = (reason, kind = 'other') => ({
    committed: false, reason, kind, errorMessage: streamError?.message,
    errorCode: streamError?.code || streamError?.type || null,
    classification: classifyError(streamError),
  });

  try {
    while (!stopReading) {
      const remaining = (committed || bufferedSearch ? lastProgress + cfg.streamProgressMs : deadline) - Date.now();
      if (remaining <= 0) {
        if (!committed) return failure('timed out waiting for first content (empty/stalled)', 'peek_timeout');
        stallReason = 'progress_timeout';
        break;
      }
      const idleMs = committed || bufferedSearch ? cfg.streamIdleMs : cfg.peekIdleMs;
      let chunk;
      try {
        chunk = await readWithTimeout(reader, Math.min(idleMs, remaining));
      } catch (e) {
        if (!committed) {
          // 上游「粗暴断连」（不发 FIN 直接销毁 socket）与「正常关流」是同一种静默丢弃：
          // 都是 HTTP 200、只回显 response.created 就没了下文。此前这里返回的
          // `stream read timeout (terminated)` 与 EMPTY_STREAM_DROP 不相等，导致换通道
          // 与请求修复的分支永远进不去 —— 生产诊断里该修复 0 次触发。
          // 只有我们自己设的空闲超时（IDLE_TIMEOUT）才算真正的「等不到」。
          if (e.message !== 'IDLE_TIMEOUT') {
            log(`upstream closed the connection before any content (${e.message})`);
            return failure(EMPTY_STREAM_DROP, 'empty_stream_drop');
          }
          return failure(`stream read timeout (${e.message})`, 'read_timeout');
        }
        idleAfterCommit = true;
        stallReason = res.destroyed ? 'client_closed'
          : e.message === 'IDLE_TIMEOUT' ? (remaining <= idleMs ? 'progress_timeout' : 'transport_idle_timeout')
          : 'upstream_read_error';
        break;
      }
      if (chunk.done) { reachedEof = true; break; }
      const raw = Buffer.from(chunk.value);
      state.bytesReceived += raw.length;
      state.lastByte = Date.now();
      if (committed) res.write(raw);
      else {
        buffered.push(raw);
        bufferedBytes += raw.length;
        if (bufferedBytes > cfg.sseBufferMaxBytes) return failure('pre-content stream exceeded buffer limit', 'buffer_limit');
      }
      pending += decoder.decode(raw, { stream: true });
      const sep = /\r?\n\r?\n/;
      let match;
      while ((match = sep.exec(pending))) {
        const event = pending.slice(0, match.index);
        pending = pending.slice(match.index + match[0].length);
        const payload = ssePayload(event);
        state.eventCount++;
        if (!payload) continue;
        if (payload.done) { sawTerminal = true; terminalKind = 'done_marker'; stopReading = true; continue; }
        if (!payload.obj) continue;
        const err = errorInfoFromJson(payload.obj);
        if (err && !committed) {
          streamError = err;
          return failure(`HTTP 200 but stream carried error: ${err.code || ''} ${err.message}`, 'stream_error');
        }
        if (isTerminalEvent(payload.obj)) {
          sawTerminal = true;
          // 只记协议级终止类型（固定枚举），绝不落盘上游任意字符串：
          // 这样诊断能区分「正常完成」与 failed / incomplete / [DONE]。
          const terminalType = payload.obj.type;
          terminalKind = RESPONSES_TERMINAL.has(terminalType) ? terminalType
            : terminalType === 'message_stop' ? 'message_stop' : 'chat_finish_reason';
          // Chat finish_reason can precede a usage-only chunk; wait for its [DONE].
          if (RESPONSES_TERMINAL.has(terminalType) || terminalType === 'message_stop') stopReading = true;
        }
        const madeProgress = !err && generationProgress(payload.obj, seenReasoning);
        if (madeProgress) lastProgress = Date.now();
        const hasContent = !err && !committed && contentBearing(payload.obj, codexResponses);
        if (codexResponses && !committed && !err && !hasContent
            && payload.obj.type === 'response.output_item.added' && payload.obj.item?.type === 'web_search_call') {
          // 只推迟下发，等待容忍度仍从首次搜索开始沿用旧版已交付流的参数。
          if (!bufferedSearch) lastProgress = Date.now();
          bufferedSearch = true;
        }
        // 原先搜索开始已交付，明确的 incomplete 终态必须继续原样到达客户端。
        if (!err && !committed && (madeProgress || hasContent
            || (bufferedSearch && payload.obj.type === 'response.incomplete'))) commit();
        if (madeProgress) {
          state.lastProgress = Date.now();
          const kind = progressKind(payload.obj);
          state.phase = kind === 'reasoning' ? 'reasoning' : kind === 'tool' ? 'tool_output' : 'text_output';
          state[kind === 'reasoning' ? 'reasoningEvents' : kind === 'tool' ? 'toolEvents' : 'textEvents']++;
        }
      }
      // Bound a malformed or unterminated event without buffering the entire stream.
      if (pending.length > cfg.sseBufferMaxBytes) {
        if (!committed) return failure('SSE event exceeded buffer limit', 'buffer_limit');
        stallReason = 'event_size_limit';
        break;
      }
    }
    if (!committed) return failure(EMPTY_STREAM_DROP, 'empty_stream_drop');
    if (stallReason || !sawTerminal) log(`stream ended without successful completion: ${stallReason || 'missing_terminal'}${terminalKind ? ' terminal=' + terminalKind : ''}`);
    res.end();
    return { committed: true, sawTerminal, terminalKind, idleAfterCommit, stallReason };
  } finally {
    // A terminal event does not require the upstream to close its HTTP connection.
    if (!reachedEof) await reader.cancel().catch(() => {});
    reader.releaseLock();
  }
}

// ── 主处理 ──────────────────────────────────────────────────────────────────

async function handle(req, res, state) {
  const requestId = state.requestId; // Never reuse client headers or upstream trace ids.
  const pathname = new URL(req.url, 'http://localhost').pathname;
  const safePath = state.path;
  let repairCount = 0;
  let attemptCount = 0;
  const diagnose = (event, fields = {}) => safeDiagnostic({ requestId, event, path: safePath,
    attempt: attemptCount, repairCount, ...fields });
  res.setHeader('x-relay-request-id', requestId);
  res.once('finish', () => diagnose('finished', { status: res.statusCode }));
  res.once('close', () => { if (!res.writableFinished) diagnose('client_closed'); });
  if (res.destroyed || req.aborted) return;
  const ac = new AbortController();
  res.once('close', () => { if (!res.writableFinished) ac.abort(); });
  const chunks = [];
  for await (const c of req) chunks.push(c);
  if (ac.signal.aborted || res.destroyed) return;
  const body = Buffer.concat(chunks);
  const target = joinUrl(cfg.upstream, req.url);

  const headers = {};
  for (const [k, v] of Object.entries(req.headers)) {
    // 原样保留所有客户端特征头（中转站会校验指纹），只丢弃下面这几个逐跳/传输层头：
    //   host / content-length / connection / transfer-encoding —— 由本地重新生成
    //   accept-encoding —— 必须为空，否则上游会压缩，窥探逻辑读不到明文
    //   expect —— 客户端用 `Expect: 100-continue` 做大请求体的预检；请求体已在上面
    //             完整读进内存，不需要这个机制。而 undici 不支持该头，转发会直接抛
    //             UND_ERR_NOT_SUPPORTED（expect header not supported）导致 502。
    if (['host', 'content-length', 'connection', 'accept-encoding', 'transfer-encoding', 'expect'].includes(k.toLowerCase())) continue;
    headers[k] = Array.isArray(v) ? v.join(', ') : v;
  }

  const noBody = req.method === 'GET' || req.method === 'HEAD';
  let lastReason = 'unknown';
  // Unknown histories retain their original bytes until a recognized error.
  let activeBody = body;
  let repairedItems = false; // one shared repair allowance, including legacy reasoning errors
  let preserveResourceHistory = false;
  let preflightRefused = false;
  let repairedStatus = false;
  let resourceFailures = 0;
  const historyErrorCounts = new Map();
  const isResponses = req.method === 'POST' && pathname === '/v1/responses';
  const clientCodex = isResponses && strictCodex(headers);
  // Classification must not change when a user disables a recovery action.
  const eligibleCodex = clientCodex && (hasPolicySettings || cfg.codexResourceRetry);
  const policy = isResponses ? policySettings.profiles[eligibleCodex ? 'codex' : 'general'] : defaultPolicyProfile(false);
  const codexResourceRetry = isResponses && clientClass(headers) === 'codex'
    && (hasPolicySettings ? policySettings.profiles.codex.retryHistory : cfg.codexResourceRetry);
  let historyRepairChecked = false;
  // 「修复版最多再发三次」只约束本次确实按历史错误做过有损降级的请求。
  // 会话记忆的预清理（首发前就剥离）不构成历史错误证据：实测它覆盖了 Codex 的
  // 全部请求，再叠上这条上限，纯限流也只能试 3 次就放弃（生产一天 83 次，配置的
  // 110 次形同虚设）。没报过历史错误的请求按通用预算重试。
  let historyRepairApplied = false;
  let bindingIdRepairApplied = false;
  let postRepairSends = 0;
  let frozenHistoryKind = null;
  diagnose('received', isResponses ? { shape: summarizeResponsesBody(body) } : {});
  if (isResponses) diagnose('request_profile', requestProfile(body, headers));
  // Keep Codex fallback memory separate; other clients retain their existing policy.
  const compatibilityCache = eligibleCodex ? codexCompatibilityKeys : compatibilityKeys;
  const compatibility = isResponses ? compatibilityIdentity(body, headers,
    policy.rememberSession && policy.historyMode === 'on_error') : null;
  const cacheRoute = isResponses ? cacheRouteIdentity(body, headers) : null;
  delete headers['x-relay-session-id'];
  delete headers['x-relay-client'];
  if (isResponses && policy.historyMode === 'always') {
    // Commit the entire candidate or none of it, including the optional ID cleanup.
    // An unsupported reference must keep the caller's original bytes intact.
    const cleaned = repairResponsesItems(body, {
      stripItemIds: true, dropReasoning: true, allowUnchanged: true, codexHistory: eligibleCodex,
      requireCompleteTools: true,
    });
    if (cleaned.error) {
      preflightRefused = true;
      preserveResourceHistory = true;
      log('policy preflight: unsupported history retained unchanged');
    } else {
      activeBody = cleaned.body;
      repairedItems = !!(cleaned.removedIds || cleaned.removedReasoning);
      historyRepairChecked = eligibleCodex;
      if (repairedItems) repairCount++;
      log(`policy preflight: removed ids=${cleaned.removedIds}, reasoning=${cleaned.removedReasoning}`);
    }
  } else {
    if (clientCodex && (hasPolicySettings ? policy.stripItemIds : cfg.codexStripIds)) {
      const stripped = stripCodexItemIds(activeBody);
      if (stripped) {
        activeBody = stripped.body;
        diagnose('session_compat_applied', { source: compatibility?.source ?? 'resource_lineage',
          removedReasoning: 0, removedIds: stripped.removedIds });
        log(`codex item ids stripped before first attempt: ${stripped.removedIds}`);
      }
    }
    if (knownCompatibility(compatibility, compatibilityCache)) {
      const cleaned = repairResponsesItems(activeBody, { stripItemIds: true, dropReasoning: true, allowUnchanged: true, codexHistory: eligibleCodex });
      if (cleaned.error) {
        diagnose('session_compat_refused', { reason: 'unsafe_inline_history' });
        if (!codexResourceRetry) {
          return sendPermanent(res, 'Session compatibility requires complete inline message and tool history; server-side references or unsupported items cannot be discarded.');
        }
        diagnose('codex_session_passthrough', { reason: 'unsafe_inline_history' });
      } else {
        activeBody = cleaned.body;
        rememberCompatibility(compatibility, compatibilityCache);
        repairedItems = true;
        diagnose('session_compat_applied', { source: compatibility.source,
          removedReasoning: cleaned.removedReasoning, removedIds: cleaned.removedIds });
      }
    }
  }
  // Reuse a proven replacement rather than paying for a new cold cache on every turn.
  if (cacheRoute) {
    for (const [key, entry] of cacheRouteKeys) if (entry.expires <= Date.now()) cacheRouteKeys.delete(key);
  }
  let cacheRouteKey = cacheRoute ? cacheRouteKeys.get(cacheRoute)?.key : null;
  if (cacheRouteKey) activeBody = withCacheRouteKey(activeBody, cacheRouteKey);
  let cacheRateFailures = 0;
  let cacheRouteChanged = false;
  // 上限可随着「通道类抖动」动态放宽
  let maxAttempts = policy.maxAttempts;
  const deadline = Date.now() + policy.totalBudgetMs;
  const outOfBudget = () => Date.now() >= deadline;
  // A capped attempt must not spend another backoff interval before discovering the cap.
  const retrySleep = (ms) => eligibleCodex && historyRepairApplied && postRepairSends >= 3
    ? Promise.resolve() : sleep(ms, ac.signal);
  // 大请求（长对话）要等更久才等到首个响应头
  const attemptTimeoutMs = connectTimeoutFor(body.length, policy.totalBudgetMs, policy.connectTimeoutMs);
  log(`incoming ${req.method} ${req.url} body=${(body.length / 1024).toFixed(0)}KB ` +
      `waitHeader=${Math.round(attemptTimeoutMs / 1000)}s budget=${Math.round(policy.totalBudgetMs / 1000)}s`);

  // A sticky upstream cache route can rate-limit indefinitely despite fresh connections.
  // Two explicit rate errors permit one new cache key, never a new session or lost history.
  const noteCacheRateLimit = (classification, attempt) => {
    if (!cacheRoute || cacheRouteChanged) return;
    if (classification.permanent || !classification.retryable ||
        String(classification.info?.code || '').toLowerCase() !== 'rate_limit_exceeded') {
      cacheRateFailures = 0;
      return;
    }
    if (++cacheRateFailures < 2 || attempt >= maxAttempts || ac.signal.aborted ||
        deadline - Date.now() < cfg.minRetryWindowMs + policy.maxDelayMs) return;
    cacheRouteKey = `ag-relay-${randomUUID()}`;
    activeBody = withCacheRouteKey(activeBody, cacheRouteKey);
    cacheRouteChanged = true;
    log('repeated rate limit: trying one fresh cache route; history and session identity retained');
  };
  const rememberCacheRoute = () => {
    if (!cacheRoute || !cacheRouteKey) return;
    cacheRouteKeys.delete(cacheRoute);
    cacheRouteKeys.set(cacheRoute, { key: cacheRouteKey, expires: Date.now() + cfg.sessionCompatTtlMs });
    while (cacheRouteKeys.size > cfg.sessionCompatMaxKeys) cacheRouteKeys.delete(cacheRouteKeys.keys().next().value);
  };

  const handleStatusError = (classification, attempt, source, status) => {
    if (!policy.repairStatus || classification.kind !== 'status') return null;
    const index = classification.statusIndex;
    const refuse = (reason) => {
      diagnose('status_repair_refused', { source, status, reason });
      sendPermanent(res, `Unsupported input status field: ${reason}. Correct the upstream request conversion.`);
      return 'stop';
    };
    if (bindingIdRepairApplied) return refuse('repair_exhausted');
    if (!isResponses) return refuse('wrong_endpoint');
    if (repairedStatus || (eligibleCodex && historyRepairChecked)) return refuse('repair_exhausted');
    if (status !== 400 && status !== 200) return refuse('unexpected_status');
    let obj;
    try { obj = JSON.parse(activeBody.toString('utf8')); }
    catch { return refuse('invalid_json'); }
    const item = Array.isArray(obj?.input) && Number.isSafeInteger(index) ? obj.input[index] : null;
    if (!item || typeof item !== 'object' || Array.isArray(item) ||
        !['message', 'function_call', 'function_call_output'].includes(item.type) ||
        !Object.hasOwn(item, 'status') || !['completed', 'in_progress', 'incomplete'].includes(item.status)) {
      return refuse('unsupported_item');
    }
    if (deadline - Date.now() < cfg.minRetryWindowMs + cfg.deployDelayMs) return refuse('time_budget');
    // Remove only the explicitly rejected metadata field, never nested tool/user data.
    delete item.status;
    activeBody = Buffer.from(JSON.stringify(obj), 'utf8');
    repairedStatus = true;
    maxAttempts = Math.max(maxAttempts, attempt + 1);
    diagnose('status_repair_applied', { source, status, index });
    return 'retry';
  };

  // Called uniformly for HTTP errors, pre-commit SSE errors and 200 JSON errors.
  const handleItemError = (classification, attempt, source, status) => {
    if (classification.sessionBindingInvalidated && eligibleCodex && policy.historyMode !== 'off') {
      const kind = 'session_binding_invalidated';
      // A retry is useful only with a changed request. Refusal or repeated failure
      // must stop, including SSE/200 error envelopes, rather than discard reasoning.
      classification.permanent = true;
      classification.retryable = false;
      diagnose('item_error', { kind, source, status, shape: summarizeResponsesBody(activeBody) });
      const refuse = (refusal) => {
        diagnose('repair_refused', { kind, source, status, refusal });
        return null;
      };
      if (bindingIdRepairApplied || repairedItems) return refuse('repair_exhausted');
      if (preflightRefused) return refuse('unsafe_inline_history');
      if (deadline - Date.now() < cfg.minRetryWindowMs + cfg.deployDelayMs) return refuse('time_budget');
      const confirmations = (historyErrorCounts.get(kind) || 0) + 1;
      historyErrorCounts.set(kind, confirmations);
      if (confirmations < policy.historyConfirmations) {
        return attempt < maxAttempts ? 'retry' : refuse('time_budget');
      }
      const repaired = repairResponsesItems(activeBody, {
        stripItemIds: true, stripReasoningIds: true, codexHistory: true,
      });
      if (repaired.error) return refuse('unsafe_inline_history');
      activeBody = repaired.body;
      bindingIdRepairApplied = true;
      repairedItems = true;
      repairCount++;
      maxAttempts = Math.max(maxAttempts, attempt + 1);
      // Never register this in the old compatibility cache: that cache means
      // dropReasoning on the next request. Client headers/metadata stay untouched.
      diagnose('repair_applied', { kind, source, status, removedIds: repaired.removedIds, removedReasoning: 0 });
      log(`attempt ${attempt}: ${kind}: removed item ids=${repaired.removedIds}; reasoning and client identity retained`);
      return 'retry';
    }
    if (bindingIdRepairApplied && classification.kind === 'history') {
      classification.permanent = true;
      classification.retryable = false;
      diagnose('repair_refused', { kind: classification.historyKind, source, status, refusal: 'repair_exhausted' });
      return null;
    }
    if (classification.kind !== 'history' || (classification.missingSearchReasoning && !eligibleCodex)) return null;
    const kind = classification.historyKind;
    const azure = kind === 'azure_resource_mismatch';
    diagnose('item_error', { kind, source, status, shape: summarizeResponsesBody(activeBody) });
    // 两类历史错误都可能来自当前通道无法接续；无法安全改写不等于无法原样重试。
    if ((codexResourceRetry || (hasPolicySettings && eligibleCodex)) && ++resourceFailures >= 5) {
      diagnose('codex_resource_exhausted', { kind, source, status, resourceFailures, refusal: 'resource_attempt_limit' });
      log(`attempt ${attempt}: ${kind}; stopping after ${resourceFailures} resource attempts`);
      sendPermanent(res, `${kind}: stopping after ${resourceFailures} resource attempts; upstream could not access the retained response history.`);
      return 'stop';
    }
    const fail = (reason, refusal = 'unsafe_inline_history') => {
      diagnose('repair_refused', { kind, source, status, refusal });
      if (codexResourceRetry) {
        preserveResourceHistory = true;
        // Refusing a lossy rewrite does not rule out another route accepting the same history.
        lastReason = `${kind}: upstream could not access the retained response history`;
        if (ac.signal.aborted) return 'stop';
        if (attempt < maxAttempts && deadline - Date.now() >= cfg.minRetryWindowMs + cfg.deployDelayMs) {
          diagnose('codex_resource_retry', { kind, source, status, refusal });
          log(`attempt ${attempt}: ${kind}; retaining request history and retrying`);
          return 'retry';
        }
        diagnose('codex_resource_exhausted', { kind, source, status, refusal });
        sendRetryable(res, 503, lastReason, attempt, policy);
        return 'stop';
      }
      log(`attempt ${attempt}: ${kind}: repair refused: ${reason}; not retrying`);
      sendPermanent(res, `${kind}: ${reason}. Use the original Azure resource or resend complete inline history without server-side references; reasoning continuity cannot be preserved.`);
      return 'stop';
    };
    if (policy.historyMode === 'off') return fail('history recovery is disabled by policy');
    if (preflightRefused) return fail('preflight history failed safety validation');
    if (!isResponses) return fail('automatic item repair is restricted to POST /v1/responses', 'wrong_endpoint');
    // The default confirms on the first error. Custom thresholds accumulate only
    // this request's same-kind history evidence, never throttling or empty replies.
    if (eligibleCodex && historyRepairChecked) {
      lastReason = `${kind}: upstream could not access the retained response history`;
      if (ac.signal.aborted) return 'stop';
      if (attempt < maxAttempts && deadline - Date.now() >= cfg.minRetryWindowMs + cfg.deployDelayMs) {
        diagnose('codex_resource_retry', { kind, source, status, resourceFailures });
        return 'retry';
      }
      diagnose('codex_resource_exhausted', { kind, source, status, refusal: 'time_budget' });
      sendRetryable(res, 503, lastReason, attempt, policy);
      return 'stop';
    }
    if (repairedItems) return fail('single item-repair attempt exhausted', 'repair_exhausted');
    const confirmations = (historyErrorCounts.get(kind) || 0) + 1;
    historyErrorCounts.set(kind, confirmations);
    if (eligibleCodex && confirmations >= policy.historyConfirmations) historyRepairChecked = true;
    const repaired = repairResponsesItems(activeBody, { stripItemIds: azure || eligibleCodex, dropReasoning: true, codexHistory: eligibleCodex });
    if (repaired.error) return fail(repaired.error);
    if (deadline - Date.now() < cfg.minRetryWindowMs + cfg.deployDelayMs) return fail('insufficient time budget for repair', 'time_budget');
    if (confirmations < policy.historyConfirmations) {
      if (ac.signal.aborted) return 'stop';
      if (attempt >= maxAttempts) return fail('history confirmation budget exhausted', 'time_budget');
      log(`attempt ${attempt}: ${kind}; confirmed ${confirmations}/${policy.historyConfirmations}, retaining history`);
      return 'retry';
    }
    if ((azure || eligibleCodex) && rememberCompatibility(compatibility, compatibilityCache)) {
      diagnose('session_compat_enabled', { kind, source: compatibility.source });
    }
    activeBody = repaired.body;
    repairedItems = true;
    if (eligibleCodex) { frozenHistoryKind = kind; historyRepairApplied = true; }
    repairCount++;
    diagnose('repair_applied', { kind, source, status, removedIds: repaired.removedIds, removedReasoning: repaired.removedReasoning });
    maxAttempts = Math.max(maxAttempts, attempt + (eligibleCodex ? 3 : 1));
    log(`attempt ${attempt}: ${kind}: removed item ids=${repaired.removedIds}, reasoning=${repaired.removedReasoning}; single repair retry (reasoning continuity may be lost)`);
    return 'retry';
  };

  /**
   * 首段内容之前的失败（静默丢弃 / 200 流内可重试错误 / 空回应）的统一处理。
   *
   * 同一份请求体换个通道往往就能成功，所以先按「通道抖动」处理：短间隔、多次数，
   * 而不是指数退避 —— 退避会让一次 5 尝试的请求光睡觉就花掉约 30 秒。
   *
   * 连续失败说明不是通道问题，而是请求体形状本身。实测同一时间窗口的 25 个真实请求：
   * 带 item id 的 91% 尝试被 `rate_limit_exceeded` 拒绝、平均 11.2 次尝试才成功、首试 0/15；
   * 不带 id 的只有 29% 被拒、平均 1.4 次、首试 6/10。两个更早时段（1013 / 445 个请求）
   * 也是同一方向（带 id 首试 48% / 9%，不带 id 81% / 73%）。也就是上游按「请求里有没有
   * 引用句柄」限流，与 reasoning 无关（「剥掉 reasoning 但保留 id」没有任何改善）。
   *
   * 因此触发条件必须覆盖所有首段失败，而不只是静默丢弃那一条：修复分两级，先做无损的：
   *   1 级（无损）：摘掉所有 item 的 id 句柄与 message 的 type 标签，reasoning 条目
   *                 连同 encrypted_content/summary 原样保留；
   *   2 级（有损）：仍然失败时才追加删除 reasoning 项，宁可损失推理连续性也不要拖到 429。
   */
  let preCommitFailures = 0;
  let shapeRepairStage = 0;
  let shapeNormalized = false;   // 1 级是否确实改动过请求体（形状假设是否成立）
  const repairShape = (stage, attempt, source) => {
    const stripReasoning = stage === 2;
    const kind = stripReasoning ? 'empty_stream_reasoning' : 'empty_stream_shape';
    const repaired = stripReasoning
      ? repairResponsesItems(activeBody, { stripItemIds: true, stripReasoningIds: true, stripMessageType: true, dropReasoning: true })
      : repairResponsesItems(activeBody, { stripItemIds: true, stripReasoningIds: true, stripMessageType: true });
    const changed = repaired.removedIds || repaired.removedTypes || repaired.removedReasoning;
    if (repaired.error || !changed) {
      diagnose('repair_refused', { kind, source, status: 200,
        refusal: repaired.error || 'nothing left to normalize' });
      return false;
    }
    activeBody = repaired.body;
    repairedItems = true;
    if (stage === 1) shapeNormalized = true;
    repairCount++;
    diagnose('repair_applied', { kind, source, status: 200, removedIds: repaired.removedIds,
      removedTypes: repaired.removedTypes, removedReasoning: repaired.removedReasoning });
    log(`attempt ${attempt}: pre-commit failure #${preCommitFailures} (${source}); `
      + (stripReasoning
        ? `removed reasoning items=${repaired.removedReasoning}; retrying without reasoning continuity`
        : `removed item ids=${repaired.removedIds}, message type tags=${repaired.removedTypes}; retrying with reasoning intact`));
    return true;
  };
  const notePreCommitFailure = (attempt, source) => {
    if (preserveResourceHistory && !eligibleCodex) return;
    preCommitFailures++;
    maxAttempts = Math.max(maxAttempts, cfg.deployMaxAttempts);
    // Codex history repair requires an explicit history error; empty replies alone
    // must neither trigger a lossy repair nor mutate the frozen/request-retained body.
    if (!isResponses || eligibleCodex || !policy.emptyResponseRepair) return;
    // 1 级无损，2 次失败就做。2 级要牺牲推理连续性，所以要多 4 次失败的证据，
    // 且只在 1 级确实改动过请求体时才做 —— 若 1 级无可改之处，说明形状假设不成立，
    // 那是与形状无关的真实限流，不该拿推理连续性去赌。
    if (shapeRepairStage === 0 && !repairedItems && preCommitFailures >= policy.emptyResponseNormalizeAfter) {
      shapeRepairStage = 1;
      if (!repairShape(1, attempt, source)) return;
    }
    if (shapeRepairStage === 1 && shapeNormalized && policy.emptyResponseDropReasoning
        && preCommitFailures >= policy.emptyResponseReasoningAfter) {
      shapeRepairStage = 2;
      repairShape(2, attempt, source);
    }
  };

  for (let attempt = 1; attempt <= maxAttempts; attempt++) {
    if (ac.signal.aborted || res.destroyed) return;
    if (eligibleCodex && historyRepairApplied && postRepairSends >= 3) {
      diagnose('codex_resource_exhausted', { kind: frozenHistoryKind, resourceFailures,
        refusal: 'resource_attempt_limit' });
      return sendRetryable(res, 503, lastReason, attemptCount, policy);
    }
    // 剩余预算不足以支撑「再等一次 + 还可能再重试」时，就别开新尝试了：
    // 否则只会白等到客户端自己超时。
    const remaining = deadline - Date.now();
    if (attempt > 1 && remaining < cfg.minRetryWindowMs) {
      lastReason = `gave up retrying: only ${Math.round(remaining / 1000)}s of budget left`;
      log(`attempt ${attempt}: ${lastReason}`);
      return sendRetryable(res, 503, lastReason, attempt - 1, policy);
    }
    // 每次尝试独立控制器：既能被客户端断开连带中止，也能被超时单独中止
    const attemptAc = new AbortController();
    const onParentAbort = () => { if (!attemptAc.signal.aborted) attemptAc.abort(ac.signal.reason); };
    ac.signal.addEventListener('abort', onParentAbort, { once: true });
    let connectTimedOut = false;
    const waitMs = Math.max(1000, Math.min(attemptTimeoutMs, remaining));
    const connectTimer = setTimeout(() => {
      connectTimedOut = true;
      if (!attemptAc.signal.aborted) attemptAc.abort(new Error('CONNECT_TIMEOUT'));
    }, waitMs);

    try {
      attemptCount = attempt;
      state.attempt = attempt;
      state.phase = 'waiting_headers';
      diagnose('attempt_started');
      let upstream;
      try {
        // 每条上游请求都用新连接。实测：网关自己重试 8 次、磨 110–180 秒全部失败，而 1–3 秒后
        // 客户端同样的新请求就能成功——1–3 秒不可能是限流窗口恢复的时间，说明重试沿用的那条
        // 上游连接被持续拒绝，换新连接即可通过。只给重试加还不够：首次尝试若保持 keep-alive，
        // 它的空闲连接会被下一次重试捡走（test/retry-fresh-connection.mjs 可复现），所以一律用
        // `connection: close`。代价是每条请求多一次握手（相对 20–40 秒的请求可忽略）。
        // `connection` 是逐跳头，站点看到的其它请求头与指纹没有任何变化。
        // Count actual fetch calls, even when no HTTP response arrives (network failure).
        if (eligibleCodex && historyRepairApplied) postRepairSends++;
        upstream = await fetch(target, {
          method: req.method,
          headers: { ...headers, connection: 'close' },
          body: noBody ? undefined : activeBody,
          redirect: 'manual',
          signal: attemptAc.signal,
        });
      } catch (e) {
        if (ac.signal.aborted) return;
        if (outOfBudget()) {
          lastReason = `total budget ${policy.totalBudgetMs}ms exhausted`;
          log(`attempt ${attempt}: ${lastReason}`);
          return sendRetryable(res, 503, lastReason, attempt, policy);
        }
        lastReason = connectTimedOut
          ? `upstream connect timeout after ${attemptTimeoutMs}ms`
          : `upstream connect failed: ${describeFetchError(e)}`;
        log(`attempt ${attempt}: ${lastReason}`);
        // 连接超时/失败属于通道抖动：放宽次数、缩短间隔，快速换通道
        maxAttempts = Math.max(maxAttempts, cfg.deployMaxAttempts);
        if (attempt < maxAttempts) { await retrySleep(cfg.deployDelayMs); continue; }
        return sendRetryable(res, 502, lastReason, attempt, policy);
      } finally {
        clearTimeout(connectTimer);   // 已拿到响应头，后续由 peek 的读取超时接管
      }

      state.phase = 'reading_body';
      diagnose('upstream_response', { status: upstream.status });
      if (!upstream.ok) {                       // 真实的 HTTP 错误
        const buf = Buffer.from(await upstream.arrayBuffer());
        const text = buf.toString('utf8');

        let errorInfo = null;
        try { errorInfo = errorInfoFromJson(JSON.parse(text)); }
        catch { errorInfo = { message: text.trim() }; }
        const classification = classifyError(errorInfo, upstream.status);
        noteCacheRateLimit(classification, attempt);
        const statusAction = handleStatusError(classification, attempt, 'http_error', upstream.status);
        if (statusAction === 'stop') return;
        if (statusAction === 'retry') { await retrySleep(cfg.deployDelayMs); continue; }
        const itemAction = handleItemError(classification, attempt, 'http_error', upstream.status);
        if (itemAction === 'stop') return;
        if (itemAction === 'retry') { await retrySleep(cfg.deployDelayMs); continue; }

        if (classification.permanent) {
          lastReason = `HTTP ${upstream.status} deterministic error: ${snippet(buf)}`;
          log(`attempt ${attempt}: ${lastReason}`);
          log('  -> deterministic error, not retrying');
          return passthrough(res, upstream.status, upstream.headers, buf);
        }
        if (!classification.retryable) return passthrough(res, upstream.status, upstream.headers, buf);
        lastReason = `HTTP ${upstream.status} ${snippet(buf)}`;
        log(`attempt ${attempt}: ${lastReason}`);
        if (classification.deployment) {
          maxAttempts = Math.max(maxAttempts, cfg.deployMaxAttempts);
          if (attempt < maxAttempts) {
            log('  -> deployment missing on this route, retrying for another route');
            await retrySleep(cfg.deployDelayMs);
            continue;
          }
          return sendRetryable(res, 429, lastReason, attempt, policy);
        }
        if (attempt < maxAttempts) { await retrySleep(backoffMs(attempt, policy)); continue; }
        return sendRetryable(res, upstream.status === 429 ? 429 : 503, lastReason, attempt, policy);
      }

      const ct = (upstream.headers.get('content-type') || '').toLowerCase();

      if (ct.includes('text/event-stream')) {   // 流式
        state.phase = 'waiting_first_content';
        const outcome = await peekAndCommit(upstream, res, () => diagnose('sse_committed', { status: 200 }), state, clientCodex);
        if (outcome.committed) {
          if (outcome.terminalKind === 'response.completed') rememberCacheRoute();
          diagnose('sse_ended', { status: 200, sawTerminal: outcome.sawTerminal, terminalKind: outcome.terminalKind, idleAfterCommit: outcome.idleAfterCommit, stallReason: outcome.stallReason });
          if (attempt > 1) log(`attempt ${attempt}: recovered on a later route (previous failures: ${attempt - 1})`);
          return;
        }
        if (ac.signal.aborted) return;
        lastReason = outcome.reason;
        const classification = outcome.classification;
        noteCacheRateLimit(classification, attempt);
        log(`attempt ${attempt}: ${lastReason}`);
        // 有界诊断：把「200 流内的失败」分类成固定枚举 + 上游错误指纹（不落盘文案）。
        // 之前这类失败没有任何诊断记录，只能靠反推，代价很大。
        {
          const fp = errorFingerprint(outcome.errorMessage
            ? { code: outcome.errorCode, message: outcome.errorMessage } : null);
          diagnose('precommit_failure', {
            source: 'sse_precommit',
            kind: outcome.kind,
            code: fp ? fp.code : 'none',
            hints: fp ? fp.hints : '',
            masked: fp ? fp.masked : false,
            retryable: classification.retryable,
          });
        }
        const statusAction = handleStatusError(classification, attempt, 'sse_precommit', upstream.status);
        if (statusAction === 'stop') return;
        if (statusAction === 'retry') { await retrySleep(cfg.deployDelayMs); continue; }
        const itemAction = handleItemError(classification, attempt, 'sse_precommit', upstream.status);
        if (itemAction === 'stop') return;
        if (itemAction === 'retry') { await retrySleep(cfg.deployDelayMs); continue; }
        if (classification.permanent) {                // 确认的确定性错误：不重试，直接失败
          log(classification.quota
            ? '  -> quota exhausted, not retrying (switch model or wait for the pool to refill)'
            : '  -> permanent error, not retrying');
          return sendPermanent(res, lastReason, classification.quota ? 402 : 400);
        }
        if (classification.deployment) {               // 通道缺部署：放宽次数、缩短间隔，快速换通道
          maxAttempts = Math.max(maxAttempts, cfg.deployMaxAttempts);
          if (attempt < maxAttempts) {
            log('  -> deployment missing on this route, retrying for another route');
            await retrySleep(cfg.deployDelayMs);
            continue;
          }
        }
        if (outcome.kind === 'empty_stream_drop') {   // 上游静默丢弃：快速换通道 + 形状修复升级
          notePreCommitFailure(attempt, 'sse_precommit');
          if (attempt < maxAttempts) {
            log('  -> upstream closed the stream with no content, retrying for another route');
            await retrySleep(cfg.deployDelayMs);
            continue;
          }
        }
        // 200 流内可重试错误（生产实测 154/154 都是 rate_limit_exceeded）刻意不做形状修复：
        // 实测把 item id 和 reasoning 都摘干净之后，请求仍然被限流 5~21 次 —— 形状不是触发条件，
        // 再改请求体只是白白损失推理连续性。这里保持纯重试。
        if (attempt < maxAttempts) { await retrySleep(backoffMs(attempt, policy)); continue; }
        return sendRetryable(res, 429, lastReason, attempt, policy);
      }

      // 非流式
      const buf = Buffer.from(await upstream.arrayBuffer());
      let obj = null;
      try { obj = JSON.parse(buf.toString('utf8')); } catch { /* 非 JSON */ }

      const info = errorInfoFromJson(obj);
      const classification = classifyError(info);
      noteCacheRateLimit(classification, attempt);
      if (info) {
        const statusAction = handleStatusError(classification, attempt, 'json_error', upstream.status);
        if (statusAction === 'stop') return;
        if (statusAction === 'retry') { await retrySleep(cfg.deployDelayMs); continue; }
        const itemAction = handleItemError(classification, attempt, 'json_error', upstream.status);
        if (itemAction === 'stop') return;
        if (itemAction === 'retry') { await retrySleep(cfg.deployDelayMs); continue; }
      }
      if (classification.permanent) {
        lastReason = `HTTP 200 but body was an error: ${info.code || info.type || ''} ${info.message}`;
        log(`attempt ${attempt}: ${lastReason}`);
        log(classification.quota ? '  -> quota exhausted, not retrying' : '  -> permanent error, not retrying');
        return sendPermanent(res, lastReason, classification.quota ? 402 : 400);
      }
      if (classification.deployment) {    // 通道缺部署：放宽次数、缩短间隔
        lastReason = `HTTP 200 but body was an error: ${info.code || ''} ${info.message}`;
        log(`attempt ${attempt}: ${lastReason}`);
        maxAttempts = Math.max(maxAttempts, cfg.deployMaxAttempts);
        if (attempt < maxAttempts) {
          log('  -> deployment missing on this route, retrying for another route');
          await retrySleep(cfg.deployDelayMs);
          continue;
        }
        return sendRetryable(res, 429, lastReason, attempt, policy);
      }
      if (classification.retryable) {
        lastReason = `HTTP 200 but body was an error: ${info.code || ''} ${info.message}`;
        log(`attempt ${attempt}: ${lastReason}`);
        if (attempt < maxAttempts) { await retrySleep(backoffMs(attempt, policy)); continue; }
        return sendRetryable(res, 429, lastReason, attempt, policy);
      }

      if (obj && isEmptyCompletion(obj)) {      // 空回应
        lastReason = 'empty response (HTTP 200 but message.content is blank)';
        log(`attempt ${attempt}: ${lastReason}`);
        notePreCommitFailure(attempt, 'json_empty');
        if (attempt < maxAttempts) {
          log('  -> upstream closed the stream with no content, retrying for another route');
          await retrySleep(cfg.deployDelayMs);
          continue;
        }
        return sendRetryable(res, 429, lastReason, attempt, policy);
      }

      if (!info) rememberCacheRoute();
      if (attempt > 1) log(`attempt ${attempt}: recovered on a later route (previous failures: ${attempt - 1})`);
      return passthrough(res, 200, upstream.headers, buf);
    } finally {
      clearTimeout(connectTimer);
      ac.signal.removeEventListener('abort', onParentAbort);
    }
  }
}

function controlResponse(res, status, value) {
  res.writeHead(status, { 'content-type': 'application/json; charset=utf-8', 'cache-control': 'no-store',
    'connection': 'close', 'x-content-type-options': 'nosniff' });
  res.end(JSON.stringify(value));
}

function localControl(req, res, pathname) {
  const address = req.socket.remoteAddress;
  const host = String(req.headers.host || '').toLowerCase();
  const expectedPort = String(cfg.port);
  const allowedHosts = [`127.0.0.1:${expectedPort}`, `localhost:${expectedPort}`, `[::1]:${expectedPort}`];
  if (!['127.0.0.1', '::1', '::ffff:127.0.0.1'].includes(address) || !allowedHosts.includes(host) ||
      req.headers.origin || req.headers['sec-fetch-site'] === 'cross-site') {
    controlResponse(res, 403, { error: 'local_control_only' });
    return;
  }
  if (pathname === '/_relay/status' && req.method === 'GET') {
    controlResponse(res, 200, {
      loadedVersion, startedAt, pid: process.pid, supervisorEnabled, restarting, draining, restartError,
      safeShutdownVersion: 1, drainWaitVersion: 1,
      policy: { revision: policyRevision, settings: policySettings },
      activeRequests: activeRequests.size,
      config: { statusIntervalMs: cfg.statusIntervalMs, streamIdleMs: cfg.streamIdleMs,
        streamProgressMs: cfg.streamProgressMs, peekTotalMs: cfg.peekTotalMs,
        connectTimeoutMs: cfg.connectTimeoutMs, totalBudgetMs: cfg.totalBudgetMs,
        maxAttempts: cfg.maxAttempts, sessionCompat: cfg.sessionCompat },
      requests: [...activeRequests.values()].slice(0, 100).map(requestSnapshot),
      requestsTruncated: activeRequests.size > 100,
    });
    return;
  }
  if (pathname === '/_relay/drain' && req.method === 'POST') {
    if (req.headers['x-relay-control'] !== '1') return controlResponse(res, 403, { error: 'control_header_required' });
    if (req.headers['x-relay-pid'] !== String(process.pid)) return controlResponse(res, 409, { error: 'instance_changed' });
    const operation = req.headers['x-relay-operation'];
    const action = req.headers['x-relay-action'];
    if (typeof operation !== 'string' || !/^[a-f0-9]{32}$/.test(operation)) {
      return controlResponse(res, 400, { error: 'invalid_operation' });
    }
    if (!['start', 'refresh', 'cancel', 'commit'].includes(action)) return controlResponse(res, 400, { error: 'invalid_action' });
    expireDrainWait();
    if (action === 'start') {
      const budgetHeader = req.headers['x-relay-timeout-ms'];
      const leaseHeader = req.headers['x-relay-lease-ms'];
      const budget = Number(budgetHeader);
      const leaseMs = Number(leaseHeader);
      if (!/^[0-9]+$/.test(budgetHeader || '') || !Number.isInteger(budget) || budget < 1 || budget > 600000) {
        return controlResponse(res, 400, { error: 'invalid_timeout' });
      }
      if (!/^[0-9]+$/.test(leaseHeader || '') || !Number.isInteger(leaseMs) || leaseMs < 100 || leaseMs > 10000) {
        return controlResponse(res, 400, { error: 'invalid_lease' });
      }
      if (drainWait) {
        if (drainWait.operation !== operation || drainWait.committing) return controlResponse(res, 409, { error: 'control_busy' });
        renewDrainLease(drainWait);
        return controlResponse(res, 202, { operation, activeRequests: activeRequests.size, draining: true });
      }
      if (finishedDrainOperations.has(operation)) return controlResponse(res, 409, { error: 'unknown_operation' });
      if (draining || restarting) return controlResponse(res, 409, { error: 'control_busy' });
      const attempt = { operation, leaseMs, deadline: performance.now() + budget, leaseDeadline: 0,
        budgetTimer: null, leaseTimer: null, committing: false };
      drainWait = attempt;
      draining = true;
      attempt.budgetTimer = setTimeout(() => clearDrainWait(attempt), budget);
      renewDrainLease(attempt);
      return controlResponse(res, 202, { operation, activeRequests: activeRequests.size, draining: true });
    }
    const attempt = drainWait;
    if (action === 'cancel' && finishedDrainOperations.get(operation) === 'cancelled') {
      return controlResponse(res, 200, { operation, cancelled: true });
    }
    if (!attempt || attempt.operation !== operation) return controlResponse(res, 409, { error: 'unknown_operation' });
    if (attempt.committing) return controlResponse(res, 409, { error: 'control_busy' });
    if (action === 'cancel') {
      clearDrainWait(attempt);
      return controlResponse(res, 200, { operation, cancelled: true });
    }
    if (action === 'commit') {
      if (activeRequests.size) return controlResponse(res, 409, { error: 'active_requests', activeRequests: activeRequests.size });
      attempt.committing = true;
      attempt.commitRes = res;
      // 只有完整确认交给 socket 后才真正停机；确认前断连撤销整个等待。
      res.once('close', () => clearDrainWait(attempt));
      res.once('finish', () => {
        expireDrainWait(attempt);
        if (drainWait !== attempt) return;
        clearDrainWait(attempt, 'committed');
        beginDrain(0);
      });
      if (res.destroyed || req.socket.destroyed) { clearDrainWait(attempt); return; }
      return controlResponse(res, 200, { operation, drained: true, pid: process.pid });
    }
    renewDrainLease(attempt);
    return controlResponse(res, 200, { operation, activeRequests: activeRequests.size, draining: true });
  }
  if (pathname === '/_relay/shutdown' && req.method === 'POST') {
    if (req.headers['x-relay-control'] !== '1') return controlResponse(res, 403, { error: 'control_header_required' });
    if (!supervisorEnabled || !process.connected) return controlResponse(res, 409, { error: 'supervisor_required' });
    if (Number(req.headers['x-relay-pid']) !== process.pid) return controlResponse(res, 409, { error: 'instance_changed' });
    const budget = Number(req.headers['x-relay-timeout-ms']);
    if (!Number.isInteger(budget) || budget < 1 || budget > 60000) return controlResponse(res, 400, { error: 'invalid_timeout' });
    if (draining || restarting) return controlResponse(res, 409, { error: 'control_busy' });
    const attempt = { res, timer: null, committing: false };
    const cancel = () => {
      if (safeShutdown !== attempt) return;
      clearTimeout(attempt.timer);
      safeShutdown = null;
      draining = false;
    };
    safeShutdown = attempt;
    draining = true;
    attempt.timer = setTimeout(() => {
      if (safeShutdown !== attempt) return;
      cancel();
      controlResponse(res, 408, { error: 'drain_timeout' });
    }, budget);
    res.once('close', cancel);
    finishDrain();
    return;
  }
  if (pathname === '/_relay/restart' && req.method === 'POST') {
    if (req.headers['x-relay-control'] !== '1') return controlResponse(res, 403, { error: 'control_header_required' });
    if (!supervisorEnabled || !process.connected) return controlResponse(res, 409, { error: 'supervisor_required' });
    if (draining && !restarting) return controlResponse(res, 409, { error: 'shutting_down' });
    if (!restarting) {
      restarting = true;
      restartError = null;
      process.send({ type: 'restart_request' }, (error) => {
        if (error) { restarting = false; restartError = 'supervisor_unavailable'; }
      });
    }
    return controlResponse(res, 202, { accepted: true, restarting, draining, activeRequests: activeRequests.size });
  }
  controlResponse(res, 404, { error: 'unknown_control_endpoint' });
}

function finishDrain() {
  if (!draining || closing || activeRequests.size || drainWait) return;
  if (safeShutdown) {
    const attempt = safeShutdown;
    if (attempt.committing) return;
    attempt.committing = true;
    // 送出完整确认之前仍允许断连/超时撤销，避免响应写失败后永远卡在排空态。
    attempt.res.once('finish', () => {
      if (safeShutdown !== attempt) return;
      safeShutdown = null;
      clearTimeout(attempt.timer);
      beginDrain(0);
    });
    controlResponse(attempt.res, 200, { drained: true, pid: process.pid });
    return;
  }
  closing = true;
  server.close(() => process.exit(drainExitCode));
  server.closeIdleConnections();
}

function beginDrain(code) {
  clearDrainWait(drainWait, 'shutdown');
  draining = true;
  drainExitCode = code;
  safeDiagnostic({ event: 'gateway_draining', activeRequests: activeRequests.size, restart: code === 75 });
  finishDrain();
}

const server = http.createServer((req, res) => {
  let pathname;
  try { pathname = new URL(req.url, 'http://localhost').pathname; }
  catch { controlResponse(res, 400, { error: 'invalid_path' }); return; }
  // 回调可能晚于真实截止：任何新 HTTP 访问都先校验，不续已到期租约/预算。
  expireDrainWait();
  if (pathname.startsWith('/_relay/')) return localControl(req, res, pathname);
  if (draining) {
    res.setHeader('retry-after', '2');
    return controlResponse(res, 503, { error: { message: 'Gateway is draining for a safe restart; retry shortly.', type: 'service_unavailable' } });
  }
  const state = {
    requestId: randomUUID(), path: ['/v1/responses', '/v1/messages', '/v1/chat/completions'].includes(pathname) ? pathname : 'other',
    started: Date.now(), phase: 'receiving_request', attempt: 0, bytesReceived: 0, eventCount: 0,
    reasoningEvents: 0, textEvents: 0, toolEvents: 0, lastByte: null, lastProgress: null,
  };
  activeRequests.set(state.requestId, state);
  const timer = setInterval(() => {
    const snapshot = requestSnapshot(state);
    safeDiagnostic({ event: 'request_status', ...snapshot });
    log(`status ${state.requestId}: ${state.phase}, elapsed=${Math.round(snapshot.elapsedMs / 1000)}s, reasoning=${state.reasoningEvents}, text=${state.textEvents}, tools=${state.toolEvents}`);
  }, cfg.statusIntervalMs);
  timer.unref();
  let handled = false;
  let responseDone = false;
  const cleanup = () => {
    if (!handled || !responseDone) return;
    clearInterval(timer);
    activeRequests.delete(state.requestId);
    finishDrain();
  };
  res.once('finish', () => { responseDone = true; cleanup(); });
  res.once('close', () => { responseDone = true; cleanup(); });
  handle(req, res, state).catch((e) => {
    if (res.destroyed) return;
    log('request handler error:', e.message);
    if (res.headersSent) { try { res.end(); } catch { /* ignore */ } }
    else sendRetryable(res, 502, e.message);
  }).finally(() => { handled = true; cleanup(); });
});

if (supervisorEnabled) {
  for (const signal of ['SIGINT', 'SIGTERM']) {
    process.on(signal, () => { restarting = false; beginDrain(0); });
  }
  process.on('message', (message) => {
    if (message?.type === 'drain' && restarting) beginDrain(75);
    else if (message?.type === 'shutdown') { restarting = false; beginDrain(0); }
    else if (message?.type === 'restart_rejected') {
      restarting = false;
      restartError = message.reason === 'supervisor_unavailable' ? 'supervisor_unavailable' : 'syntax_check_failed';
    }
  });
  process.on('disconnect', () => {
    restarting = false;
    restartError = 'supervisor_unavailable';
  });
}

server.listen(cfg.port, cfg.host, () => {
  healDiagnosticsRotation();
  safeDiagnostic({ event: 'gateway_started', loadedVersion, startedAt, pid: process.pid, supervisorEnabled });
  if (supervisorEnabled && process.connected) process.send({ type: 'ready' });
  log(`relay-retry-proxy version ${loadedVersion.slice(0, 12)}; supervised=${supervisorEnabled}`);
  log(`relay-retry-proxy started: http://${cfg.host}:${cfg.port}  ->  ${cfg.upstream}`);
  log(`Set this provider's baseURL in ZCode to http://${cfg.host}:${cfg.port}/v1`);
  log(`Retry: max ${cfg.maxAttempts} attempts / base ${cfg.baseDelayMs}ms / max ${cfg.maxDelayMs}ms; ` +
      `deployment-retry up to ${cfg.deployMaxAttempts} at ${cfg.deployDelayMs}ms; ` +
      `first-content wait limit ${Math.round(cfg.peekTotalMs / 1000)}s`);
});
