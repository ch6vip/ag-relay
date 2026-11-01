import { useEffect, useId, useState, useSyncExternalStore } from 'react'
import { PlusIcon, RotateCcwIcon, RotateCwIcon, ShieldAlertIcon, XIcon } from 'lucide-react'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select'
import { Switch } from '@/components/ui/switch'
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs'
import { AlertDialog, AlertDialogAction, AlertDialogCancel, AlertDialogContent, AlertDialogDescription, AlertDialogFooter, AlertDialogHeader, AlertDialogTitle } from '@/components/ui/alert-dialog'
import { moduleEnabled, moduleTiming, policyModules, type PolicyModule } from '@/lib/policy-catalog'
import { policyGroupNames, type PolicyDraftProfile, type PolicyEditor, type PolicyNumberField } from '@/lib/policy-settings'
import { STATUS_POLL_MS } from '@/lib/gateway'
import type { GatewayAction, GatewaySnapshot, PolicyGroup, PolicyMode } from '@/lib/types'

const cardClass = 'min-w-0 rounded-[24px] ring-border [--card-spacing:1.25rem]'
const hintClass = 'text-xs leading-5 text-muted-foreground'

export function PolicyPage({ editor, snapshot, onAction }: {
  editor: PolicyEditor
  snapshot: GatewaySnapshot
  onAction: (action: GatewayAction) => void
}) {
  const id = useId()
  const state = useSyncExternalStore(editor.subscribe, editor.getSnapshot, editor.getSnapshot)
  const { view, draft, group } = state
  const [libraryOpen, setLibraryOpen] = useState(false)
  const [earlyGroup, setEarlyGroup] = useState<PolicyGroup | null>(null)
  const gatewayBusy = snapshot.state === 'starting' || snapshot.state === 'draining'
  const running = snapshot.state === 'running' || snapshot.state === 'external'
  const disabled = state.working || gatewayBusy

  useEffect(() => {
    if (gatewayBusy) return
    void editor.refresh()
    const timer = setInterval(() => void editor.refresh(), STATUS_POLL_MS)
    return () => clearInterval(timer)
  }, [editor, gatewayBusy, snapshot.state])

  const profile = draft?.profiles[group]
  const modules = policyModules.filter(module => module.groups.includes(group))
  const enabled = profile ? modules.filter(module => moduleEnabled(module, profile)) : []
  const available = profile ? modules.filter(module => !moduleEnabled(module, profile)) : []
  const applied = Boolean(running && view?.activeSupported && view.active && view.activeRevision && !view.restartRequired)
  const statusCode = gatewayBusy ? 'busy' : !running ? 'stopped' : applied ? 'applied' : view?.activeSupported && view.active && view.activeRevision ? 'restart' : 'unknown'
  const statusText = statusCode === 'busy' ? '网关正在启动或排空，等待核实运行策略。'
    : statusCode === 'stopped' ? '网关未运行，已保存策略将在下次启动时应用。'
    : statusCode === 'applied' ? '已保存策略已生效，已核实运行中的策略快照。'
    : statusCode === 'restart' ? '已保存策略尚未应用，需安全重启网关。'
    : '运行策略未核实：旧核心或外部实例未提供可确认的策略快照；不能把保存成功当作已生效。'

  const changeMode = (mode: PolicyMode) => {
    if (mode === 'always' && profile?.historyMode !== 'always') setEarlyGroup(group)
    else editor.editProfile(group, { historyMode: mode })
  }
  const remove = (module: PolicyModule) => {
    if (module.id === 'historyMode') editor.editProfile(group, { historyMode: 'off' })
    else if (module.id === 'emptyResponseRepair') editor.editProfile(group, { emptyResponseRepair: false, emptyResponseDropReasoning: false })
    else editor.editProfile(group, { [module.id]: false })
  }
  const add = (module: PolicyModule) => {
    if (!view) return
    if (module.id === 'historyMode') changeMode(view.defaults.profiles[group].historyMode === 'always' ? 'always' : 'on_error')
    else if (module.id === 'emptyResponseRepair') editor.editProfile(group, { emptyResponseRepair: true, emptyResponseDropReasoning: view.defaults.profiles[group].emptyResponseDropReasoning })
    else editor.editProfile(group, { [module.id]: true })
  }
  const field = (key: PolicyNumberField, label: string, hint?: string) => <div className="min-w-0 space-y-2" key={key}>
    <Label htmlFor={`${id}-${group}-${key}`} className="text-xs">{label}</Label>
    <Input id={`${id}-${group}-${key}`} name={key} inputMode={key.endsWith('Ms') ? 'decimal' : 'numeric'} value={profile?.[key] ?? ''} disabled={disabled}
      onChange={event => editor.editProfile(group, { [key]: event.target.value } as Partial<PolicyDraftProfile>)}
      aria-describedby={`${id}-validation${hint ? ` ${id}-${key}-hint` : ''}`} className="tabular-nums" autoComplete="off" />
    {hint && <p id={`${id}-${key}-hint`} className={hintClass}>{hint}</p>}
  </div>

  const moduleDetails = (module: PolicyModule) => <details className="mt-2">
    <summary className="w-fit cursor-pointer rounded text-xs text-muted-foreground outline-none focus-visible:ring-2 focus-visible:ring-ring">字段、保留内容与限制</summary>
    <dl className="mt-2 grid gap-1.5 text-xs leading-5 text-muted-foreground">
      <div><dt className="inline font-medium text-foreground">触发与范围：</dt><dd className="inline">{module.trigger}</dd></div>
      <div><dt className="inline font-medium text-foreground">保留：</dt><dd className="inline">{module.preserves}</dd></div>
      {module.id === 'historyMode' && <div><dt className="inline font-medium text-foreground">本组规则：</dt><dd className="inline">{group === 'codex' ? '搜索关联的历史必须成组处理，并保留可验证的搜索载荷。每个请求最多一次成组改写，历史错误累计第 5 次停止；报错后改写的请求最多再发 3 次，仅做会话预清理而没有历史错误的请求按本组常规次数重试。' : '只使用通用 Responses 规则，不套用 Codex 专属的搜索处理。'}</dd></div>}
      <div><dt className="inline font-medium text-foreground">桌面默认：</dt><dd className="inline">{view ? moduleTiming(module, view.defaults.profiles[group]) : '等待读取'}</dd></div>
    </dl>
  </details>

  return <section aria-labelledby={`${id}-heading`} className="min-w-0 space-y-4 text-sm text-foreground">
    <header className="flex flex-wrap items-start justify-between gap-3">
      <div className="min-w-0 space-y-2">
        <h1 id={`${id}-heading`} className="text-xl font-semibold tracking-tight">重试与剥离策略</h1>
        <p className={hintClass}>决定遇错等多久、哪些旧字段可以省略。只修改转发副本，不删除本地聊天记录。</p>
      </div>
      <Button variant="outline" size="sm" disabled={!view || disabled} onClick={() => editor.reset()}><RotateCcwIcon />恢复全部策略默认</Button>
    </header>

    {!view || !profile ? <Card className={cardClass}><CardContent className="space-y-3">
      <p role={state.error ? 'alert' : 'status'} className={state.error ? 'text-destructive' : hintClass}>{state.error ?? '正在读取桌面策略与默认值…'}</p>
      {state.error && <Button variant="outline" disabled={state.loading} onClick={() => void editor.refresh()}>重新读取策略</Button>}
      <p className={hintClass}>没有读到配置前不会替你开启剥离。普通浏览器需使用隔离验收页面。</p>
    </CardContent></Card> : <>
      <Tabs value={group} onValueChange={value => { editor.selectGroup(value as PolicyGroup); setLibraryOpen(false) }} className="gap-4">
        <TabsList aria-label="策略适用分组" className="max-w-full">
          <TabsTrigger value="codex" className="px-4">Codex</TabsTrigger>
          <TabsTrigger value="general" className="px-4">Claude / Zcode</TabsTrigger>
        </TabsList>
        {(['codex', 'general'] as const).map(tab => <TabsContent key={tab} value={tab} className="space-y-4">
          {group === tab && <>
            <div className="space-y-2 px-1">
              <p className={hintClass}>{group === 'codex' ? '适用于识别为 Codex 的 Responses 请求。' : '适用于 Claude / Zcode 经 CCS 转为 Responses 后的通用请求。'} CCS 若未保留 Codex 识别信息，请求仍归通用组；这里不是 Messages 接口设置。</p>
              <div role="status" data-policy-status={state.error ? 'error' : statusCode} className="space-y-1 text-xs leading-5">
                <p>{state.error ? '暂时无法核实策略状态；上次读取的信息不能代表当前已生效。' : statusText}</p>
                {applied && !state.error && <p className="break-all text-muted-foreground">运行修订：{view.activeRevision}</p>}
                {state.dirty && <p className="font-medium">有未保存的草稿；切换页面和分组会保留，运行中的网关不受影响。</p>}
              </div>
            </div>

            <Card className={cardClass}>
              <CardHeader><CardTitle><h2>遇到限流等错误怎么重试</h2></CardTitle><CardDescription>常规尝试次数包含首次发送。等待越久越有机会绕过短暂故障，也可能让客户端等得更久。</CardDescription></CardHeader>
              <CardContent className="space-y-4">
                <div className="grid grid-cols-2 gap-4 xl:grid-cols-4">
                  {field('maxAttempts', '常规尝试次数', '1–1000 次，包含首次发送')}
                  {field('baseDelayMs', '起始等待（秒）')}
                  {field('maxDelayMs', '最长等待（秒）')}
                  {field('totalBudgetMs', '总时间预算（秒）')}
                  {field('connectTimeoutMs', '单次等待上限（秒）', '等待上游响应头的最长时间。请求体越大，核心会按体积再追加等待；不能大于总时间预算。')}
                </div>
                <details><summary className="w-fit cursor-pointer rounded text-xs text-muted-foreground focus-visible:ring-2 focus-visible:ring-ring">其他重试限制与安全边界</summary>
                  <div className={`mt-2 space-y-2 ${hintClass}`}>
                    <p>指数退避（每次等待逐渐加长）到最长等待，另含小幅随机延迟，不是固定请求频率。剩余预算不足 30 秒通常不再发起新尝试，不保证试满次数。</p>
                    <p>连接、部署异常和空回应会把尝试上限至少提高到 8 次，等待 0.8 秒；定向修复也可能追加少量尝试。因此常规次数不是绝对全局上限。</p>
                    <p>鉴权、余额或配额等确定失败不盲目重试；已经传回内容不重放，取消后不再发送。未覆盖错误保底不追加预算，清理后的候选请求最多实际发送 2 次。</p>
                  </div>
                </details>
              </CardContent>
            </Card>

            <Card className={cardClass}>
              <CardHeader><CardTitle><h2>历史错误怎么恢复</h2></CardTitle><CardDescription>仅累计明确的历史兼容错误：当前请求同类累计，不要求连续；不混入限流、空回应或网络错误。等待确认也会消耗尝试预算。未覆盖错误保底不使用此门槛，单独按原样确认一次处理。</CardDescription></CardHeader>
              <CardContent className="grid gap-3 sm:grid-cols-[minmax(0,16rem)_1fr] sm:items-center">
                {field('historyConfirmations', '执行历史降级前，明确错误达到（次）')}
                <p className={hintClass}>{group === 'codex' ? '可选 1–4 次。历史错误累计第 5 次停止；每请求只改写一次，报错后改写的请求最多再发 3 次，仅做会话预清理的请求按本组常规次数重试。' : '可选 1–1000 次，不能超过本组常规尝试次数。'}{profile.historyMode === 'off' ? ' 历史模块已关闭，此门槛暂不触发历史降级。' : profile.historyMode === 'always' ? ' 提前模式会先尝试安全处理，不等待此门槛。' : ''}</p>
              </CardContent>
            </Card>

            <Card className={cardClass}>
              <CardHeader className="flex flex-wrap items-center justify-between gap-3">
                <div className="space-y-1"><CardTitle><h2>已启用模块</h2></CardTitle><CardDescription>移除就是关闭；需要时可从模块库重新添加。</CardDescription></div>
                <Button variant="outline" size="sm" aria-expanded={libraryOpen} aria-controls={`${id}-library`} disabled={disabled} onClick={() => setLibraryOpen(value => !value)}><PlusIcon />添加模块{available.length > 0 ? `（${available.length}）` : ''}</Button>
              </CardHeader>
              <CardContent>
                <div className="divide-y divide-border">
                  {enabled.map(module => <article key={module.id} data-policy-module={module.id} className="min-w-0 space-y-2 py-4 first:pt-0 last:pb-0">
                    <div className="flex flex-wrap items-start justify-between gap-2">
                      <h3 className="min-w-0 font-medium leading-6">{module.title}<span className="ml-2 break-words text-xs font-normal text-muted-foreground">{module.fields}</span></h3>
                      <div className="flex flex-wrap items-center gap-2">
                        {module.id === 'historyMode' ? <Select value={profile.historyMode} disabled={disabled} onValueChange={value => changeMode(value as PolicyMode)}>
                          <SelectTrigger size="sm" aria-label="历史兼容降级处理时机"><SelectValue /></SelectTrigger>
                          <SelectContent><SelectItem value="off">关闭</SelectItem><SelectItem value="on_error">报错后</SelectItem><SelectItem value="always">提前处理</SelectItem></SelectContent>
                        </Select> : <span className="rounded-full bg-muted px-2.5 py-1 text-[11px] text-muted-foreground">{moduleTiming(module, profile)}</span>}
                        <Button variant="ghost" size="sm" disabled={disabled} aria-label={`移除${module.title}`} onClick={() => remove(module)}><XIcon className="size-3.5" />移除</Button>
                      </div>
                    </div>
                    {module.id === 'uncoveredErrorFallback' && <p className={hintClass}>{group === 'general' ? '本组仅 Claude 参与，ZCode 不参与。' : '仅明确识别为 Codex 的请求参与。'} 关闭历史模块或改为提前处理时只暂停保底，不清空此选择。</p>}
                    <p className={hintClass}>{module.purpose}</p>
                    <p className={hintClass}><span className="text-foreground">实际影响：</span>{module.impact}</p>
                    {moduleDetails(module)}
                    {module.id === 'emptyResponseRepair' && <div className="space-y-3 rounded-2xl bg-muted/50 p-3">
                      <div className="flex items-start justify-between gap-3">
                        <div className="space-y-1"><Label htmlFor={`${id}-stage2`}>第二阶段：允许省略旧 reasoning（推理历史）</Label><p className={hintClass}>第一阶段确实改过字段后才执行；关闭此项仍保留第一阶段。</p></div>
                        <Switch id={`${id}-stage2`} checked={profile.emptyResponseDropReasoning} disabled={disabled} onCheckedChange={value => editor.editProfile(group, { emptyResponseDropReasoning: value })} />
                      </div>
                      <div className="grid grid-cols-2 gap-3">{field('emptyResponseNormalizeAfter', '第一阶段：空回应达到（次）')}{field('emptyResponseReasoningAfter', '第二阶段：空回应达到（次）')}</div>
                    </div>}
                  </article>)}
                  {enabled.length === 0 && <p className={`${hintClass} py-3`}>本组没有启用的恢复模块。常规重试仍受上面的预算约束；可添加需要的模块。</p>}
                </div>
                {libraryOpen && <div id={`${id}-library`} className="mt-4 space-y-3 border-t border-border pt-4">
                  <h3 className="font-medium">可添加模块</h3>
                  {available.length === 0 ? <p className={hintClass}>本组支持的模块都已启用。移除后会回到这里。</p> : available.map(module => <article key={module.id} className="min-w-0 rounded-2xl border border-border p-3">
                    <div className="flex items-start justify-between gap-3"><div className="min-w-0"><h4 className="font-medium">{module.title}</h4><p className={`${hintClass} mt-1`}>{module.purpose}</p></div><Button variant="outline" size="sm" disabled={disabled} aria-label={`添加${module.title}`} onClick={() => add(module)}>添加</Button></div>
                    <p className={`${hintClass} mt-2`}>实际影响：{module.impact}</p>{moduleDetails(module)}
                  </article>)}
                </div>}
              </CardContent>
            </Card>

            <Card className={cardClass}>
              <CardHeader><CardTitle><h2>按当前草稿，会怎样处理</h2></CardTitle><CardDescription>这是所选策略的顺序说明，不是正在运行的请求记录。</CardDescription></CardHeader>
              <CardContent className="space-y-3">
                <ol className="list-decimal space-y-2 pl-5 text-xs leading-5 text-muted-foreground">
                  <li>{profile.historyMode === 'always' ? '首发先尝试整组历史兼容降级；不能安全改写就保留原始请求，并跳过相关预清理。' : `${profile.historyMode === 'on_error' && profile.rememberSession ? '未命中会话记忆时，' : ''}${profile.stripItemIds ? '首发只清理 Codex 可选编号，保留搜索依赖；尚不执行整组历史降级。' : '首发不执行额外历史剥离。'}`}</li>
                  <li>{profile.historyMode === 'off' ? '不执行历史兼容降级；会话记忆不能绕过此关闭项。' : profile.historyMode === 'always' ? '每次首发完整预处理，不读写会话记忆；历史错误确认次数不作为提前处理门槛。' : `明确历史错误达到 ${profile.historyConfirmations || '待填写'} 次，才进入错误后的历史恢复。${profile.rememberSession ? '已记住的同组会话可在首发提前复用兼容处理。' : '不读写会话兼容记忆。'}`}</li>
                  <li>{profile.uncoveredErrorFallback ? profile.historyMode === 'on_error' ? `${group === 'general' ? '本组仅 Claude 参与，ZCode 不参与。' : '仅明确识别的 Codex 参与。'}未覆盖错误先原样确认一次，连续两次合格错误后尝试一次有损清理；候选最多实际发送 2 次，受原次数与时间预算约束，不跨请求记忆。` : '未覆盖错误保底暂不参与；仅报错后模式参与，已保留开启选择。' : '未覆盖错误保底已关闭。'}</li>
                  <li>{profile.repairStatus ? '若上游明确点名 status 错误，只修复那个状态字段。' : '不进行定向 status 修复。'}{profile.retryHistory ? '无法安全剥离时可保留历史再试。' : ''}</li>
                  {group === 'general' && <li>{profile.emptyResponseRepair ? `空回应第 ${profile.emptyResponseNormalizeAfter || '待填写'} 次起整理 id/type；${profile.emptyResponseDropReasoning ? `第 ${profile.emptyResponseReasoningAfter || '待填写'} 次起，且第一阶段确实改过，才允许省略旧推理。` : '不进入删除旧推理的第二阶段。'}` : '不执行空回应分级兼容。'}真正限流错误不会触发这两级处理。</li>}
                </ol>
                <p className={hintClass}>不盲删 call_id（工具配对编号）、previous_response_id / conversation / item_reference（服务端历史引用）。含未知项、非推理密文或无法验证的配对时，不强行半改写。</p>
              </CardContent>
            </Card>
          </>}
        </TabsContent>)}
      </Tabs>

      <div className="space-y-3 rounded-[20px] border border-border bg-card p-4">
        <p id={`${id}-validation`} role={state.validation ? 'alert' : undefined} className={state.validation ? 'text-xs leading-5 text-destructive' : hintClass}>{state.validation ?? '保存只写入设置，不自动重启；恢复默认只填回草稿。安全重启只应用已保存的策略。'}</p>
        {state.error && <p role="alert" className="text-xs leading-5 text-destructive">{state.error}</p>}
        {state.notice && <p role="status" className="text-xs leading-5">{state.notice}</p>}
        <div className="flex flex-wrap items-center gap-2">
          <Button variant="outline" disabled={disabled} onClick={() => editor.reset(group)}>恢复本组默认</Button>
          <Button variant="ghost" disabled={disabled || !state.dirty} onClick={() => editor.discard()}>放弃修改</Button>
          <Button disabled={disabled || !state.dirty || Boolean(state.validation)} onClick={() => void editor.save()}>{state.working ? '正在保存…' : '保存策略'}</Button>
          <Button variant="outline" disabled={disabled || !running || state.dirty} onClick={() => onAction('restart')}><RotateCwIcon />安全重启并应用</Button>
        </div>
        {state.dirty && <p className={hintClass}>请先保存或放弃修改，再重启，避免把草稿误当作已应用。</p>}
      </div>
    </>}

    <AlertDialog open={earlyGroup !== null} onOpenChange={open => { if (!open) setEarlyGroup(null) }}>
      <AlertDialogContent>
        <AlertDialogHeader><ShieldAlertIcon className="mb-2 size-5 text-destructive" /><AlertDialogTitle>提前处理{earlyGroup ? policyGroupNames[earlyGroup] : ''}历史？</AlertDialogTitle>
          <AlertDialogDescription>没有报错也会尝试省略旧 reasoning（推理历史）与适用资源编号。可能失去旧推理连续性和资源关联，即使上游本来能接受这些内容。</AlertDialogDescription>
        </AlertDialogHeader>
        <p className={hintClass}>只修改本次转发副本，不删本地聊天，不关闭模型本轮思考。遇到服务端引用、未知项或不能验证的配对时整组保留，不做半剥离。确认只改变草稿，仍需保存与重启。</p>
        <AlertDialogFooter><AlertDialogCancel>取消，保留原时机</AlertDialogCancel><AlertDialogAction variant="destructive" onClick={() => { if (earlyGroup) editor.editProfile(earlyGroup, { historyMode: 'always' }); setEarlyGroup(null) }}>了解损失，填入草稿</AlertDialogAction></AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  </section>
}
