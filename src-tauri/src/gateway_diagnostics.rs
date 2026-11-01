//! 诊断汉化核心（T4 工单）：壳侧只读消费者的**纯逻辑**层。
//!
//! 分层（沿用 backend-map「纯逻辑在 gateway_*」铁律）：本模块只做
//! 「一条诊断记录(已解析 JSON) → 可选活动事件」的提炼，**不碰文件/进程/网络**。
//! T5 的增量 tail / 分页读取薄层与 `#[tauri::command]` 命令都薄封装本模块的纯函数。
//!
//! 字典以**稳定封闭枚举**为键：`event` 加 `kind`/`source`/`refusal`/`terminalKind`/
//! `stallReason`/`phase`/`status`（HTTP 段）以及 `status_repair_refused.reason`。
//! 请求标识与时间戳原样保留；用户解释把毫秒转换为可读时长，技术信息保留原有码表。
//! 未知事件码或未知子枚举值 → **诚实回退**（显示原始码），绝不隐藏、绝不 panic。
//!
//! 严重度不只看 `event`：`sse_ended` 由 `terminalKind`+`stallReason` 定级、
//! `finished`/`upstream_response` 由 HTTP `status` 段定级、`precommit_failure` 由 `kind` 定级
//! （依据 diagnostics-codes.md §4）。
//!
//! 权威事件码/子枚举清单：ag-relay-notes/diagnostics-codes.md（只读通读打包脚本得出）。
//! 覆盖率契约测试（本文件末尾 mod tests）照 `gateway_env`↔`gateway-config.bat` 范式：
//! 从只读打包脚本 `resources/gateway/relay-retry-proxy.mjs` 抽取当前 24 个 `event` 码，
//! 与当前写出契约严格核对；退役事件和枚举另列历史读取契约，不要求核心继续写出。

use serde_json::Value;

/// 活动事件级别（GLOSSARY：信息 / 警告 / 错误）。
/// serde 输出小写稳定机器码（info/warn/error），前端据此筛选/着色，中文由前端渲染或取 `as_cn`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ActivityLevel {
    /// 信息
    Info,
    /// 警告
    Warn,
    /// 错误
    Error,
}

impl ActivityLevel {
    /// 中文标签（信息 / 警告 / 错误）——供需要直接展示级别文案处取用。
    pub fn as_cn(&self) -> &'static str {
        match self {
            ActivityLevel::Info => "信息",
            ActivityLevel::Warn => "警告",
            ActivityLevel::Error => "错误",
        }
    }
}

/// 活动事件：由一条诊断记录提炼的、面向用户的中文事件（GLOSSARY「活动事件」）。
/// `time` 是诊断记录里的 ISO8601 时间戳**原样透传**（透传槽，不解析不翻译）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityEvent {
    pub attempt: Option<u64>,
    pub http_status: Option<u16>,
    pub summary: String,
    /// 用于关联同一请求，不从正文猜测标识。
    pub request_id: Option<String>,
    pub code: String,
    /// 已汉化的技术细节，默认折叠，不包含原始报错正文。
    pub technical_detail: String,
    pub level: ActivityLevel,
    /// 中文标题（简短，面向「最近活动」列表）
    pub title: String,
    /// 面向用户的含义解释；路径与内部枚举保留在 technical_detail 中。
    pub detail: String,
    /// 原始 ISO8601 时间戳（透传槽；缺失则空串）
    pub time: String,
}

// ───────────────────────── 公共纯函数（T5 包装入口） ─────────────────────────

/// 纯函数核心：一条**已解析**的诊断记录 → 可选活动事件。
///
/// - 无 `event` 字段（或非字符串）⇒ `None`（不是可展示记录）。
/// - `request_profile`（received 的兄弟记录，纯请求体字段画像）⇒ `None`（不面向用户）。
/// - 未知 `event` 码 ⇒ `Some(诚实回退)`：原样显示原始码、级别取信息、不崩。
/// - 其余 ⇒ `Some`：中文标题 + 中文详情 + 据子枚举定级。
pub fn localize_record(record: &Value) -> Option<ActivityEvent> {
    let event = record.get("event").and_then(Value::as_str)?;
    let attempt = record.get("attempt").and_then(Value::as_u64);
    let http_status = record.get("status").and_then(Value::as_u64)
        .filter(|s| (100..=599).contains(s)).map(|s| s as u16);
    let request_id = record.get("requestId").and_then(Value::as_str)
        .filter(|id| !id.is_empty()).map(str::to_string);
    let time = record
        .get("time")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    if SUPPRESSED_EVENTS.contains(&event) {
        return None;
    }

    let Some((base_level, title)) = event_base(event) else {
        // 未知事件码：诚实回退，原样显示原始码（不隐藏、不 panic）。
        return Some(ActivityEvent {
            attempt, http_status, summary: "未知事件".into(),
            request_id,
            code: event.to_string(),
            technical_detail: format!("原始事件码：{event}"),
            level: ActivityLevel::Info,
            title: format!("未识别事件：{event}"),
            detail: "壳未收录此事件码，已原样显示原始码。".to_string(),
            time,
        });
    };

    let (level, mut technical_detail) = refine(event, base_level, record);
    if event == "sse_ended" {
        let terminal_seen = match record.get("sawTerminal").and_then(Value::as_bool) {
            Some(true) => "是", Some(false) => "否", None => "未知",
        };
        technical_detail.push_str(&format!("终止标记已确认：{terminal_seen}。"));
    }
    let (title, detail) = explain(event, title, record, &technical_detail);
    let summary = compact_summary(event, record, &title);
    Some(ActivityEvent { attempt, http_status, summary, level, title, detail, technical_detail, request_id, code: event.to_string(), time })
}

/// 便捷入口：一行诊断记录文本 → 可选活动事件（供 fixture 与 T5 读取层逐行调用）。
/// 非法 JSON 行 ⇒ `None`（跳过，绝不 panic）。
pub fn localize_line(line: &str) -> Option<ActivityEvent> {
    let v: Value = serde_json::from_str(line).ok()?;
    localize_record(&v)
}

