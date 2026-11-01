# 账号绑定的旧推理不可用（曾称 Astra 绑定报错）

日期：2026-11-01。该报错并非 Astra 或 Claude 专属；按用户更正，Codex 也可能出现。以下规则与验证均不按客户端或模型写死。

## 现象

2026-11-01 19:33，Claude 经中转发出非流 Responses 请求。网关日志显示：342 个条目（74 条 reasoning、120 次函数调用及结果、28 条无 type 的消息），共 6 次尝试，前 5 次上游均返回 HTTP 400，第 6 次发出后客户端在 42 秒时取消。全程没有 `session_compat_*`、`repair_*`、`codex_*` 或 `sse_*` 事件，说明网关只是用同一份请求体反复重试。

截图中的错误原文为：

> the upstream account bound to this conversation is unavailable; start a new conversation without previous encrypted reasoning items

网关日志不保存错误正文，CC Switch 数据库也没有这笔 19:33 记录；该原文目前只能以截图为准，不能从本机日志独立核对。UI 显示的 0 秒与网关实际 42 秒不一致，客户端日志缺失。

## 为什么 v0.3.18 没有剥离

1. 文案含 `unavailable`，先被通用规则归为“可重试错误”；
2. 历史/绑定识别只在“未知类错误”上继续判断，因此根本进不去；
3. 文案不匹配现有“会话绑定失效”“跨 Azure 资源”“加密推理不可验证”三类关键词；
4. 即使识别，原会话绑定恢复只对严格 Codex 生效，并且只去条目编号、保留推理；这条错误明确要求不带旧加密推理，方向相反。

所以这是策略覆盖缺口，不是漏了一个次数。旧行为符合当时的既定策略，但对这条错误无效。

## 实现边界

- 仅 `POST /v1/responses`；HTTP 400/409、200 JSON 错误、下发前 SSE 错误共用识别。必须同时出现 `upstream account bound to this conversation` 与 `without previous encrypted reasoning items` 两条短语。
- 单独 `unavailable`（普通 503）与 `rate_limit_exceeded`（普通 429）不触发；负例验证五次发送的请求体完全不变。
- 首次命中删除旧 `reasoning` 项；保留 message、function_call/output、`call_id`、工具参数/结果、`client_metadata` 与 UA/鉴权等身份头。Codex 按既有规则另去可选 item id，Claude 保留。
- 本次请求内最多两次重试（首错后修复并发送一次，再重发一次）；仍报同一错误即停止并返回错误。不写入“下一条请求自动删推理”的兼容缓存。
- 不改变重试预算、总时间预算、取消、已交付不重放；不扩大既有会话绑定或跨资源修复。

## 验证

`scripts/gateway-history.test.mjs` 使用真实核心、随机回环端口、假上游和假凭据：

- HTTP、200 JSON、SSE 三种载体：首错后仅第二次请求无推理，消息与工具保持；失败版本先红后绿。
- Claude 与 Codex 各一组；Codex 额外验证可选编号被去除，工具与身份保留。
- 同一错误持续：恰好 3 次发送（原始 + 两次重试）后停止，后两次请求体相同。
- 普通 503 与 429：各 5 次发送，全部请求体与原始一致。
- 历史全量 139 项通过，含旧核心 8 项逐项差分。

## 证据缺口

- 没有真实账号下去掉旧推理后成功继续的样本；假上游只能证明网关行为。
- 不确定该错误是否账号级：若去掉推理仍无效，网关会按设计在两次重试后停止，不会无限重发。
- 客户端是否在下一轮重复发送旧推理、以及 74 条 reasoning 是否都带 `encrypted_content`，仍需真实请求确认。

源码已改，但未发布、未安装、未重启现用网关。
