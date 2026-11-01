import { Fragment, useState } from 'react'
import { Activity, ArrowRight, ChevronDown } from 'lucide-react'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card, CardHeader, CardTitle } from '@/components/ui/card'
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table'
import type { GatewaySnapshot } from '@/lib/types'

// 只解释核心已报告的阶段，不根据耗时猜测卡死或剩余时间。
const phases: Record<string, { title: string; detail: string }> = {
  receiving_request: { title: '正在接收请求', detail: '客户端还在把请求交给网关。' },
  waiting_headers: { title: '等待上游响应', detail: '已向上游服务发送请求，尚未收到响应。' },
  retry_waiting: { title: '等待重试', detail: '网关正在等待后再试，不是上游正在生成内容；下一次尝试尚未发出。' },
  reading_body: { title: '正在读取回复', detail: '上游已响应，网关正在接收回复内容。' },
  waiting_first_content: { title: '等待首段内容', detail: '上游已连接，还没有返回可交付的内容。' },
  stream_waiting: { title: '等待后续内容', detail: '已开始传回内容，正在等待后续输出。' },
  reasoning: { title: '正在推理', detail: '最近收到的是推理内容，正文可能还未开始。' },
  text_output: { title: '正在输出文本', detail: '最近收到的是正文内容，回复仍在进行。' },
  tool_output: { title: '正在输出工具调用', detail: '最近收到的是工具调用信息，不代表工具已执行完成。' },
}

function formatRequestDuration(ms: number | null | undefined): string {
  if (ms == null || !Number.isFinite(ms) || ms < 0) return '未知'
  const seconds = Math.floor(ms / 1000)
  if (seconds < 1) return '不足1秒'
  if (seconds < 60) return `${seconds}秒`
  if (seconds < 3600) return `${Math.floor(seconds / 60)}分${seconds % 60}秒`
  return `${Math.floor(seconds / 3600)}小时${Math.floor(seconds % 3600 / 60)}分`
}

export function RequestProgressCard({ snapshot, onOpenLogs }: { snapshot: GatewaySnapshot; onOpenLogs?: () => void }) {
  const [expanded, setExpanded] = useState<string | null>(null)
  const connected = ['running', 'external', 'draining'].includes(snapshot.state)
  const requests = connected ? snapshot.requests : null
  const count = connected ? snapshot.inflight : null
  const incomplete = snapshot.requestsTruncated || (count != null && count > (requests?.length ?? 0))
  return (
    <Card size="sm" className="min-w-0 gap-0 pb-0" role="region" aria-labelledby="request-progress-heading">
      <CardHeader className="pb-2">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <CardTitle role="heading" aria-level={2} id="request-progress-heading" className="flex items-center gap-2 text-sm">
            <Activity className="size-4 text-muted-foreground" aria-hidden="true" />请求进展
            <Badge variant="secondary">{count === null ? '未知' : `${count} 个进行中`}</Badge>
          </CardTitle>
          <div className="flex items-center gap-3">
            <span className="text-[11px] text-muted-foreground" title="约每 3 秒刷新；无进展不代表卡死，不预计完成时间。">3 秒刷新</span>
            {onOpenLogs && <Button variant="ghost" size="sm" className="h-7 text-xs" onClick={onOpenLogs}>全部日志<ArrowRight aria-hidden="true" /></Button>}
          </div>
        </div>
      </CardHeader>
      <div className="max-h-[340px] overflow-auto">
        <Table data-density="compact" aria-label="进行中的请求" className="min-w-[600px] text-xs">
          <TableHeader className="bg-muted/35">
            <TableRow>
              {['请求', '最近阶段', '已用时', '尝试', '距上次进展', ''].map((label, index) => <TableHead key={index} scope="col" className={`h-9 text-xs ${index === 0 ? 'pl-4' : ''} ${index >= 2 ? 'text-right' : ''}`}>{label || <span className="sr-only">说明</span>}</TableHead>)}
            </TableRow>
          </TableHeader>
          <TableBody>
            {requests && requests.length > 0 ? requests.map(request => {
              const phase = request.phase ? phases[request.phase] : null
              const detail = phase?.detail ?? `未知阶段：${request.phase ?? '未提供'}`
              const lastProgress = request.sinceLastProgressMs === undefined ? '未知' : request.sinceLastProgressMs === null ? '尚无' : formatRequestDuration(request.sinceLastProgressMs)
              const progressLabel = request.sinceLastProgressMs === undefined ? '内容进展时间未知' : request.sinceLastProgressMs === null ? '尚未收到内容进展' : `距上次内容进展 ${lastProgress}`
              const open = expanded === request.requestId
              return <Fragment key={request.requestId}>
                <TableRow>
                  <TableCell className="py-2.5 pl-4 font-mono text-muted-foreground" title={request.requestId}>{request.requestId.slice(0, 8)}</TableCell>
                  <TableCell className="py-2.5 font-medium" title={detail}>{phase?.title ?? request.phase ?? '阶段未知'}</TableCell>
                  <TableCell className="py-2.5 text-right tabular-nums">{formatRequestDuration(request.elapsedMs)}</TableCell>
                  <TableCell className="py-2.5 text-right tabular-nums" aria-label={request.attempt == null ? '尝试次数未知' : `第 ${request.attempt} 次尝试`}>{request.attempt ?? '—'}</TableCell>
                  <TableCell className="py-2.5 text-right tabular-nums text-muted-foreground" aria-label={progressLabel}>{lastProgress}</TableCell>
                  <TableCell className="py-1 pr-3 text-right"><Button variant="ghost" size="icon-xs" aria-label={`查看请求 ${request.requestId} 的说明`} aria-expanded={open} onClick={() => setExpanded(open ? null : request.requestId)}><ChevronDown className={open ? 'rotate-180' : ''} aria-hidden="true" /></Button></TableCell>
                </TableRow>
                {open && <TableRow className="bg-muted/20"><TableCell colSpan={6} className="whitespace-normal px-4 py-3 text-xs leading-5 text-muted-foreground">{detail}<span className="ml-2">{progressLabel}</span></TableCell></TableRow>}
              </Fragment>
            }) : <TableRow><TableCell colSpan={6} className="h-16 text-center text-muted-foreground">{!connected ? '网关未连接' : count === 0 ? '暂无进行中的请求' : '暂未提供请求详情'}</TableCell></TableRow>}
          </TableBody>
        </Table>
      </div>
      {requests && requests.length > 0 && incomplete && <p className="border-t px-4 py-2 text-[11px] text-muted-foreground">仅展示 {requests.length} 条请求详情</p>}
    </Card>
  )
}