/// 用户先看到含义与下一步，原有码表解释留在可展开的技术信息中。
fn explain(event: &str, title: &str, r: &Value, fallback: &str) -> (String, String) {
    let (title, detail) = match event {
        "gateway_started" => (title, "网关已就绪，可以接收客户端请求。".into()),
        "gateway_draining" => ("正在等待请求结束", format!("网关准备{}，会先等待进行中的请求完成。", if r.get("restart").and_then(Value::as_bool) == Some(true) { "重启" } else { "停止" })),
        "received" => (title, "客户端已把请求交给网关，开始处理。".into()),
        "request_status" => {
            let phase = match r.get("phase").and_then(Value::as_str) {
                Some("receiving_request") => "正在接收客户端请求".into(),
                Some("waiting_headers") => "等待上游响应，还未收到回复".into(),
                Some("retry_waiting") => "网关正在等待后再试，不是上游正在生成内容；下一次尝试尚未发出".into(),
                Some("reading_body") => "上游已响应，正在读取回复".into(),
                Some("waiting_first_content") => "等待首段内容，上游已连接但还未返回可交付的内容".into(),
                Some("stream_waiting") => "已开始传回内容，等待后续输出".into(),
                Some("reasoning") => "最近收到推理内容，正文可能尚未开始".into(),
                Some("tool_output") => "最近收到工具调用信息，不代表工具已执行完成".into(),
                Some("text_output") => "最近收到正文，回复仍在进行".into(),
                _ => field_cn(r, "phase").unwrap_or_else(|| "请求仍在处理，具体阶段未知".into()),
            };
            let mut parts = vec![phase];
            if let Some(ms) = num(r, "elapsedMs").filter(|n| *n >= 0) { parts.push(format!("已用时 {}", duration_text(ms))); }
            if let Some(n) = num(r, "attempt").filter(|n| *n >= 0) { parts.push(if n == 0 { "尚未发起尝试".into() } else { format!("第 {n} 次尝试") }); }
            match r.get("sinceLastProgressMs") {
                Some(Value::Null) => parts.push("尚未收到内容进展".into()),
                Some(value) => if let Some(ms) = value.as_i64().filter(|n| *n >= 0) { parts.push(format!("距上次内容进展 {}", duration_text(ms))); },
                None => {},
            }
            let title = match r.get("phase").and_then(Value::as_str) {
                Some("receiving_request") => "接收请求",
                Some("waiting_headers") => "等待上游响应",
                Some("retry_waiting") => "等待重试",
                Some("reading_body") => "读取回复",
                Some("waiting_first_content") => "等待首段内容",
                Some("stream_waiting") => "等待后续内容",
                Some("reasoning") => "正在推理",
                Some("tool_output") => "输出工具调用",
                Some("text_output") => "输出文本",
                _ => "请求阶段未知",
            };
            (title, format!("{}。", parts.join("；")))
        },
        "upstream_response" => ("上游已响应", match num(r, "status") {
            Some(200..=299) => "上游已接受本次连接，仍需等待内容；这还不代表回复完整结束。".into(),
            Some(401 | 403) => "上游拒绝了访问，请检查客户端填写的密钥及使用权限。".into(),
            Some(429) => "上游暂时限制请求频率或额度，请留意后续尝试结果。".into(),
            Some(s @ 500..=599) => format!("上游服务暂时出错（HTTP {s}），请留意后续尝试结果。"),
            _ => fallback.into(),
        }),
        "sse_committed" => ("开始传回内容", "已收到可交付的内容，开始向客户端传回回复。".into()),
        "sse_ended" => {
            let terminal = r.get("terminalKind").and_then(Value::as_str);
            let stall = r.get("stallReason");
            let success = r.get("sawTerminal").and_then(Value::as_bool) == Some(true)
                && stall == Some(&Value::Null)
                && matches!(terminal, Some("done_marker" | "response.completed" | "message_stop" | "chat_finish_reason"));
            if success { ("回复已完整结束", "已收到正常结束标记，本次流式回复完整结束。".into()) }
            else if stall.and_then(Value::as_str) == Some("client_closed") { ("客户端已断开", "客户端在回复完成前断开，本次回复未确认完整结束。".into()) }
            else if terminal == Some("response.failed") { ("上游未能完成回复", "上游明确报告本次生成失败，可查看技术信息定位原因。".into()) }
            else if terminal == Some("response.incomplete") { ("回复未完整结束", "上游报告回复不完整，请在客户端检查结果后决定是否重试。".into()) }
            else { ("回复结束，完整性未确认", "连接已结束，但缺少正常完成的完整证据。请检查客户端收到的内容，技术信息中保留了结束原因。".into()) }
        },
        "finished" => (title, match num(r, "status") {
            Some(200..=299) => "本次连接已结束。HTTP 成功状态不等于内容完整，流式回复请结合结束记录判断。".into(),
            Some(401 | 403) => "本次请求被拒绝，请检查客户端的密钥与权限。".into(),
            Some(429) => "本次请求因上游限流或额度限制未能完成，请稍后再试并检查额度。".into(),
            Some(s) => format!("本次请求结束，返回 HTTP {s}。请结合前面的事件查看原因。"),
            None => "本次连接已结束，记录未提供结果状态。".into(),
        }),
        "client_closed" => ("客户端提前断开", "客户端在回复完成前断开，可能是主动取消、超时或关闭了连接。".into()),
        "precommit_failure" => ("尚未收到可用回复", format!("回复交付前遇到问题：{}。是否继续尝试，请查看后续记录。", field_cn(r, "kind").unwrap_or_else(|| "原因未知".into()))),
        "item_error" if r.get("kind").and_then(Value::as_str) == Some("uncovered_error_fallback") => ("未覆盖错误保底", "原样确认后仍收到未覆盖错误，网关正在判断能否对本次历史做一次安全清理；尚未确认错误由历史引起。".into()),
        "repair_applied" if r.get("kind").and_then(Value::as_str) == Some("uncovered_error_fallback") => ("已执行未覆盖错误保底", "本次转发副本已做一次有损清理，可能损失旧推理连续性；不跨请求记忆，是否恢复仍需查看后续结果。".into()),
        "repair_refused" if r.get("kind").and_then(Value::as_str) == Some("uncovered_error_fallback") => ("未覆盖错误保底已停止", format!("未覆盖错误保底已停止：{}。保留本次错误，不跨请求记忆。", field_cn(r, "refusal").unwrap_or_else(|| "原因未知".into()))),
        "item_error" => ("请求中的历史内容不被上游接受", "上游无法使用部分历史内容，网关正在判断能否兼容处理。".into()),
        "session_compat_refused" | "repair_refused" => ("无法安全自动修复", "继续修改可能损失历史内容，网关没有强行修复。可在客户端检查会话内容。".into()),
        "status_repair_refused" => ("无法自动调整请求格式", "当前请求不符合安全修复条件，未执行这项修改。具体原因见技术信息。".into()),
        "status_repair_applied" | "repair_applied" | "session_compat_applied" => ("已调整请求兼容性", "网关已处理不兼容的请求字段；是否成功仍需查看后续回复结果。".into()),
        "session_compat_enabled" => ("已记录会话兼容方式", "已记录当前会话需要的兼容处理，供后续请求使用。".into()),
        "codex_session_passthrough" => ("保留原请求继续尝试", "为保留会话历史，网关没有删改原请求，继续按原请求尝试。".into()),
        "codex_resource_retry" => ("正在重新尝试", "上游未能接续当前会话，网关保留历史内容再次尝试。".into()),
        "codex_resource_exhausted" => ("本次自动恢复已停止", "已无法继续安全恢复，本次请求未能完成。请检查客户端结果后决定是否重新发起。".into()),
        "request_summary" => {
            let mut parts = Vec::new();
            for (field, label) in [("attempts", "实际尝试"), ("rateErrors", "限流"), ("otherErrors", "其他错误")] {
                if let Some(n) = num(r, field).filter(|n| *n >= 0) {
                    parts.push(format!("{label} {n} 次"));
                }
            }
            parts.push(format!("结果：{}", field_cn(r, "result").unwrap_or_else(|| "未确认".into())));
            let note = if r.get("result").and_then(Value::as_str) == Some("json_returned") {
                "已返回 JSON 响应不等于模型已成功完成。"
            } else {
                "该摘要只记录本次请求实际观察到的结果。"
            };
            ("请求处理摘要", format!("{}。{note}", parts.join("；")))
        }
        // 仅解释旧版日志；当前核心不再写出缓存路由事件。
        "cache_route_summary" => {
            let prepared = r.get("candidatePrepared").and_then(Value::as_bool);
            let after = num(r, "afterAttempts");
            let route = field_cn(r, "initialRoute").unwrap_or_else(|| "未知".into());
            let result = field_cn(r, "result").unwrap_or_else(|| "未确认".into());
            let detail = match (prepared, after) {
                (Some(false), _) => format!("本次请求起始使用{route}，未准备新的缓存键；结果：{result}。"),
                (Some(true), Some(0)) => format!("本次请求起始使用{route}，准备过新的缓存键，但尚未实际发送就结束了；结果：{result}。"),
                (Some(true), Some(n)) if n > 0 => {
                    let rate = num(r, "afterRateErrors").filter(|n| *n >= 0)
                        .map(|n| format!("又发生 {n} 次限流，")).unwrap_or_default();
                    if r.get("result").and_then(Value::as_str) == Some("stream_completed") {
                        format!("本次请求起始使用{route}；之后换用了新的缓存键，{rate}最终完成。")
                    } else {
                        format!("本次请求起始使用{route}；之后换用了新的缓存键，{rate}结果：{result}。")
                    }
                }
                _ => format!("本次请求起始使用{route}，换键过程记录不完整；结果：{result}。"),
            };
            ("缓存路由记录", format!("{detail}该记录只说明本次发生了什么，不能据此认定缓存通道损坏或换键一定有效。"))
        }
        _ => (title, fallback.into()),
    };
    (title.to_string(), detail)
}

/// 列表直接展示短原因；只读取已有封闭枚举和数字，不解析上游正文。
fn compact_summary(event: &str, r: &Value, title: &str) -> String {
    match event {
        "gateway_started" => "就绪，可接收请求".into(),
        "gateway_draining" => num(r, "activeRequests").map(|n| format!("等待 {n} 个请求")).unwrap_or_else(|| "等待请求结束".into()),
        "received" => text(r, "path").unwrap_or_else(|| "请求已接收".into()),
        "attempt_started" => "已发送至上游".into(),
        "request_status" => num(r, "elapsedMs").map(|ms| format!("已用时 {}", duration_text(ms.max(0)))).unwrap_or_else(|| "等待后续进展".into()),
        "sse_committed" => "开始返回内容".into(),
        "sse_ended" if title == "回复已完整结束" => "已收到正常结束标记".into(),
        "sse_ended" => {
            match r.get("stallReason").and_then(Value::as_str) {
                Some("client_closed") => "客户端断开".into(),
                Some("progress_timeout") => "内容进展超时".into(),
                Some("transport_idle_timeout") => "连接静默超时".into(),
                Some("upstream_read_error") => "上游读取失败".into(),
                Some("event_size_limit") => "内容片段过大".into(),
                _ => match r.get("terminalKind").and_then(Value::as_str) {
                    Some("response.failed") => "上游生成失败".into(),
                    Some("response.incomplete") => "上游报告不完整".into(),
                    _ => "完整性未确认".into(),
                },
            }
        },
        "finished" | "upstream_response" => match num(r, "status") {
            Some(200..=299) => if event == "finished" { "连接结束，完整性待确认" } else { "已响应，等待内容" }.into(),
            Some(401 | 403) => "密钥或权限被拒".into(),
            Some(402) => "额度不足".into(),
            Some(429) => "限流或额度限制".into(),
            Some(408) => "上游请求超时".into(),
            Some(500..=599) => "上游服务错误".into(),
            _ => "查看详情".into(),
        },
        "client_closed" => "取消、超时或连接断开".into(),
        "precommit_failure" => match r.get("kind").and_then(Value::as_str) {
            Some("peek_timeout") => "首段等待超时".into(),
            Some("empty_stream_drop") => "上游未返回内容".into(),
            Some("read_timeout") => "读取超时".into(),
            Some("buffer_limit") => "首段内容过大".into(),
            Some("stream_error") => "上游返回错误".into(),
            _ => "交付前失败".into(),
        },
        "repair_refused" | "codex_resource_exhausted" => field_cn(r, "refusal").unwrap_or_else(|| "无法安全恢复".into()),
        "status_repair_refused" => field_cn(r, "reason").unwrap_or_else(|| "无法安全修复".into()),
        "item_error" => field_cn(r, "kind").unwrap_or_else(|| "历史内容不兼容".into()),
        "codex_resource_retry" | "codex_session_passthrough" => "保留历史继续尝试".into(),
        "session_compat_enabled" => "已记录兼容方式".into(),
        "session_compat_refused" => "历史内容无法安全改写".into(),
        "repair_applied" | "status_repair_applied" | "session_compat_applied" => "已调整不兼容字段".into(),
        "request_summary" => field_cn(r, "result").unwrap_or_else(|| "结果未确认".into()),
        "cache_route_summary" => {
            match (r.get("candidatePrepared").and_then(Value::as_bool), num(r, "afterAttempts")) {
                (Some(false), _) => "未换键".into(),
                (Some(true), Some(0)) => "已准备，未尝试".into(),
                (Some(true), Some(n)) if n > 0 => match r.get("result").and_then(Value::as_str) {
                    Some("stream_completed") => "换键后完成".into(),
                    Some("json_returned") => "换键后已返回 JSON".into(),
                    Some("cancelled") => "换键后取消".into(),
                    _ => "换键后未完成".into(),
                },
                _ => "换键记录不完整".into(),
            }
        }
        _ => "查看详情".into(),
    }
}

fn duration_text(ms: i64) -> String {
    let seconds = ms / 1000;
    if seconds == 0 { "不足1秒".into() }
    else if seconds < 60 { format!("{seconds}秒") }
    else if seconds < 3600 { format!("{}分{}秒", seconds / 60, seconds % 60) }
    else { format!("{}小时{}分", seconds / 3600, seconds % 3600 / 60) }
}

// ───────────────────────── 字典数据（稳定封闭枚举为键） ─────────────────────────

/// 不面向用户、localize 返回 None 的事件码（仍在 EVENT_BASE 中登记，保证契约覆盖）。
/// `request_profile`：与 `received` 同 requestId 的请求体字段画像（diagnostics-codes.md §1-B#5），
/// 属内部诊断、不进「最近活动」。
const SUPPRESSED_EVENTS: &[&str] = &["request_profile"];

