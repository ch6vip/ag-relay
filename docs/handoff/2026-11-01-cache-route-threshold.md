# 交接：收紧"换缓存键"的触发阈值（v0.3.19）

接续 2026-11-01 的排查。**核心 bug 已修完并已发布 v0.3.18**，这里只剩一个**参数调优**问题。

## 一句话背景

用户上游（agentrouter.org）会按请求里的 `prompt_cache_key` 把请求黏在一条通道上。那条通道 token 额度打满后，就一直返回 `rate_limit_exceeded`，旧网关只会原样重发，于是"永远转"，一轮能重试 57～110 次、耗时六分钟才被客户端掐断。

v0.3.18 的对策：**连续两次明确限流后，换一次 `prompt_cache_key`**，其余（历史、推理、工具、会话身份、重试次数、预算）全不动。实测同一份卡死的历史：旧键 9.5 秒就被限流，换新键 42.7 秒完整跑完。修复本身有效，已发布。

## 现在要解决的问题

**这个"连续 2 次"太松了，会误伤大量普通限流，导致缓存被反复重建。**

用用户自己的真实日志统计（`%APPDATA%\ag-relay\logs\relay-diagnostics.jsonl`）：

| 触发条件 | Claude 触发率 | Codex 触发率 | 其中"本来等一等就能成功"的 |
| --- | ---: | ---: | --- |
| **当前：连续 2 次** | 84% | 87% | Claude 14 笔、Codex 156 笔 |
| 连续 5 次 | 56% | 63% | Claude 5、Codex 62 |
| **连续 5 次且已等 60 秒** | 41% | 39% | Claude **1**、Codex **6** |
| 连续 8 次且已等 120 秒 | 25% | 17% | Claude 0、Codex 1 |

关键事实：**普通限流几乎都能自己恢复**——Claude 限流 1～4 次的 14 笔全部最终成功；Codex 限流 1～9 次的 210 笔里 208 笔成功。所以"限流"本身不是换键理由，只有"重试很多次、等很久还不行"才是。

## 建议方案

三个条件**同时**满足才换键：

1. 连续限流次数 ≥ 5
2. 本请求已耗时 ≥ 60 秒
3. 错误文案带区域 / token 额度特征（如 `in eastus2 have exceeded token rate limit`）

第 3 条是更精准的判别：卡死的报错带**区域名 + token rate limit**；普通瞬时限流文案不同或没有区域信息。要新增一个"限流是否属于缓存黏滞型"的判别函数，放在 `classifyError` 附近。

按现有数据，这样触发率约 40%，误伤降到个位数。**阈值可以再按你的判断调整，改完用下面方法回放验证再说。**

## 要改的地方

| 文件 | 位置 | 说明 |
| --- | --- | --- |
| `src-tauri/resources/gateway/relay-retry-proxy.mjs` | `noteCacheRateLimit()` | 现在的判断是 `if (++cacheRateFailures < 2 ...) return;`，改成上面的三条件；需要拿到请求已耗时（`Date.now() - started`，函数内有 `deadline`，可反推起点）和错误文案 |
| 同上 | `cacheRouteIdentity()` | 作用范围：Claude + 严格识别 Codex，需自带非空 `prompt_cache_key`。保持不动 |
| 同上 | `classifyError()` | 附近适合加"缓存黏滞型限流"判别 |
| `scripts/gateway-policy.test.mjs` | `缓存路由` 那组用例（20 项） | 现有 20 项按"2 次"写死，改阈值后要同步更新，并补"普通限流不换键""耗时不足不换键""文案不匹配不换键" |
| `docs/retry-boundaries.md` | `#cache-route-recovery` 段 | 阈值与判别规则写进去 |

**不要动**：重试次数、退避、总预算、历史恢复策略、其他客户端分支。

## 怎么验证

```bash
node --test --test-name-pattern='缓存路由' scripts/gateway-policy.test.mjs
node --test scripts/gateway-policy.test.mjs
```

完整回归（当前基线，全绿）：

```bash
AG_RELAY_HISTORY_BASELINE=<旧核心快照路径> node --test scripts/gateway-history.test.mjs
```

基线快照可从 `git show v0.3.17:src-tauri/resources/gateway/relay-retry-proxy.mjs` 导出到临时文件。

**回放真实日志验证误伤率**（推荐）：读 `relay-diagnostics.jsonl`，按"同一 requestId 内 `precommit_failure` + `code=rate_limit_exceeded` 的次数与首末时间差"套用新阈值，统计会触发换键的请求数，以及其中最终 `sse_ended` 成功的比例。当前实现下这两份日志共有 Claude 32 笔、Codex 398 笔限流请求，可作为对照基线。

## 发布注意

- 已发布的是 **v0.3.18**（Release 已公开并标 Latest，tag 已推）。本次改动应发 **v0.3.19**。
- 流程按 `docs/release-runbook.md`：`node scripts/bump-version.mjs 0.3.19` → 写 `docs/releases/v0.3.19.md` → 刷许可指纹 → `npm run release:build` → tag → `npm run release:upload`。
- **许可校验的坑**：本机 rustc 是 1.99.0，`licenses/inventory.json` 已按 1.99.0 记录（此前记的是 1.99.1，材料逐字节相同，只是版本标注旧）。换机器如果 rustc 版本不同，`check-licenses.mjs --rust-version` 会拒绝，需先核对标准库 `COPYRIGHT-library.html` 是否与材料一致再更新记录，**不要直接改版本号蒙混**。
- 更新后需要重启网关才生效；用户是手动安装更新的。

## 已排除的方向（不要重复排查）

- 不是 0.3.16／0.3.17 的回归：10-28（v0.3.14 时期）日志已有 Codex 38 笔、10-29 有 191 笔超过 10 次重试，早于两个版本发布；两版也只改了流式交付时机。
- 不是"全怪上游"：直连上游同样被限流，说明限流真实存在。
- 不是"全怪网关"：但空转时长是网关放大的（同请求内重试 57 次 × 约 6 秒）。
- `prompt_cache_key` 来自 CC Switch 转换层，取会话 id，不随工具列表变化——换键不影响会话身份和工具缓存。

## 现场调试脚本（未入库，需重建）

`.claude/debug/claude-limit-20261101/` 下有本次用的还原与探针脚本（`reconstruct.py`、`probe.mjs`、`isolated-live.mjs`），但 `.claude/debug/` 被 gitignore，**不会随仓库过去**。它们做的事：从 `.claude/projects/.../<session>.jsonl` 的 `api-request-blob` / `api-request` 逐条哈希校验还原原始 Messages 请求，转成 Responses 后经独立网关（随机端口、临时目录、限定尝试次数）发一次真实请求。需要时可在家里电脑按同样思路重写。
