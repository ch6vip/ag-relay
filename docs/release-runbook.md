# 发布一个版本的完整流程

面向执行发布任务的 agent。用户只需说明要打包、上传草稿还是发布，不需要记住或手工运行下面的命令。构建、上传草稿和确认公开分开进行。
命令都在仓库根目录执行。相关设计见 [ADR-0005](adr/0005-whole-product-safe-updates.md)。

## Agent 执行约定

- 按下文完成版本、许可、更新说明、构建、上传和结果核对。现有 `gh` 登录、原签名和构建缓存先自行核实，不把查得到的信息反复交给用户填写。
- 只在用户授权范围内行动：仅要求打包时不顺带上传或公开；提交、推送和公开未获授权时，只询问必要决定。已明确授权的步骤由 agent 执行，不再要求用户自己点按钮或运行命令。
- 仍采用本机 Tauri/Windows 发布方式时，沿用 `release:build`、`release:upload`。普通代码改动不需要改命令；较大改动若影响依赖、资源、平台、安装器或签名约定，由 agent 同步维护相关配置、许可材料和既有脚本，并更新本手册，不能绕过检查或把适配责任交给用户。
- 验证范围和结果复用按 [核心维护说明的快速工作流](gateway-core-maintenance.md#快速工作流与定向验证)。进入提交、合并或发布时直接复用仍有效的定向验证，不另加全套业务测试或 GitHub CI。若已有入口不再适配，先修正入口再执行；没有真实构建、上传或公开，就分别报告未执行，不把测试通过当作已发布。

## 0. 先记住三件事

1. **签名私钥只有一把**，决定「用户能不能应用内更新」。丢了就再也推不了自动更新，只能让用户手动重装；泄露了别人就能伪造更新包推给你的用户。
   - 私钥与备份都放在**维护者本机、仓库之外**；不要提交或随包分发私钥、密码、`.env`。手册只说明通用目录约定，实际存放与备份位置由维护者保管。
   - 公钥已经写在 [src-tauri/tauri.conf.json](../src-tauri/tauri.conf.json) 的 `plugins.updater.pubkey`，公开无妨。
   - **绝不要**把私钥内容贴进聊天、Issue、提交或任何在线文档。
2. **正式签名构建在维护者本机跑**：私钥不进云端。用户明确要求在 GitHub 编译时，可运行手动编译工作流，仅生成未签名的便携产物，不创建安装器或 Release，不代替正式发布。
3. **已有 tag 不覆盖，已公开 Release 不修改**。草稿只允许补齐缺少的资产；发错了就发下一个版本号，不要删了重发——用户可能已经装上了。

## 1. 前提（一次性，已经配好）

| 项 | 值 |
| --- | --- |
| 仓库 | `xinxinxiaoxin/ag-relay`（必须公开，否则更新端点无法匿名读取） |
| 签名私钥 | 维护者本机、仓库之外的原生产密钥；**只在本机构建时用**，不经云端 |
| 云端流水线 | 无 GitHub Actions 发布工作流；`Windows Compile` 仅手动编译并保存 artifact，不使用签名 secret |

密钥最早是在维护者本机用 `npm run tauri -- signer generate -w <路径> --ci` 生成的。**换密钥等于换信任根**：旧版本内置的是旧公钥，认不了新私钥签的包，必须让用户手动装一次新版本才能继续自动更新。

## 日常提交与合并

- 先核对当前分支、未提交文件和并行会话状态。已有任务分支继续复用；从 `main` 开始开发则先切短分支，只纳入本次改动，不混入旁边的研究文档或其他任务。
- 日常小改默认本地合并到 `main` 后推送，不为合并专门开 PR；用户明确要求 PR 或确需独立评审的大型整合时再用 PR。
- 提交、合并、删除分支、推送、发布各按用户授权执行。“只提交”就留在本地分支；“不 push”就不推送，其余步骤也不能超出授权。获准合并时先核对远端与本地差异，确认没有其他会话正在使用该分支，成功合并后再按授权删分支、推送。
- 合并未改变已验证的相关输入时复用结果；若产生冲突或带入相关行为变更，按影响范围补测。同步在正式构建前完成，构建后保持输入不变。

## GitHub 上仅编译

用户明确要求云端编译时，使用 `.github/workflows/windows-build.yml`，只通过 `workflow_dispatch` 手动触发，不随 push 或 PR 自动运行。原仓库没有写权限时，将已审核的代码推到用户有权限的 fork，在该 fork 触发；不修改原仓库权限或发布状态。

工作流在 Windows 2022 上使用已记录许可的 Rust 1.99.1、Node 24.18.1 和锁文件安装依赖，沿用 `tauri build --no-bundle` 编译入口及许可准备器。构建保持原版本、更新公钥和安全设置，不读取或上传签名私钥，不生成更新清单、不签发安装包。

Actions artifact 保留 14 天，包含可直接解压使用的 `ag-relay/`（主程序、Node、网关与许可文件）、`third-party-sources.tar.gz`、`build-info.json` 和 `SHA256SUMS`。构建记录包含提交、时间、主程序及四个核心脚本和默认配置的 SHA-256；取得产物后核对摘要，不能把编译成功称为真机功能验证。正式发版仍走下方本机签名流程；便携包不属于可自动更新的签名发布包。

## 2. 正常发版（推荐路径）

日常用两个入口：先准备版本与许可，构建完成并推送 tag 后再上传。

```bash
npm run release:build
```

```bash
npm run release:upload
```

它们减少手工步骤，不承诺 Rust 编译加速；沿用现有 target 缓存与并发参数，不改 CI。下文 `<版本>` 均指 `src-tauri/tauri.conf.json` 的本次发布版本，执行命令时替换占位符。

### 少重复验证与构建

- 功能验证先用定向测试或前端预览；不要为了验证核心小改先构建整套安装包。用户还有待合入的改动时，先完成集成和版本准备，再开始正式构建。
- 同一批最终输入通常只做一轮正式签名构建。复用原 `CARGO_TARGET_DIR` 和既有构建参数，不用 `cargo clean`、临时换 target 或改变优化参数来尝试加速。测试与正式发布可能使用不同编译配置，不能把测试编译产物冒充发布物。
- 构建入口已包含前端构建和正式预检；到发布阶段不再手工追加一轮同样的前端构建或有效的业务回归。保留入口内部检查，不绕过签名、许可、安装器安全、资源与产物核验。
- 已有同输入的有效产物时直接验证并上传，不因上传或公开再编译；产物不完整／校验失败或构建输入发生变化时，仍需重新构建，不能复用旧包。

### 准备版本与许可

```bash
node scripts/bump-version.mjs <版本>
```

要一致的文件：`package.json`、`package-lock.json`（根 `version` 与 `packages[""].version`）、`src-tauri/Cargo.toml`、`src-tauri/Cargo.lock`（`ag-relay` 条目）、`src-tauri/tauri.conf.json`。任何一处落后都会以「产品版本不一致」拒绝打包。

许可校验对四个清单文件做指纹校验，所以改完版本号会报「依赖或二进制已变化」。**不能只改指纹蒙混过关**，必须先证明依赖集合真的没变：

对比 cargo 解析树与 npm 锁定树，确认与清单记录逐项一致：

```bash
cargo metadata --locked --filter-platform x86_64-pc-windows-msvc --format-version 1 --manifest-path src-tauri/Cargo.toml
```

无差异才刷新四个输入文件的指纹；有差异须按 THIRD_PARTY_NOTICES.md 重新取证，再复核：

```bash
node scripts/check-licenses.mjs --root . --rust-version <实测 rustc 版本> --installer-runtime
```

新依赖还要补许可原文与 `licenses/inventory.json` 记录，不能只刷新指纹。写好 `docs/releases/v<版本>.md`（UTF-8，面向用户的更新说明），将本次代码、版本与许可改动提交、合并到 main 并同步本地。**构建后不要再 `pull` 或改变构建输入；有变化就重新构建。** 发布前的源码、历史与资产敏感信息复核见[公开前复核报告](security/spec19-prepublication-audit.md)。

### 维护者本机构建（用私钥签名）

`npm run release:build` 自带正式预检：检查生产公钥、更新端点与降级策略、版本、安装器安全策略、随包资源和许可材料（含安装器运行时）；不通过就拒绝构建。保留 `requireSignedVersion=true`，不得启用不安全 TLS 或允许降级。

默认输出到 `release-artifacts/v<版本>`，更新说明默认读取 `docs/releases/v<版本>.md`。已有 `TAURI_SIGNING_PRIVATE_KEY` 环境变量时优先使用；否则自动读取当前用户目录下的 `.ag-relay-signing/ag-relay.key`，并核对相邻公钥与仓库生产配置一致。密码仍通过 `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` 提供。不读 `.env`、不回显私钥、不上传云端，无需手工导出私钥环境变量。

- 需要另选位置时用 `npm run release:build -- --out <新目录> --notes <UTF-8说明文件>`。**输出目录已存在就拒绝，不覆盖。**
- 构建自动准备安装用许可资源与同版 `third-party-sources.tar.gz`，失败即拒绝打包。共享同一个 target 的构建须串行排队，不覆盖现用程序。
- 输出目录应有 7 个文件：NSIS `.exe`、MSI `.msi`、各自 `.sig`、`latest.json`、`third-party-sources.tar.gz`、`SHA256SUMS`。

这里的签名用于应用内更新，不是 Windows Authenticode 代码签名，不能承诺“已验证发布者”或无 SmartScreen 警告。

### 推送本次构建的 tag

确认本次构建输入已提交并推送，在实际构建提交上打 tag：

```bash
git tag v<版本>
```

```bash
git push origin v<版本>
```

tag 用于追溯构建输入，**不触发云端构建**。脚本不会自动提交、打 tag 或推送。

### 上传草稿，确认后公开

`npm run release:upload` 默认读取当前版本的 `release-artifacts/v<版本>`，也可用 `npm run release:upload -- --artifacts <产物目录>` 指定。它先执行现有正式复核，再通过 `gh` 创建草稿并上传全部 7 个文件；创建时用 `--verify-tag` 要求远端 tag 已存在，不会代为创建。说明直接使用已打包的 `latest.json.notes`，无需再次指定说明文件。

已有草稿只补传缺少的文件；同名文件仅在远端 digest（文件摘要）与本地一致时跳过，否则拒绝，不覆盖。**已公开的 Release 拒绝修改；上传完成仍保持草稿。**

用户已授权公开时，由 agent 确认草稿的说明与资产无误后执行以下命令；尚未授权则仅确认是否公开，不要求用户自己操作 GitHub：

```bash
gh release edit v<版本> --repo xinxinxiaoxin/ag-relay --draft=false --latest
```

- 确认 `https://github.com/xinxinxiaoxin/ag-relay/releases/latest/download/latest.json` 的 `version` 已变成新版本，且可匿名读取。
- 在 README 的「打包快照」一节按现有格式追加一条（本仓构建提交、四个核心 `.mjs` 与 `bat` 的 sha256、成品 sha256、发布日期、Release 链接）。**没有实际构建就不要补记录，也不要改日期装成新产物。**

## 3. 出问题怎么办

| 现象 | 处理 |
| --- | --- |
| 报「产品版本不一致」 | 五处版本号没同步全，回到「准备版本与许可」 |
| 报「依赖或二进制已变化」 | 先证明依赖集合没变，再刷新指纹 |
| 报「缺少安装器运行时许可」/「正式分发许可来源未闭合」 | 许可材料或 `releaseBlockers` 的决定记录被动过，按 `THIRD_PARTY_NOTICES.md` 复核；不要删记录绕过 |
| 找不到签名私钥或公钥不匹配 | 检查环境变量或用户目录约定；找回原密钥，不另造密钥顶替 |
| 报「发布目录缺少文件或混入额外文件」/「发布文件校验和不匹配」 | 资产不齐或被改动，重新构建，不要手工增删文件 |
| 上传时远端 tag 不存在 | 确认 tag 指向实际构建提交，再手工推送 |
| 草稿中的同名文件摘要不匹配，或 Release 已公开 | 不覆盖远端文件、不修改公开版本；核对是否选错产物，需改版就用新版本号 |
| 构建很慢 | 瘦身用的 LTO 与单 codegen unit 会拉长编译；确认在复用同一 `CARGO_TARGET_DIR` 缓存，新入口不改变编译参数 |

## 4. 发完之后

真机验收（干净机器安装、旧的 ZIP 版升级、在途请求不被掐断、篡改包被拒等）按
[真实 Windows 成品验收清单](windows-acceptance-checklist.md) 在独立沙箱/VM 中逐条执行。
现用实例的安装、停机和重启需另有用户授权；发布成功不等于本机已升级。获准升级后核对应用版本、运行核心 `loadedVersion` 与连接设置／诊断记录保留；未获授权就停在发布与更新端点核对，不为验收打断现用网关。

## 5. 相关脚本

| 脚本 | 作用 |
| --- | --- |
| `scripts/release.mjs` | 预检、本机构建、草稿上传与独立复核（`preflight` / `build` / `upload` / `test-build` / `verify` / `audit-paths`） |
| `scripts/prepare-distribution.mjs` | 离线重建安装用许可资源与同版第三方源码附件；Tauri 打包前与 `build.rs` 自动执行，也可单独运行排查 |
| `scripts/bump-version.mjs` | 一次同步上面五处版本号；先全部校验再落盘，中途失败不会留下半套版本号（测试：`scripts/bump-version.test.mjs`） |
| `scripts/check-licenses.mjs` | 离线核验许可材料与输入指纹；依赖或二进制变化即拒绝 |
| `scripts/smoke.ps1` | 只读冒烟：对已有发布物验签，不安装、不启动 |

改完发布脚本后运行定向回归（不构建安装包、不访问 GitHub）：

```bash
node --test --test-concurrency=1 scripts/release-flow.test.mjs scripts/release.test.mjs scripts/check-licenses.test.mjs scripts/bump-version.test.mjs scripts/license-distribution.test.mjs
```