/// 当前核心写出事件基表：`event` → (默认级别, 中文标题)，共 24 项。
/// 默认级别对 severity-依赖子字段的事件（sse_ended/finished/upstream_response/precommit_failure）
/// 只是兜底，真正级别在 refine 里据子枚举覆盖。
const EVENT_BASE: &[(&str, ActivityLevel, &str)] = &[
    // A. 生命周期 / 状态（3）
    ("gateway_started", ActivityLevel::Info, "网关已启动"),
    ("gateway_draining", ActivityLevel::Info, "网关开始排空"),
    ("request_status", ActivityLevel::Info, "请求进行中"),
    // B. 单请求正常流程（8）
    ("received", ActivityLevel::Info, "收到请求"),
    ("request_profile", ActivityLevel::Info, "请求体画像"),
    ("attempt_started", ActivityLevel::Info, "发起上游尝试"),
    ("upstream_response", ActivityLevel::Info, "收到上游响应头"),
    ("sse_committed", ActivityLevel::Info, "开始流式下发"),
    ("sse_ended", ActivityLevel::Info, "流式响应结束"),
    ("finished", ActivityLevel::Info, "请求结束"),
    ("client_closed", ActivityLevel::Warn, "客户端提前断开"),
    // C. 会话兼容（4）
    ("session_compat_refused", ActivityLevel::Warn, "会话兼容修复被拒"),
    ("codex_session_passthrough", ActivityLevel::Info, "会话原样透传重试"),
    ("session_compat_applied", ActivityLevel::Info, "已应用会话兼容修复"),
    ("session_compat_enabled", ActivityLevel::Info, "已登记会话兼容"),
    // D. 请求体修复 / 上游错误处理（8）
    ("status_repair_refused", ActivityLevel::Warn, "status 字段修复被拒"),
    ("status_repair_applied", ActivityLevel::Info, "已删除 status 字段并重试"),
    ("item_error", ActivityLevel::Warn, "识别到 item 级错误"),
    ("repair_refused", ActivityLevel::Warn, "请求体修复被拒"),
    ("repair_applied", ActivityLevel::Info, "已修复请求体并重试"),
    ("codex_resource_retry", ActivityLevel::Info, "保留历史换通道重试"),
    ("codex_resource_exhausted", ActivityLevel::Error, "资源重试耗尽，放弃"),
    ("precommit_failure", ActivityLevel::Warn, "流式下发前失败"),
    // E. 通用请求观察（1）
    ("request_summary", ActivityLevel::Info, "请求处理摘要"),
];

/// 退役事件的只读兼容表，不属于当前核心写出契约。
const LEGACY_EVENT_BASE: &[(&str, ActivityLevel, &str)] = &[
    ("cache_route_summary", ActivityLevel::Info, "缓存路由记录"),
];

/// 当前子枚举中文标签表：(字段名, 枚举值) → 中文；历史专用项另列。
/// 字段名区分同名异义值（如 refusal 与 reason 都有 wrong_endpoint/repair_exhausted/time_budget）。
/// current/legacy 各自按独立清单逐项核对，不要求退役枚举留在核心源码。
const SUB_LABELS: &[(&str, &str, &str)] = &[
    // source（6）—— 错误识别层 + 会话兼容来源（§2.A）
    ("source", "http_error", "HTTP 错误响应"),
    ("source", "sse_precommit", "流式下发前"),
    ("source", "json_error", "JSON 错误信封"),
    ("source", "json_empty", "空回应（HTTP 200）"),
    ("source", "explicit_session", "显式会话"),
    ("source", "resource_lineage", "资源血缘"),
    // kind（13）—— 历史错误与保底 5 + 下发前失败 6 + 空回应形状 2（§2.B）
    ("kind", "azure_resource_mismatch", "跨 Azure 资源引用"),
    ("kind", "stale_reasoning_item", "失效的 reasoning 项"),
    ("kind", "session_binding_invalidated", "会话绑定失效（仅清历史编号）"),
    ("kind", "account_reasoning_unavailable", "账号绑定的旧推理不可用"),
    ("kind", "uncovered_error_fallback", "未覆盖错误保底"),
    ("kind", "other", "其他"),
    ("kind", "peek_timeout", "等待首个内容超时"),
    ("kind", "empty_stream_drop", "上游静默丢弃（空流）"),
    ("kind", "read_timeout", "空闲读超时"),
    ("kind", "buffer_limit", "下发前缓冲超限"),
    ("kind", "stream_error", "流内携带错误"),
    ("kind", "empty_stream_shape", "空回应形状修复"),
    ("kind", "empty_stream_reasoning", "空回应删除 reasoning 项"),
    // refusal（5）—— 修复被拒 / 资源耗尽原因（§2.C）
    ("refusal", "unsafe_inline_history", "内联历史不安全"),
    ("refusal", "wrong_endpoint", "非 /v1/responses 端点"),
    ("refusal", "repair_exhausted", "单次修复额度已用尽"),
    ("refusal", "time_budget", "剩余预算不足"),
    ("refusal", "resource_attempt_limit", "资源尝试达上限"),
    // terminalKind（7，含 null）—— 流终止类型（§2.E）
    ("terminalKind", "null", "未见终止事件（截断）"),
    ("terminalKind", "done_marker", "[DONE] 标记"),
    ("terminalKind", "response.completed", "正常完成"),
    ("terminalKind", "response.failed", "失败终止"),
    ("terminalKind", "response.incomplete", "不完整终止"),
    ("terminalKind", "message_stop", "Anthropic 风格终止"),
    ("terminalKind", "chat_finish_reason", "Chat finish_reason 终止"),
    // stallReason（6，含 null）—— 下发后停滞原因（§2.F）
    ("stallReason", "null", "无停滞"),
    ("stallReason", "progress_timeout", "无生成进展超时"),
    ("stallReason", "client_closed", "客户端断开"),
    ("stallReason", "transport_idle_timeout", "传输层空闲超时"),
    ("stallReason", "upstream_read_error", "上游读取错误"),
    ("stallReason", "event_size_limit", "单事件超缓冲上限"),
    // phase（9）—— request_status 请求状态机（§2.G）
    ("phase", "receiving_request", "正在接收请求"),
    ("phase", "waiting_headers", "等待上游响应头"),
    ("phase", "retry_waiting", "等待重试"),
    ("phase", "reading_body", "读取上游响应体"),
    ("phase", "waiting_first_content", "等待首个内容"),
    ("phase", "stream_waiting", "已下发，等待后续"),
    ("phase", "reasoning", "正在输出推理"),
    ("phase", "tool_output", "正在输出工具调用"),
    ("phase", "text_output", "正在输出正文"),
    // reason（6）—— status_repair_refused 专属（§2.H，spec 7 字段列表外的额外封闭枚举）
    ("reason", "wrong_endpoint", "非 /v1/responses 端点"),
    ("reason", "repair_exhausted", "status 修复已做过一次"),
    ("reason", "unexpected_status", "非 400/200 状态"),
    ("reason", "invalid_json", "请求体非 JSON"),
    ("reason", "unsupported_item", "目标项不符合可删 status 的形状"),
    ("reason", "time_budget", "剩余预算不足"),
    // status_band（3）—— HTTP status 段，供 finished/upstream_response 定级与详情（§4）
    ("status_band", "2xx", "成功"),
    ("status_band", "4xx", "请求被拒"),
    ("status_band", "5xx", "上游故障"),
    // result（8）—— 本次请求实际结果，不等于故障归因
    ("result", "unknown", "未确认"),
    ("result", "json_returned", "已返回 JSON 响应"),
    ("result", "stream_completed", "流式完整结束"),
    ("result", "stream_failed", "流式未完整结束"),
    ("result", "retry_exhausted", "重试预算或次数用尽"),
    ("result", "permanent_error", "确定性错误"),
    ("result", "error_passthrough", "错误原样返回"),
    ("result", "cancelled", "客户端取消"),
    // rateClass（6）—— 仅记录观察到的错误特征
    ("rateClass", "none", "未出现限流"),
    ("rateClass", "token_rate", "token 频率限流"),
    ("rateClass", "request_rate", "请求频率限流"),
    ("rateClass", "capacity", "容量压力"),
    ("rateClass", "mixed", "混合限流特征"),
    ("rateClass", "unknown", "未细分限流"),
    // client（6）—— 有界客户端分类；unknown 是观察尚未建立时的初始值
    ("client", "unknown", "客户端未知"),
    ("client", "none", "未识别客户端"),
    ("client", "zcode", "ZCode"),
    ("client", "claude", "Claude"),
    ("client", "codex", "Codex"),
    ("client", "other", "其他客户端"),
    // policyGroup（3）—— 实际采用的策略组
    ("policyGroup", "unknown", "策略组未知"),
    ("policyGroup", "codex", "Codex 策略组"),
    ("policyGroup", "general", "通用策略组"),
    // usageShape（3）—— 已有解析得到的用量形状，不推算未提供的 token 数
    ("usageShape", "missing", "未提供用量"),
    ("usageShape", "responses", "Responses 用量"),
    ("usageShape", "chat", "Chat 用量"),
];

/// 旧缓存路由摘要的专用枚举，只服务历史日志读取。
const LEGACY_SUB_LABELS: &[(&str, &str, &str)] = &[
    ("initialRoute", "unknown", "未知"),
    ("initialRoute", "ineligible", "不适用"),
    ("initialRoute", "original", "客户端原键"),
    ("initialRoute", "remembered", "已记录的成功键"),
];

// ───────────────────────── 查表与透传辅助 ─────────────────────────

/// 事件基表查表。
fn event_base(event: &str) -> Option<(ActivityLevel, &'static str)> {
    EVENT_BASE
        .iter()
        .chain(LEGACY_EVENT_BASE.iter())
        .find(|(e, _, _)| *e == event)
        .map(|(_, lvl, title)| (*lvl, *title))
}

/// 子枚举中文标签查表。
fn sub_label(field: &str, value: &str) -> Option<&'static str> {
    SUB_LABELS
        .iter()
        .chain(LEGACY_SUB_LABELS.iter())
        .find(|(f, v, _)| *f == field && *v == value)
        .map(|(_, _, cn)| *cn)
}

/// 子枚举值 → 中文；未知值诚实回退为「原始码：X」（不隐藏、不 panic）。
fn sub_cn(field: &str, value: &str) -> String {
    match sub_label(field, value) {
        Some(cn) => cn.to_string(),
        None => format!("原始码：{value}"),
    }
}

/// 从记录取某封闭枚举字段的「值 token」：字符串原样；JSON null ⇒ "null"；
/// 缺失/其它类型 ⇒ None（调用方按「未携带该判别」处理）。
fn field_token(record: &Value, field: &str) -> Option<String> {
    match record.get(field) {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Null) => Some("null".to_string()),
        _ => None,
    }
}

/// 把子枚举字段渲染成中文（未知值诚实回退）；字段缺失 ⇒ None。
fn field_cn(record: &Value, field: &str) -> Option<String> {
    field_token(record, field).map(|v| sub_cn(field, &v))
}

/// HTTP status 段 → (段 token, 该段默认级别)。段外（非 2xx/4xx/5xx）⇒ None。
fn http_band(status: i64) -> Option<(&'static str, ActivityLevel)> {
    match status {
        200..=299 => Some(("2xx", ActivityLevel::Info)),
        400..=499 => Some(("4xx", ActivityLevel::Warn)),
        500..=599 => Some(("5xx", ActivityLevel::Error)),
        _ => None,
    }
}

/// 透传槽取整数（status/attempt/activeRequests 等）。
fn num(record: &Value, field: &str) -> Option<i64> {
    record.get(field).and_then(Value::as_i64)
}

/// 透传槽取字符串（path 等半枚举位，原样搬运不翻译）。
fn text(record: &Value, field: &str) -> Option<String> {
    record.get(field).and_then(Value::as_str).map(str::to_string)
}

// ───────────────────────── 定级 + 详情提炼 ─────────────────────────

/// 据 `event` + 子枚举 refine 出最终级别与中文详情。
/// severity-依赖事件（sse_ended/finished/precommit_failure）在此覆盖 base_level（§4）。
fn refine(event: &str, base_level: ActivityLevel, record: &Value) -> (ActivityLevel, String) {
    match event {
        "gateway_started" => {
            let detail = match num(record, "pid") {
                Some(pid) => format!("网关监听就绪，进程 {pid}。"),
                None => "网关监听就绪。".to_string(),
            };
            (base_level, detail)
        }
        "gateway_draining" => {
            let restart = record.get("restart").and_then(Value::as_bool).unwrap_or(false);
            let active = num(record, "activeRequests").unwrap_or(0);
            let kind = if restart { "热重启排空" } else { "关停排空" };
            (base_level, format!("开始{kind}，等待 {active} 个在途请求完成。"))
        }
        "request_status" => {
            let detail = match field_cn(record, "phase") {
                Some(p) => format!("{p}。"),
                None => "在途请求状态快照。".to_string(),
            };
            (base_level, detail)
        }
        "received" => {
            let detail = match text(record, "path") {
                Some(p) => format!("收到客户端请求：{p}。"),
                None => "收到客户端请求。".to_string(),
            };
            (base_level, detail)
        }
        "attempt_started" => {
            let detail = match num(record, "attempt") {
                Some(n) => format!("第 {n} 次向上游发起尝试。"),
                None => "向上游发起尝试。".to_string(),
            };
            (base_level, detail)
        }
        "upstream_response" => {
            // 级别恒信息：是否成功由后续 finished / sse_ended 定（§1-B#7）；此处只在详情报 HTTP 段。
            let detail = match num(record, "status") {
                Some(s) => {
                    let band = http_band(s)
                        .map(|(b, _)| sub_cn("status_band", b))
                        .unwrap_or_else(|| "其它".to_string());
                    format!("收到上游响应头：HTTP {s}（{band}）。")
                }
                None => "收到上游响应头。".to_string(),
            };
            (base_level, detail)
        }
        "sse_committed" => (base_level, "已确认有真实内容，开始向客户端下发。".to_string()),
        "sse_ended" => {
            let tk = field_token(record, "terminalKind");
            let sr = field_token(record, "stallReason");
            let level = sse_ended_level(tk.as_deref(), sr.as_deref());
            let tk_cn = field_cn(record, "terminalKind").unwrap_or_else(|| "未知".to_string());
            let sr_cn = field_cn(record, "stallReason").unwrap_or_else(|| "未知".to_string());
            (level, format!("流式响应结束：终止类型 {tk_cn}，停滞 {sr_cn}。"))
        }
        "finished" => match num(record, "status") {
            Some(s) => match http_band(s) {
                Some((band, lvl)) => {
                    (lvl, format!("请求结束：HTTP {s}（{}）。", sub_cn("status_band", band)))
                }
                None => (base_level, format!("请求结束：HTTP {s}。")),
            },
            None => (base_level, "请求结束。".to_string()),
        },
        "client_closed" => (base_level, "客户端在响应完成前断开连接。".to_string()),
        "session_compat_refused" => {
            (base_level, "命中已知会话但内联历史不安全，无法无损剥离。".to_string())
        }
        "codex_session_passthrough" => {
            (base_level, "命中已知会话，codex 资源重试放行、原样透传。".to_string())
        }
        "session_compat_applied" => {
            let src = field_cn(record, "source").unwrap_or_else(|| "未知来源".to_string());
            (base_level, format!("已剥离会话 id / reasoning 项（来源：{src}）。"))
        }
        "session_compat_enabled" => {
            let src = field_cn(record, "source").unwrap_or_else(|| "未知来源".to_string());
            (base_level, format!("azure 修复时顺带登记会话兼容（来源：{src}）。"))
        }
        "status_repair_refused" => {
            let reason = field_cn(record, "reason").unwrap_or_else(|| "未知原因".to_string());
            (base_level, format!("拒绝删除 input.status 修复：{reason}。"))
        }
        "status_repair_applied" => {
            (base_level, "已删除被拒的 status 字段并重试。".to_string())
        }
        "item_error" => {
            let kind = field_cn(record, "kind").unwrap_or_else(|| "未知类别".to_string());
            let src = field_cn(record, "source").unwrap_or_else(|| "未知来源".to_string());
            let st = num(record, "status").map(|s| format!("，HTTP {s}")).unwrap_or_default();
            (base_level, format!("识别到 item 级错误：{kind}（来源：{src}{st}）。"))
        }
        "repair_refused" => {
            let kind = field_cn(record, "kind").unwrap_or_else(|| "未知类别".to_string());
            let refusal = field_cn(record, "refusal").unwrap_or_else(|| "未知原因".to_string());
            (base_level, format!("拒绝 item / 形状修复：{kind}；原因：{refusal}。"))
        }
        "repair_applied" => {
            let kind = field_cn(record, "kind").unwrap_or_else(|| "修复".to_string());
            (base_level, format!("已做 {kind} 并重试。"))
        }
        "codex_resource_retry" => {
            let kind = field_cn(record, "kind").unwrap_or_else(|| "资源错误".to_string());
            (base_level, format!("{kind}：保留历史、换通道重试。"))
        }
        "codex_resource_exhausted" => {
            let kind = field_cn(record, "kind").unwrap_or_else(|| "资源错误".to_string());
            let refusal = field_cn(record, "refusal").unwrap_or_else(|| "未知原因".to_string());
            (base_level, format!("{kind}：资源重试耗尽（{refusal}），放弃。"))
        }
        "precommit_failure" => {
            let level = match field_token(record, "kind").as_deref() {
                Some("stream_error") | Some("buffer_limit") => ActivityLevel::Error,
                Some("peek_timeout") | Some("empty_stream_drop") | Some("read_timeout")
                | Some("other") => ActivityLevel::Warn,
                _ => ActivityLevel::Warn, // 未知 kind：安全兜底警告
            };
            let kind_cn = field_cn(record, "kind").unwrap_or_else(|| "未知类别".to_string());
            (level, format!("流式下发前失败：{kind_cn}。"))
        }
        "request_summary" => {
            let mut parts = Vec::new();
            for (field, label) in [
                ("observationVersion", "观察版本"), ("inputBytes", "输入字节"),
                ("inputItems", "输入条目"), ("attempts", "实际尝试次数"),
                ("rateErrors", "限流次数"), ("otherErrors", "其他错误次数"),
                ("waitHintSeen", "等待提示检查次数"), ("waitHintValid", "有效等待提示次数"),
                ("waitHintInvalid", "无效等待提示次数"),
                ("usageInputTokens", "输入 token"), ("usageOutputTokens", "输出 token"),
                ("usageCachedTokens", "已缓存 token"), ("usageCacheReadTokens", "缓存读取 token"),
                ("usageCacheCreationTokens", "缓存创建 token"),
            ] {
                if let Some(n) = num(record, field).filter(|n| *n >= 0) {
                    parts.push(format!("{label}：{n}"));
                }
            }
            for (field, label) in [("client", "客户端"), ("policyGroup", "策略组"), ("usageShape", "用量形状")] {
                if let Some(value) = field_cn(record, field) { parts.push(format!("{label}：{value}")); }
            }
            if let Some(hash) = text(record, "modelHash") { parts.push(format!("模型哈希：{hash}")); }
            for (field, label) in [("rateFirstClass", "首次限流类别"), ("rateLastClass", "末次限流类别")] {
                if let Some(value) = field_token(record, field) {
                    parts.push(format!("{label}：{}", sub_cn("rateClass", &value)));
                }
            }
            if let Some(mixed) = record.get("rateClassMixed").and_then(Value::as_bool) {
                parts.push(format!("限流类别是否变化：{}", if mixed { "是" } else { "否" }));
            }
            if let Some(ms) = num(record, "waitHintMaxMs").filter(|n| *n >= 0) {
                parts.push(format!("最大等待提示：{ms} ms（仅观察，不代表实际等待时长）"));
            }
            parts.push(format!("结果：{}", field_cn(record, "result").unwrap_or_else(|| "未确认".into())));
            if record.get("terminalKind").is_some_and(|v| !v.is_null()) {
                if let Some(value) = field_cn(record, "terminalKind") { parts.push(format!("终止类型：{value}")); }
            }
            for (field, label) in [("firstContentMs", "首次内容"), ("elapsedMs", "总耗时")] {
                if let Some(ms) = num(record, field).filter(|n| *n >= 0) {
                    parts.push(format!("{label} {}", duration_text(ms)));
                }
            }
            (base_level, parts.join("；") + "。")
        }
        "cache_route_summary" => {
            let prepared = record.get("candidatePrepared").and_then(Value::as_bool);
            let mut parts = vec![
                format!("起始选键：{}", field_cn(record, "initialRoute").unwrap_or_else(|| "未知".into())),
            ];
            if let Some(prepared) = prepared {
                parts.push(format!("是否准备换键：{}", if prepared { "是" } else { "否" }));
            }
            for (prefix, title) in [("before", "前段"), ("after", "换键后")] {
                if prefix == "after" && !(prepared == Some(true) && num(record, "afterAttempts").is_some_and(|n| n > 0)) { continue; }
                if let Some(n) = num(record, &format!("{prefix}Attempts")).filter(|n| *n >= 0) {
                    parts.push(format!("{title}尝试 {n} 次"));
                }
                for (suffix, label) in [("RateErrors", "限流"), ("OtherErrors", "其他错误")] {
                    if let Some(n) = num(record, &format!("{prefix}{suffix}")).filter(|n| *n >= 0) {
                        parts.push(format!("{title}{label} {n} 次"));
                    }
                }
            }
            parts.push(format!("结果：{}", field_cn(record, "result").unwrap_or_else(|| "未确认".into())));
            if let Some(ms) = num(record, "firstContentMs").filter(|n| *n >= 0) {
                parts.push(format!("首次内容 {}", duration_text(ms)));
            }
            if let Some(ms) = num(record, "elapsedMs").filter(|n| *n >= 0) {
                parts.push(format!("总耗时 {}", duration_text(ms)));
            }
            (base_level, parts.join("；") + "。")
        }
        _ => (base_level, String::new()),
    }
}

/// `sse_ended` 定级：先看下发后停滞（stallReason），无停滞再看终止类型（terminalKind）。
/// 依据 diagnostics-codes.md §4；未知值安全兜底（不崩、不误报为严重）。
fn sse_ended_level(terminal_kind: Option<&str>, stall_reason: Option<&str>) -> ActivityLevel {
    if let Some(sr) = stall_reason {
        match sr {
            "null" => {} // 无停滞 → 落到 terminalKind 判定
            "client_closed" => return ActivityLevel::Info, // 客户端主动断开
            "upstream_read_error" => return ActivityLevel::Error,
            "progress_timeout" | "transport_idle_timeout" | "event_size_limit" => {
                return ActivityLevel::Warn
            }
            _ => return ActivityLevel::Warn, // 未知停滞：安全兜底警告
        }
    }
    match terminal_kind {
        Some("response.completed") | Some("done_marker") | Some("message_stop")
        | Some("chat_finish_reason") => ActivityLevel::Info,
        Some("response.incomplete") => ActivityLevel::Warn,
        Some("response.failed") => ActivityLevel::Error,
        Some("null") | None => ActivityLevel::Warn, // 未见终止事件 = 流被截断
        Some(_) => ActivityLevel::Info, // 未知终止类型：有终止即视为信息（诚实回退）
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};

    // ────────── 独立契约清单：当前写出与历史可读分别核对 ──────────
    // 当前清单与打包核心抽取结果交叉核对；历史清单只约束读取兼容，不要求核心保留旧字面量。

    const EXPECTED_ACTIVE_EVENTS: [&str; 24] = [
        "gateway_started", "gateway_draining", "request_status", "received", "request_profile",
        "attempt_started", "upstream_response", "sse_committed", "sse_ended", "finished",
        "client_closed", "session_compat_refused", "codex_session_passthrough",
        "session_compat_applied", "session_compat_enabled", "status_repair_refused",
        "status_repair_applied", "item_error", "repair_refused", "repair_applied",
        "codex_resource_retry", "codex_resource_exhausted", "precommit_failure",
        "request_summary",
    ];
    const EXPECTED_LEGACY_EVENTS: [&str; 1] = ["cache_route_summary"];
    const EXPECTED_SOURCE: [&str; 6] = [
        "http_error", "sse_precommit", "json_error", "json_empty",
        "explicit_session", "resource_lineage",
    ];
    const EXPECTED_KIND: [&str; 13] = [
        "azure_resource_mismatch", "stale_reasoning_item", "session_binding_invalidated",
        "account_reasoning_unavailable", "uncovered_error_fallback", "other", "peek_timeout",
        "empty_stream_drop", "read_timeout", "buffer_limit", "stream_error",
        "empty_stream_shape", "empty_stream_reasoning",
    ];
    const EXPECTED_REFUSAL: [&str; 5] = [
        "unsafe_inline_history", "wrong_endpoint", "repair_exhausted",
        "time_budget", "resource_attempt_limit",
    ];
    // 含 "null"（JS null，非字面量——脚本字面量核对时跳过）
    const EXPECTED_TERMINAL_KIND: [&str; 7] = [
        "null", "done_marker", "response.completed", "response.failed",
        "response.incomplete", "message_stop", "chat_finish_reason",
    ];
    const EXPECTED_STALL_REASON: [&str; 6] = [
        "null", "progress_timeout", "client_closed", "transport_idle_timeout",
        "upstream_read_error", "event_size_limit",
    ];
    const EXPECTED_PHASE: [&str; 9] = [
        "receiving_request", "waiting_headers", "retry_waiting", "reading_body", "waiting_first_content",
        "stream_waiting", "reasoning", "tool_output", "text_output",
    ];
    const EXPECTED_STATUS_REPAIR_REASON: [&str; 6] = [
        "wrong_endpoint", "repair_exhausted", "unexpected_status",
        "invalid_json", "unsupported_item", "time_budget",
    ];
    const EXPECTED_STATUS_BAND: [&str; 3] = ["2xx", "4xx", "5xx"];
    // 只有起始选键属于退役协议，其余通用观察枚举由 request_summary 继续使用。
    const EXPECTED_LEGACY_INITIAL_ROUTE: [&str; 4] = ["unknown", "ineligible", "original", "remembered"];
    const EXPECTED_RESULT: [&str; 8] = [
        "unknown", "json_returned", "stream_completed", "stream_failed",
        "retry_exhausted", "permanent_error", "error_passthrough", "cancelled",
    ];
    const EXPECTED_RATE_CLASS: [&str; 6] = [
        "none", "token_rate", "request_rate", "capacity", "mixed", "unknown",
    ];
    const EXPECTED_CLIENT: [&str; 6] = ["unknown", "none", "zcode", "claude", "codex", "other"];
    const EXPECTED_POLICY_GROUP: [&str; 3] = ["unknown", "codex", "general"];
    const EXPECTED_USAGE_SHAPE: [&str; 3] = ["missing", "responses", "chat"];
    const EXPECTED_ACTIVE_SUB_ENUMS: &[(&str, &[&str])] = &[
        ("source", &EXPECTED_SOURCE), ("kind", &EXPECTED_KIND),
        ("refusal", &EXPECTED_REFUSAL), ("terminalKind", &EXPECTED_TERMINAL_KIND),
        ("stallReason", &EXPECTED_STALL_REASON), ("phase", &EXPECTED_PHASE),
        ("reason", &EXPECTED_STATUS_REPAIR_REASON), ("status_band", &EXPECTED_STATUS_BAND),
        ("result", &EXPECTED_RESULT), ("rateClass", &EXPECTED_RATE_CLASS),
        ("client", &EXPECTED_CLIENT), ("policyGroup", &EXPECTED_POLICY_GROUP),
        ("usageShape", &EXPECTED_USAGE_SHAPE),
    ];
    const EXPECTED_LEGACY_SUB_ENUMS: &[(&str, &[&str])] = &[
        ("initialRoute", &EXPECTED_LEGACY_INITIAL_ROUTE),
    ];

    // ────────── 打包脚本只读抽取（照 gateway_env↔bat：从真值源抽取再比对） ──────────

    /// 唯一诊断来源脚本（禁改，.gitattributes -text 锁字节）。
    fn mjs_path() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("resources").join("gateway").join("relay-retry-proxy.mjs")
    }

    /// 抽取 marker 之后到下一个单引号之间的字面量（逐个不重叠）。
    fn scan_quoted_after(text: &str, marker: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut from = 0;
        while let Some(rel) = text[from..].find(marker) {
            let start = from + rel + marker.len();
            match text[start..].find('\'') {
                Some(q) => {
                    out.push(text[start..start + q].to_string());
                    from = start + q + 1;
                }
                None => break,
            }
        }
        out
    }

    /// 从脚本抽取全部 event 码：`diagnose('X'` 的首参 + 3 处直发 `event: 'X'`。
    /// （diagnostics-codes.md §1：所有 event 均字符串字面量，无动态拼接 → 清单封闭完整。）
    fn extract_events(text: &str) -> BTreeSet<String> {
        let mut set = BTreeSet::new();
        for e in scan_quoted_after(text, "diagnose('") {
            set.insert(e);
        }
        for e in scan_quoted_after(text, "event: '") {
            set.insert(e);
        }
        set
    }

    /// 旧重试边界 fixture 的事件与关联信息仍需保持可读。
    #[test]
    fn 重试边界诊断样本_沿用事件码并保留关联信息() {
        let samples = include_str!("../tests/fixtures/diagnostics-retry-boundaries.jsonl");
        let events: Vec<_> = samples.lines().map(|line| localize_line(line).unwrap()).collect();
        assert_eq!(events.len(), 5);
        assert_eq!(events[0].code, "precommit_failure");
        assert_eq!(events[0].attempt, Some(1));
        assert_eq!(events[1].http_status, Some(400));
        assert_eq!(events[0].request_id, events[1].request_id);
        assert_eq!(events[2].title, "尚未收到可用回复");
        assert_eq!(events[3].code, "codex_resource_retry");
        assert_eq!(events[3].level, ActivityLevel::Info);
        assert_eq!(events[3].summary, "保留历史继续尝试");
        assert!(events[3].technical_detail.contains("失效的 reasoning 项"));
        assert_eq!(events[4].code, "codex_resource_exhausted");
        assert_eq!(events[4].level, ActivityLevel::Error);
        assert_eq!(events[4].attempt, Some(5));
        assert_eq!(events[4].summary, "资源尝试达上限");
    }

    #[test]
    fn 紧凑活动列只复用既有字段_缺失值不补零() {
        let event = localize_record(&json!({"event":"precommit_failure","requestId":"r","attempt":3,"status":200,"kind":"peek_timeout"})).unwrap();
        let value = serde_json::to_value(event).unwrap();
        assert_eq!(value["attempt"], 3);
        assert_eq!(value["httpStatus"], 200);
        assert_eq!(value["summary"], "首段等待超时");
        let legacy = serde_json::to_value(localize_record(&json!({"event":"finished","status":200})).unwrap()).unwrap();
        assert_eq!(legacy["attempt"], Value::Null);
        assert_eq!(legacy["summary"], "连接结束，完整性待确认");
    }

    #[test]
    fn 易读诊断_等待解释包含耗时并保留请求标识() {
        let event = localize_record(&json!({"event":"request_status","requestId":"req-a","phase":"waiting_first_content","attempt":3,"elapsedMs":65000,"sinceLastProgressMs":null})).unwrap();
        assert!(event.detail.contains("首段内容"));
        assert!(event.detail.contains("1分5秒"));
        assert!(event.detail.contains("第 3 次尝试"));
        let value = serde_json::to_value(event).unwrap();
        assert_eq!(value["requestId"], "req-a");
        assert_eq!(value["code"], "request_status");
    }

    #[test]
    fn 易读诊断_只有完整终止证据才声称完成() {
        let event = |record| localize_record(&record).unwrap();
        assert!(!event(json!({"event":"finished","status":200})).detail.contains("（成功）"));
        let complete = event(json!({"event":"sse_ended","sawTerminal":true,"terminalKind":"response.completed","stallReason":null}));
        assert_eq!(complete.title, "回复已完整结束");
        for record in [
            json!({"event":"sse_ended","sawTerminal":false,"terminalKind":"response.completed","stallReason":null}),
            json!({"event":"sse_ended","sawTerminal":true,"terminalKind":"response.completed","stallReason":"progress_timeout"}),
            json!({"event":"sse_ended","sawTerminal":true,"terminalKind":"response.failed","stallReason":null}),
            json!({"event":"sse_ended","terminalKind":"future_terminal","stallReason":null}),
        ] {
            assert_ne!(event(record).title, "回复已完整结束");
        }
    }

    /// 当前写出契约主闸门：抽取事件码必须恰为当前清单 24 项。
    #[test]
    fn 契约_打包脚本事件码恰为清单24项且字典全覆盖() {
        let text = std::fs::read_to_string(mjs_path()).expect("读取打包脚本失败（只读）");
        let scanned = extract_events(&text);
        let expected: BTreeSet<String> = EXPECTED_ACTIVE_EVENTS.iter().map(|s| s.to_string()).collect();
        assert_eq!(expected.len(), 24, "当前写出清单应恰 24 项");
        assert_eq!(
            scanned, expected,
            "打包脚本 event 码与当前写出清单不一致。\n脚本独有：{:?}\n清单独有：{:?}",
            scanned.difference(&expected).collect::<Vec<_>>(),
            expected.difference(&scanned).collect::<Vec<_>>()
        );
        for e in &scanned {
            assert!(EVENT_BASE.iter().any(|row| row.0 == e), "当前字典缺事件码 `{e}`");
        }
        for e in EXPECTED_LEGACY_EVENTS {
            assert!(!scanned.contains(e), "退役事件 `{e}` 不应继续写出");
        }
    }

    /// 当前枚举仍严格核对核心字面量；历史枚举不参与此检查。
    #[test]
    fn 契约_当前子枚举字典覆盖且与脚本字面量一致() {
        let text = std::fs::read_to_string(mjs_path()).expect("读取打包脚本失败");
        for &(field, values) in EXPECTED_ACTIVE_SUB_ENUMS {
            for &v in values {
                assert!(SUB_LABELS.iter().any(|row| row.0 == field && row.1 == v),
                    "当前字典缺子枚举 `{field}={v}` 的中文标签");
                // null 是 JS null；status_band 是壳侧派生段，均不作为脚本字符串核对。
                if v != "null" && field != "status_band" {
                    let needle = format!("'{v}'");
                    assert!(text.contains(needle.as_str()),
                        "当前子枚举 `{field}={v}` 未作为字面量出现在打包脚本");
                }
            }
        }
    }

    /// 当前与历史清单分别做集合相等、去重和计数；不能用历史字典掩盖当前缺项。
    #[test]
    fn 契约_字典键计数且无清单外多余条目() {
        let active_events: BTreeSet<_> = EVENT_BASE.iter().map(|row| row.0).collect();
        let legacy_events: BTreeSet<_> = LEGACY_EVENT_BASE.iter().map(|row| row.0).collect();
        assert_eq!(active_events, EXPECTED_ACTIVE_EVENTS.into_iter().collect());
        assert_eq!(legacy_events, EXPECTED_LEGACY_EVENTS.into_iter().collect());
        assert_eq!(EVENT_BASE.len(), 24);
        assert_eq!(LEGACY_EVENT_BASE.len(), 1);
        assert!(active_events.is_disjoint(&legacy_events));

        for (labels, groups, count) in [
            (SUB_LABELS, EXPECTED_ACTIVE_SUB_ENUMS, 81),
            (LEGACY_SUB_LABELS, EXPECTED_LEGACY_SUB_ENUMS, 4),
        ] {
            let actual: BTreeSet<_> = labels.iter().map(|row| (row.0, row.1)).collect();
            let expected: BTreeSet<_> = groups.iter()
                .flat_map(|(field, values)| values.iter().map(move |value| (*field, *value))).collect();
            assert_eq!(actual, expected, "字典必须逐项等于对应清单");
            assert_eq!(labels.len(), actual.len(), "不允许重复字典键");
            assert_eq!(labels.len(), count, "字典标签数与契约不一致");
            assert!(labels.iter().all(|row| !row.2.is_empty()), "中文标签不能为空");
        }
        assert!(SUB_LABELS.iter().all(|row| !LEGACY_SUB_LABELS.iter()
            .any(|old| old.0 == row.0 && old.1 == row.1)), "当前与历史枚举不能重复");
        assert_eq!(EVENT_BASE.len() + LEGACY_EVENT_BASE.len() + SUB_LABELS.len() + LEGACY_SUB_LABELS.len(), 110);
    }

    #[test]
    fn 契约_历史事件和枚举保持可读且不冒充当前写出() {
        for event in EXPECTED_LEGACY_EVENTS {
            let localized = localize_record(&json!({"event": event})).unwrap();
            assert!(!localized.title.contains("未识别事件"));
            assert!(LEGACY_EVENT_BASE.iter().any(|row| row.0 == event));
            assert!(!EVENT_BASE.iter().any(|row| row.0 == event));
        }
        for &(field, values) in EXPECTED_LEGACY_SUB_ENUMS {
            for &value in values {
                assert!(sub_label(field, value).is_some(), "旧枚举 {field}={value} 必须可读");
                let localized = localize_record(&json!({"event":"cache_route_summary",(field):value})).unwrap();
                assert!(!localized.technical_detail.contains("原始码"));
                assert!(localized.technical_detail.contains(sub_label(field, value).unwrap()));
            }
        }
    }

    /// 真实样本（trimmed）：读 tests/fixtures/diagnostics-samples.jsonl，逐条断言译文/级别。
    fn load_fixture() -> Vec<Value> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests").join("fixtures").join("diagnostics-samples.jsonl");
        let text = std::fs::read_to_string(&path).expect("读取诊断样本 fixture 失败");
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("fixture 行应为合法 JSON"))
            .collect()
    }

    /// 按谓词取 fixture 里第一条匹配记录。
    fn pick<'a>(recs: &'a [Value], pred: impl Fn(&Value) -> bool) -> &'a Value {
        recs.iter().find(|&r| pred(r)).expect("fixture 应含该记录")
    }
    fn ev(r: &Value) -> &str {
        r.get("event").and_then(Value::as_str).unwrap_or("")
    }
    fn status_is(r: &Value, s: i64) -> bool {
        r.get("status").and_then(Value::as_i64) == Some(s)
    }

    #[test]
    fn 真实样本fixture_各记录译文级别正确() {
        use ActivityLevel::*;
        let recs = load_fixture();

        // 生命周期
        let a = localize_record(pick(&recs, |r| ev(r) == "gateway_started")).unwrap();
        assert_eq!((a.level, a.title.as_str()), (Info, "网关已启动"));
        let a = localize_record(pick(&recs, |r| ev(r) == "gateway_draining")).unwrap();
        assert_eq!(a.level, Info);
        assert!(a.technical_detail.contains("热重启排空"), "restart=true → 热重启排空：{}", a.technical_detail);

        // happy-path
        assert_eq!(localize_record(pick(&recs, |r| ev(r) == "received")).unwrap().title, "收到请求");
        // request_profile 抑制为 None（received 的兄弟记录）
        assert!(localize_record(pick(&recs, |r| ev(r) == "request_profile")).is_none());
        assert_eq!(localize_record(pick(&recs, |r| ev(r) == "attempt_started")).unwrap().title, "发起上游尝试");
        assert_eq!(localize_record(pick(&recs, |r| ev(r) == "sse_committed")).unwrap().level, Info);

        let up200 = localize_record(pick(&recs, |r| ev(r) == "upstream_response" && status_is(r, 200))).unwrap();
        assert_eq!(up200.level, Info);
        assert!(up200.technical_detail.contains("成功"), "{}", up200.technical_detail);
        let up400 = localize_record(pick(&recs, |r| ev(r) == "upstream_response" && status_is(r, 400))).unwrap();
        assert_eq!(up400.level, Info, "upstream_response 级别恒信息（成败由 finished/sse_ended 定）");
        assert!(up400.technical_detail.contains("请求被拒"), "{}", up400.technical_detail);

        // sse_ended：三种 terminalKind/stallReason → 三种级别
        let done = localize_record(pick(&recs, |r| ev(r) == "sse_ended" && r.get("terminalKind").and_then(Value::as_str) == Some("response.completed"))).unwrap();
        assert_eq!(done.level, Info);
        assert!(done.technical_detail.contains("正常完成"), "{}", done.technical_detail);
        let failed = localize_record(pick(&recs, |r| ev(r) == "sse_ended" && r.get("terminalKind").and_then(Value::as_str) == Some("response.failed"))).unwrap();
        assert_eq!(failed.level, Error);
        assert!(failed.technical_detail.contains("失败终止"), "{}", failed.technical_detail);
        let closed = localize_record(pick(&recs, |r| ev(r) == "sse_ended" && r.get("stallReason").and_then(Value::as_str) == Some("client_closed"))).unwrap();
        assert_eq!(closed.level, Info, "stallReason=client_closed 客户端主动断 → 信息");
        assert!(closed.technical_detail.contains("客户端断开"), "{}", closed.technical_detail);

        assert_eq!(localize_record(pick(&recs, |r| ev(r) == "client_closed")).unwrap().level, Warn);

        // finished：HTTP 段定级
        assert_eq!(localize_record(pick(&recs, |r| ev(r) == "finished" && status_is(r, 200))).unwrap().level, Info);
        assert_eq!(localize_record(pick(&recs, |r| ev(r) == "finished" && status_is(r, 400))).unwrap().level, Warn);
        assert_eq!(localize_record(pick(&recs, |r| ev(r) == "finished" && status_is(r, 402))).unwrap().level, Warn);
        assert_eq!(localize_record(pick(&recs, |r| ev(r) == "finished" && status_is(r, 503))).unwrap().level, Error);

        // request_status：phase 决定详情
        let rs_r = localize_record(pick(&recs, |r| ev(r) == "request_status" && r.get("phase").and_then(Value::as_str) == Some("reasoning"))).unwrap();
        assert!(rs_r.technical_detail.contains("正在输出推理"), "{}", rs_r.technical_detail);
        let rs_t = localize_record(pick(&recs, |r| ev(r) == "request_status" && r.get("phase").and_then(Value::as_str) == Some("tool_output"))).unwrap();
        assert!(rs_t.technical_detail.contains("正在输出工具调用"), "{}", rs_t.technical_detail);

        // 错误/修复路径
        let ie = localize_record(pick(&recs, |r| ev(r) == "item_error")).unwrap();
        assert_eq!(ie.level, Warn);
        assert!(ie.technical_detail.contains("跨 Azure 资源引用"), "{}", ie.technical_detail);
        let rr = localize_record(pick(&recs, |r| ev(r) == "repair_refused")).unwrap();
        assert_eq!(rr.level, Warn);
        assert!(rr.technical_detail.contains("内联历史不安全"), "{}", rr.technical_detail);
        assert_eq!(localize_record(pick(&recs, |r| ev(r) == "codex_resource_retry")).unwrap().level, Info);

        let exh_limit = localize_record(pick(&recs, |r| ev(r) == "codex_resource_exhausted" && r.get("refusal").and_then(Value::as_str) == Some("resource_attempt_limit"))).unwrap();
        assert_eq!(exh_limit.level, Error);
        assert!(exh_limit.technical_detail.contains("资源尝试达上限"), "{}", exh_limit.technical_detail);
        let exh_unsafe = localize_record(pick(&recs, |r| ev(r) == "codex_resource_exhausted" && r.get("refusal").and_then(Value::as_str) == Some("unsafe_inline_history"))).unwrap();
        assert_eq!(exh_unsafe.level, Error);

        let pf = localize_record(pick(&recs, |r| ev(r) == "precommit_failure")).unwrap();
        assert_eq!(pf.level, Error, "kind=stream_error → 错误");
        assert!(pf.technical_detail.contains("流内携带错误"), "{}", pf.technical_detail);

        // 缓存路由观察：只陈述本次事实，不冒充故障结论
        let cr = localize_record(pick(&recs, |r| ev(r) == "cache_route_summary")).unwrap();
        assert_eq!(cr.level, Info);
        assert_eq!(cr.title, "缓存路由记录");
        assert_eq!(cr.summary, "换键后完成");
        assert!(cr.detail.contains("客户端原键"), "{}", cr.detail);
        assert!(cr.detail.contains("新的缓存键"), "{}", cr.detail);
        assert!(cr.detail.contains("最终完成"), "{}", cr.detail);
        assert!(cr.detail.contains("不能据此认定缓存通道损坏"), "{}", cr.detail);
        assert!(cr.technical_detail.contains("流式完整结束"), "{}", cr.technical_detail);
        assert!(cr.technical_detail.contains("换键后尝试 3 次"), "{}", cr.technical_detail);
    }

    #[test]
    fn 请求摘要新样本_读取通用观察且没有换键字段() {
        let recs = load_fixture();
        let samples: Vec<_> = recs.iter().filter(|r| ev(r) == "request_summary").collect();
        assert_eq!(samples.len(), 2);
        let fields: BTreeSet<_> = [
            "time", "event", "requestId", "path", "attempt", "status", "elapsedMs",
            "observationVersion", "client", "policyGroup", "inputBytes", "inputItems", "modelHash",
            "attempts", "rateErrors", "otherErrors", "rateFirstClass", "rateLastClass", "rateClassMixed",
            "waitHintSeen", "waitHintValid", "waitHintMaxMs", "waitHintInvalid", "firstContentMs",
            "result", "terminalKind", "usageShape", "usageInputTokens", "usageOutputTokens",
            "usageCachedTokens", "usageCacheReadTokens", "usageCacheCreationTokens",
        ].into_iter().collect();
        for sample in &samples {
            let keys: BTreeSet<_> = sample.as_object().unwrap().keys().map(String::as_str).collect();
            assert_eq!(keys, fields, "新样本只包含通用协议字段，不能残留换键字段");
            assert_eq!(sample["observationVersion"], 1);
        }
        let event = localize_record(samples[0]).unwrap();
        assert_eq!(event.code, "request_summary");
        assert_eq!(event.title, "请求处理摘要");
        assert_eq!(event.summary, "流式完整结束");
        assert_eq!(event.level, ActivityLevel::Info);
        assert_eq!(event.attempt, Some(5));
        assert_eq!(event.http_status, Some(200));
        assert_eq!(event.request_id.as_deref(), samples[0]["requestId"].as_str());
        assert_eq!(event.time, samples[0]["time"].as_str().unwrap());
        for expected in ["实际尝试 5 次", "限流 3 次", "其他错误 1 次"] {
            assert!(event.detail.contains(expected), "{}", event.detail);
        }
        for expected in [
            "观察版本：1", "输入字节：240000", "输入条目：120", "客户端：Claude", "策略组：通用策略组",
            "实际尝试次数：5", "首次限流类别：token 频率限流", "末次限流类别：请求频率限流",
            "限流类别是否变化：是", "等待提示检查次数：4", "有效等待提示次数：2", "无效等待提示次数：1",
            "最大等待提示：2000 ms", "仅观察，不代表实际等待时长", "首次内容 12秒", "总耗时 18秒",
            "终止类型：正常完成", "用量形状：Responses 用量", "输入 token：240000", "输出 token：300", "已缓存 token：200000",
        ] {
            assert!(event.technical_detail.contains(expected), "缺少 {expected}：{}", event.technical_detail);
        }
        assert!(event.technical_detail.contains(samples[0]["modelHash"].as_str().unwrap()));
        for forbidden in ["换键", "起始选键", "前段", "缓存读取 token：0", "缓存创建 token：0"] {
            assert!(!event.technical_detail.contains(forbidden), "{}", event.technical_detail);
        }
    }

    #[test]
    fn 请求摘要_JSON已返回不冒充模型成功_缺失值不补零() {
        let recs = load_fixture();
        let event = localize_record(pick(&recs, |r| ev(r) == "request_summary" && r["result"] == "json_returned")).unwrap();
        assert_eq!(event.summary, "已返回 JSON 响应");
        assert!(event.detail.contains("已返回 JSON 响应不等于模型已成功完成"));
        assert!(!event.detail.contains("最终完成"));
        assert!(event.technical_detail.contains("未提供用量"));
        for forbidden in ["输入 token：", "输出 token：", "已缓存 token：", "首次内容", "最大等待提示", "截断"] {
            assert!(!event.technical_detail.contains(forbidden), "{}", event.technical_detail);
        }
        for record in [
            json!({"event":"request_summary"}),
            json!({"event":"request_summary","attempts":null,"rateErrors":null,"otherErrors":null,"usageInputTokens":null}),
        ] {
            let event = localize_record(&record).unwrap();
            assert_eq!(event.attempt, None);
            assert_eq!(event.http_status, None);
            assert_eq!(event.summary, "结果未确认");
            assert_eq!(event.technical_detail, "结果：未确认。");
            assert!(!event.detail.contains("0 次"));
        }
        let zero = localize_record(&json!({"event":"request_summary","attempts":0,"rateErrors":0,"usageInputTokens":0,"usageCacheReadTokens":7,"usageCacheCreationTokens":9})).unwrap();
        for expected in ["实际尝试次数：0", "限流次数：0", "输入 token：0", "缓存读取 token：7", "缓存创建 token：9"] {
            assert!(zero.technical_detail.contains(expected), "实际报告的值须保留：{}", zero.technical_detail);
        }
    }

    #[test]
    fn 请求摘要_全部结果与枚举可读且未知值诚实回退() {
        for result in EXPECTED_RESULT {
            let event = localize_record(&json!({"event":"request_summary","result":result})).unwrap();
            assert_eq!(event.summary, sub_label("result", result).unwrap());
            assert!(!event.technical_detail.contains("原始码"));
        }
        for (field, dictionary, values) in [
            ("rateFirstClass", "rateClass", EXPECTED_RATE_CLASS.as_slice()),
            ("rateLastClass", "rateClass", EXPECTED_RATE_CLASS.as_slice()),
            ("client", "client", EXPECTED_CLIENT.as_slice()),
            ("policyGroup", "policyGroup", EXPECTED_POLICY_GROUP.as_slice()),
            ("usageShape", "usageShape", EXPECTED_USAGE_SHAPE.as_slice()),
        ] {
            for value in values {
                let event = localize_record(&json!({"event":"request_summary",(field):value})).unwrap();
                assert!(event.technical_detail.contains(sub_label(dictionary, value).unwrap()));
                assert!(!event.technical_detail.contains("原始码"));
            }
        }
        for field in ["result", "rateFirstClass", "rateLastClass", "client", "policyGroup", "usageShape", "terminalKind"] {
            let event = localize_record(&json!({"event":"request_summary",(field):"future_value"})).unwrap();
            assert!(event.technical_detail.contains("原始码：future_value"), "{field} 必须诚实回退");
        }
        let event = localize_record(&json!({"event":"request_status","phase":"future_phase"})).unwrap();
        assert!(event.detail.contains("原始码：future_phase"));
    }

    #[test]
    fn 请求摘要_不读取退役字段_旧摘要仍按旧协议解释() {
        let base = json!({"event":"request_summary","result":"cancelled","attempts":2});
        let mut mixed = base.clone();
        for (field, value) in json!({"initialRoute":"remembered","scopeHash":"old-scope","firstKeyHash":"old-key","lastKeyHash":"new-key","candidatePrepared":true,"candidatePreparedMs":1,"candidateStartMs":2,"remainingBudgetMs":3000,"mappingRecorded":true,"beforeAttempts":9,"afterAttempts":8,"beforeRateErrors":7,"afterRateErrors":6,"beforeOtherErrors":5,"afterOtherErrors":4,"beforeElapsedMs":3,"afterElapsedMs":2}).as_object().unwrap() {
            mixed[field] = value.clone();
        }
        assert_eq!(localize_record(&base), localize_record(&mixed), "新事件不能走历史换键解释");
        let old = localize_record(&json!({"event":"cache_route_summary","candidatePrepared":true,"afterAttempts":1,"result":"json_returned"})).unwrap();
        assert_eq!(old.summary, "换键后已返回 JSON");
        assert!(old.detail.contains("已返回 JSON 响应"));
        assert!(!old.detail.contains("最终完成"));
        let partial = localize_record(&json!({"event":"cache_route_summary"})).unwrap();
        assert_eq!(partial.summary, "换键记录不完整");
        assert!(!partial.technical_detail.contains("0 次"));
        assert!(!partial.technical_detail.contains("是否准备换键：否"));
    }

    #[test]
    fn 等待重试样本_网关等待不是上游生成且尝试次数不预增() {
        let recs = load_fixture();
        let record = pick(&recs, |r| ev(r) == "request_status" && r["phase"] == "retry_waiting");
        let event = localize_record(record).unwrap();
        assert_eq!(event.title, "等待重试");
        assert_eq!(event.attempt, Some(3));
        assert_eq!(event.summary, "已用时 1分5秒");
        assert!(event.detail.contains("网关正在等待后再试，不是上游正在生成内容"));
        assert!(event.detail.contains("下一次尝试尚未发出"));
        assert!(event.detail.contains("第 3 次尝试"));
        assert!(event.detail.contains("尚未收到内容进展"));
        for forbidden in ["第 4 次", "倒计时", "剩余", "预计"] { assert!(!event.detail.contains(forbidden)); }
        let mut next = record.clone();
        next["phase"] = json!("waiting_headers");
        next["attempt"] = json!(4);
        let event = localize_record(&next).unwrap();
        assert_eq!(event.title, "等待上游响应");
        assert_eq!(event.attempt, Some(4));
    }

    #[test]
    fn 账号绑定旧推理错误_保留中文映射() {
        let event = localize_record(&json!({"event":"item_error","kind":"account_reasoning_unavailable","source":"json_error","status":200})).unwrap();
        assert_eq!(event.summary, "账号绑定的旧推理不可用");
        assert!(event.technical_detail.contains("账号绑定的旧推理不可用"));
    }

    #[test]
    fn 未覆盖错误保底样本_复用三事件及既有拒绝原因且不冒充归因() {
        use ActivityLevel::*;
        let recs = load_fixture();
        let samples: Vec<_> = recs.iter()
            .filter(|record| record["kind"] == "uncovered_error_fallback")
            .collect();
        assert_eq!(samples.len(), 5);
        let expected = [
            ("item_error", Warn, None),
            ("repair_applied", Info, None),
            ("repair_refused", Warn, Some("repair_exhausted")),
            ("repair_refused", Warn, Some("unsafe_inline_history")),
            ("repair_refused", Warn, Some("time_budget")),
        ];
        for (record, (code, level, refusal)) in samples.iter().zip(expected) {
            let event = localize_record(record).unwrap();
            assert_eq!(event.code, code);
            assert_eq!(event.level, level);
            assert!(event.title.contains("未覆盖错误保底"));
            assert!(event.technical_detail.contains("未覆盖错误保底"));
            assert!(!event.technical_detail.contains("原始码"));
            assert_eq!(event.attempt, record["attempt"].as_u64());
            assert_eq!(record["refusal"].as_str(), refusal);
            if let Some(refusal) = refusal {
                let label = sub_label("refusal", refusal).unwrap();
                assert_eq!(event.summary, label);
                assert!(event.detail.contains(label));
                assert!(event.technical_detail.contains(label));
            }
        }
        let identified = localize_record(samples[0]).unwrap();
        let applied = localize_record(samples[1]).unwrap();
        let exhausted = localize_record(samples[2]).unwrap();
        assert_eq!(identified.summary, "未覆盖错误保底");
        assert!(identified.detail.contains("尚未确认错误由历史引起"));
        assert!(applied.detail.contains("可能损失旧推理连续性"));
        assert!(applied.detail.contains("不跨请求记忆"));
        assert_eq!(identified.request_id, applied.request_id);
        assert_eq!(applied.request_id, exhausted.request_id);
        assert_eq!(applied.attempt, Some(2));
        assert_eq!(exhausted.attempt, Some(4));
    }

    /// 额外事件形状（手造最小记录，字段形状照 diagnostics-codes.md §1/§2）。
    #[test]
    fn 未采样事件_手造记录_译文级别正确() {
        use ActivityLevel::*;
        let base = |ev: Value| localize_record(&ev).unwrap();

        let r = base(json!({"time":"t","event":"session_compat_refused","reason":"unsafe_inline_history"}));
        assert_eq!(r.level, Warn);

        let r = base(json!({"time":"t","event":"codex_session_passthrough","reason":"unsafe_inline_history"}));
        assert_eq!(r.level, Info);

        let r = base(json!({"time":"t","event":"session_compat_applied","source":"explicit_session","removedIds":2}));
        assert_eq!(r.level, Info);
        assert!(r.technical_detail.contains("显式会话"), "{}", r.technical_detail);
        let r = base(json!({"time":"t","event":"session_compat_applied","source":"resource_lineage"}));
        assert!(r.technical_detail.contains("资源血缘"), "{}", r.technical_detail);

        let r = base(json!({"time":"t","event":"session_compat_enabled","kind":"azure","source":"explicit_session"}));
        assert_eq!(r.level, Info);

        // status_repair_refused：reason 全 6 值均有译文且为警告
        for reason in EXPECTED_STATUS_REPAIR_REASON {
            let r = base(json!({"time":"t","event":"status_repair_refused","source":"http_error","status":400,"reason":reason}));
            assert_eq!(r.level, Warn, "status_repair_refused reason={reason}");
            assert!(sub_label("reason", reason).is_some());
            assert!(!r.technical_detail.contains("原始码"), "reason={reason} 应已汉化：{}", r.technical_detail);
        }

        let r = base(json!({"time":"t","event":"status_repair_applied","source":"http_error","status":400,"index":3}));
        assert_eq!(r.level, Info);

        // repair_applied：补充空回应形状修复，与保底样本区分。
        let r = base(json!({"time":"t","event":"repair_applied","kind":"empty_stream_shape","source":"json_empty","status":200,"removedIds":4}));
        assert_eq!(r.level, Info);
        assert!(r.technical_detail.contains("空回应形状修复"), "{}", r.technical_detail);
    }

    /// 样本未触发的子枚举值（kind/source/refusal/terminalKind/stallReason/phase）全覆盖、不回退。
    #[test]
    fn 未采样子枚举值_均已汉化不回退() {
        // item/peek/shape 全部 kind
        for k in EXPECTED_KIND {
            let r = localize_record(&json!({"time":"t","event":"item_error","kind":k,"source":"json_error","status":200})).unwrap();
            assert!(!r.technical_detail.contains("原始码"), "kind={k} 未汉化：{}", r.technical_detail);
        }
        // 全部 phase
        for p in EXPECTED_PHASE {
            let r = localize_record(&json!({"time":"t","event":"request_status","phase":p})).unwrap();
            assert!(!r.technical_detail.contains("原始码"), "phase={p} 未汉化：{}", r.technical_detail);
        }
        // 全部 source（经 item_error 详情体现）
        for s in EXPECTED_SOURCE {
            assert!(sub_label("source", s).is_some(), "source={s}");
        }
        // 全部 refusal（经 repair_refused）
        for rf in EXPECTED_REFUSAL {
            let r = localize_record(&json!({"time":"t","event":"repair_refused","kind":"stale_reasoning_item","source":"http_error","status":400,"refusal":rf})).unwrap();
            assert_eq!(r.level, ActivityLevel::Warn);
            assert!(!r.technical_detail.contains("原始码"), "refusal={rf} 未汉化：{}", r.technical_detail);
        }
    }

    /// sse_ended 定级：覆盖全部 terminalKind × stallReason 的代表组合（§4）。
    #[test]
    fn sse_ended_定级覆盖terminalKind与stallReason() {
        use ActivityLevel::*;
        let lvl = |tk: Value, sr: Value| {
            localize_record(&json!({"time":"t","event":"sse_ended","terminalKind":tk,"stallReason":sr})).unwrap().level
        };
        // 无停滞：按 terminalKind
        assert_eq!(lvl(json!("response.completed"), json!(null)), Info);
        assert_eq!(lvl(json!("done_marker"), json!(null)), Info);
        assert_eq!(lvl(json!("message_stop"), json!(null)), Info);
        assert_eq!(lvl(json!("chat_finish_reason"), json!(null)), Info);
        assert_eq!(lvl(json!("response.incomplete"), json!(null)), Warn);
        assert_eq!(lvl(json!("response.failed"), json!(null)), Error);
        assert_eq!(lvl(json!(null), json!(null)), Warn, "未见终止事件=截断");
        // 有停滞：停滞决定级别（压过 terminalKind）
        assert_eq!(lvl(json!(null), json!("progress_timeout")), Warn);
        assert_eq!(lvl(json!(null), json!("transport_idle_timeout")), Warn);
        assert_eq!(lvl(json!(null), json!("upstream_read_error")), Error);
        assert_eq!(lvl(json!(null), json!("event_size_limit")), Warn);
        assert_eq!(lvl(json!("response.completed"), json!("client_closed")), Info);
    }

    /// finished 定级：2xx 信息 / 4xx 警告 / 5xx 错误（§4）。
    #[test]
    fn finished_定级按http段() {
        use ActivityLevel::*;
        let lvl = |s: i64| localize_record(&json!({"time":"t","event":"finished","status":s})).unwrap().level;
        assert_eq!(lvl(200), Info);
        assert_eq!(lvl(400), Warn);
        assert_eq!(lvl(402), Warn);
        assert_eq!(lvl(429), Warn);
        assert_eq!(lvl(502), Error);
        assert_eq!(lvl(503), Error);
    }

    /// precommit_failure 定级：按 kind（§4：stream_error/buffer_limit=错误，其余=警告）。
    #[test]
    fn precommit_failure_定级按kind() {
        use ActivityLevel::*;
        let lvl = |k: &str| localize_record(&json!({"time":"t","event":"precommit_failure","source":"sse_precommit","kind":k})).unwrap().level;
        assert_eq!(lvl("peek_timeout"), Warn);
        assert_eq!(lvl("empty_stream_drop"), Warn);
        assert_eq!(lvl("read_timeout"), Warn);
        assert_eq!(lvl("other"), Warn);
        assert_eq!(lvl("stream_error"), Error);
        assert_eq!(lvl("buffer_limit"), Error);
    }

    /// 未知事件码：诚实回退，原样显示原始码，不隐藏、不 panic。
    #[test]
    fn 未知事件码_诚实回退显示原始码() {
        let r = localize_record(&json!({"time":"2026-10-11T00:00:00Z","event":"brand_new_upstream_code","foo":1})).unwrap();
        assert_eq!(r.level, ActivityLevel::Info);
        assert!(r.title.contains("brand_new_upstream_code"), "回退标题须含原始码：{}", r.title);
        assert_eq!(r.time, "2026-10-11T00:00:00Z", "时间透传");
    }

    /// 已知事件 + 未知子枚举值：不 panic，详情回退「原始码：X」，级别安全兜底。
    #[test]
    fn 未知子枚举值_诚实回退不崩() {
        // sse_ended 未知 terminalKind（无停滞）→ 有终止即信息（诚实回退）
        let r = localize_record(&json!({"time":"t","event":"sse_ended","terminalKind":"response.brand_new","stallReason":null})).unwrap();
        assert_eq!(r.level, ActivityLevel::Info);
        assert!(r.technical_detail.contains("原始码：response.brand_new"), "{}", r.technical_detail);
        // precommit_failure 未知 kind → 兜底警告
        let r = localize_record(&json!({"time":"t","event":"precommit_failure","kind":"mystery_kind"})).unwrap();
        assert_eq!(r.level, ActivityLevel::Warn);
        assert!(r.technical_detail.contains("原始码：mystery_kind"), "{}", r.technical_detail);
        // item_error 未知 source → 详情回退，不崩
        let r = localize_record(&json!({"time":"t","event":"item_error","kind":"azure_resource_mismatch","source":"new_layer","status":400})).unwrap();
        assert!(r.technical_detail.contains("原始码：new_layer"), "{}", r.technical_detail);
    }

    /// None 语义：无 event / event 非字符串 / request_profile / 非法行。
    #[test]
    fn 无event字段_或request_profile_返回None() {
        assert!(localize_record(&json!({"time":"t","path":"/x"})).is_none(), "无 event → None");
        assert!(localize_record(&json!({"time":"t","event":123})).is_none(), "event 非字符串 → None");
        assert!(localize_record(&json!({"time":"t","event":"request_profile","client":"codex"})).is_none(), "request_profile 抑制");
        assert!(localize_line("not a json line").is_none(), "非法 JSON 行 → None");
        assert!(localize_line("").is_none());
    }

    /// 透传槽：时间 / 路径 / 次数等原样搬运、绝不翻译。
    #[test]
    fn 透传槽原样保留_数字与时间不翻译() {
        let r = localize_record(&json!({
            "time":"2026-10-11T11:49:17.160Z","event":"attempt_started",
            "path":"/v1/responses","attempt":7,"repairCount":0
        })).unwrap();
        assert_eq!(r.time, "2026-10-11T11:49:17.160Z");
        assert!(r.technical_detail.contains("第 7 次"), "attempt 数字透传：{}", r.technical_detail);

        // 级别序列化为稳定小写机器码；中文标签经 as_cn 取用
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"level\":\"info\""), "{json}");
        assert_eq!(ActivityLevel::Warn.as_cn(), "警告");
    }
}
