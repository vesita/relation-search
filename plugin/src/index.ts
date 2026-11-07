/**
 * dsh-relation-search — 把 Rust 引擎（tantivy BM25 + jieba + 可选 ast-grep）包成一个
 * agent 可调用的工具 `relation_search`。
 *
 * 它与用户级 skill `search-before-implement` 是**策略 + 能力**的一对：
 * skill 说"开发新功能前先检索旧实现、不复用要写原因"；本插件提供**自动召回**这个硬机制，
 * 让"检索"这一步不必靠模型记得手动 rg。
 *
 * 二进制解析顺序（现读，不写死逐机路径）：
 *   1. 环境变量 RELATION_SEARCH_BIN；
 *   2. 包内 `bin/relation-search`（发布 tarball 时随包带上）；
 *   3. 仓库开发态 `../../target/release/relation-search`；
 *   4. 退回 PATH 里的 `relation-search`。
 *
 * 每次检索用一次 `search --refresh` 完成「增量索引 + 检索」（未变化的文件几乎瞬时），
 * 保证结果反映当前源码；不再为每次检索额外 spawn 一次 `index` 进程。
 * 索引目录由引擎按 `${RELATION_SEARCH_CACHE_DIR:-$HOME/.cache/relation-search}/<root 哈希>` 决定。
 */

import { spawn } from 'node:child_process'
import { existsSync } from 'node:fs'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { defineTool } from '@deepseek-ai/dsh-tools'

export const name = 'relation-search'
export const inject = ['tools', 'workingDirectory']

/** 本文件编译后位于 <pkg>/lib/index.js，故 HERE = <pkg>/lib。 */
const HERE = dirname(fileURLToPath(import.meta.url))

/** 解析引擎二进制路径（见文件头注释的四级顺序）。 */
function resolveBinary(): string {
  const fromEnv = process.env.RELATION_SEARCH_BIN
  if (fromEnv && existsSync(fromEnv)) return fromEnv
  const bundled = resolve(HERE, '..', 'bin', 'relation-search')
  if (existsSync(bundled)) return bundled
  const dev = resolve(HERE, '..', '..', 'target', 'release', 'relation-search')
  if (existsSync(dev)) return dev
  return 'relation-search'
}

/** 跑一次引擎子命令，解析其 stdout 最后一行 JSON。 */
function runEngine(binary: string, args: string[], signal?: AbortSignal): Promise<any> {
  return new Promise((resolvePromise, reject) => {
    const child = spawn(binary, args, { stdio: ['ignore', 'pipe', 'pipe'] })
    let out = ''
    let err = ''
    const onAbort = () => child.kill('SIGKILL')
    if (signal) signal.addEventListener('abort', onAbort, { once: true })
    child.stdout.on('data', (chunk) => (out += String(chunk)))
    child.stderr.on('data', (chunk) => (err += String(chunk)))
    child.on('error', (error: NodeJS.ErrnoException) => {
      if (signal) signal.removeEventListener('abort', onAbort)
      reject(new Error(`无法启动 relation-search（${binary}）：${error.message}。请先构建：cargo build --release`))
    })
    child.on('close', (code) => {
      if (signal) signal.removeEventListener('abort', onAbort)
      if (code !== 0) {
        reject(new Error(`relation-search 退出码 ${String(code)}：${(err || out).trim().slice(0, 500)}`))
        return
      }
      const lastLine = out.trim().split('\n').filter(Boolean).pop() ?? ''
      try {
        resolvePromise(JSON.parse(lastLine))
      } catch {
        reject(new Error(`无法解析 relation-search 输出：${lastLine.slice(0, 300)}`))
      }
    })
  })
}

/** 把工具结果渲染成模型可读的文本。 */
function renderText(args: any, value: any): string {
  const count = Number(value?.count ?? 0)
  const lines: string[] = [`复用检索：${String(args?.query ?? '')}（${count} 个候选，root=${String(value?.root ?? '')}）`]
  if (count === 0) {
    lines.push('未找到相似旧实现。若确为新功能，按 search-before-implement 在决策记录里写明"无可用旧实现"。')
    return lines.join('\n')
  }
  const candidates: any[] = Array.isArray(value?.candidates) ? value.candidates : []
  candidates.forEach((c, index) => {
    const score = typeof c?.score === 'number' ? c.score.toFixed(2) : String(c?.score ?? '')
    const kind = String(c?.kind ?? '')
    const symbol = String(c?.symbol ?? '')
    const meta = [kind, symbol, `score ${score}`, String(c?.source)].filter(Boolean)
    lines.push('')
    lines.push(`[${index + 1}] ${String(c?.path)}:${String(c?.startLine)}-${String(c?.endLine)}  (${meta.join(' · ')})`)
    lines.push('```')
    lines.push(String(c?.snippet ?? ''))
    lines.push('```')
  })
  lines.push('')
  lines.push('逐条判定：直接复用 / 改造复用 / 不复用（不复用写清具体差异，指到 文件:行）。')
  return lines.join('\n')
}

/** 按文件聚合候选：命中块数、最佳分、涉及 kind。 */
function aggregate(candidates: any[]): Map<string, { hits: number; best: number; kinds: Set<string> }> {
  const byFile = new Map<string, { hits: number; best: number; kinds: Set<string> }>()
  for (const c of candidates) {
    const path = String(c?.path ?? '')
    const prev = byFile.get(path) ?? { hits: 0, best: 0, kinds: new Set<string>() }
    prev.hits += 1
    prev.best = Math.max(prev.best, Number(c?.score ?? 0))
    prev.kinds.add(String(c?.kind ?? ''))
    byFile.set(path, prev)
  }
  return byFile
}

/** 默认渲染：只回扫描摘要，不把代码片段塞进上下文。 */
function renderSummary(args: any, value: any): string {
  const count = Number(value?.count ?? 0)
  const documents = Number(value?.documents ?? 0)
  const root = String(value?.root ?? '')
  const head = `复用检索：${String(args?.query ?? '')}（扫描 ${documents > 0 ? documents : '?'} 块，命中 ${count} 块`
  if (count === 0) {
    return `${head} / 0 个文件，root=${root}）\n未找到相似旧实现。若确为新功能，按 search-before-implement 在决策记录里写明"无可用旧实现"。`
  }
  const candidates: any[] = Array.isArray(value?.candidates) ? value.candidates : []
  const byFile = aggregate(candidates)
  const kinds = new Map<string, number>()
  for (const c of candidates) {
    const kind = String(c?.kind ?? '')
    kinds.set(kind, (kinds.get(kind) ?? 0) + 1)
  }
  const lines = [`${head} / ${byFile.size} 个文件，root=${root}）`]
  lines.push(`kind：${[...kinds.entries()].map(([k, n]) => `${k} ${n}`).join(' · ')}`)
  lines.push('')
  for (const [path, info] of byFile) {
    lines.push(`  ${path}  ${info.hits} 块 · ${[...info.kinds].join('/')} · 最高 ${info.best.toFixed(2)}`)
  }
  lines.push('')
  candidates.forEach((c, index) => {
    const score = typeof c?.score === 'number' ? c.score.toFixed(2) : String(c?.score ?? '')
    const meta = [String(c?.kind ?? ''), String(c?.symbol ?? ''), `score ${score}`].filter(Boolean).join(' · ')
    lines.push(`[${index + 1}] ${String(c?.path)}:${String(c?.startLine)}-${String(c?.endLine)}  (${meta})`)
  })
  lines.push('')
  lines.push('需要看代码：read <path> offset=<startLine> limit=<行数>（默认只回摘要；直接要片段把 detail 设为 true）。')
  return lines.join('\n')
}

export function apply(ctx: any): void {
  const codeReuseSearchTool = defineTool({
    name: 'relation_search',
    description:
      '在写新功能前检索本仓库里已有的相似实现（tantivy BM25 + 代码分词 + 可选 ast-grep 结构匹配）。默认只回扫描摘要：命中块数/文件数、按文件聚合、每条路径:行号 + kind/symbol/score，不含代码片段。需要看代码时用 read 打开 path/行号；要一次拿到片段就把 detail 设为 true。默认代码优先（文档/配置/生成物降权、每文件最多 2 块）；可选 kinds/include/exclude 做硬过滤。',
    parameters: {
      query: {
        type: 'string',
        required: true,
        description: '你想实现的功能 / 标识符 / 中文描述，多个关键词用空格分开。',
      },
      root: {
        type: 'string',
        description: '要检索的仓库根目录；默认当前会话工作目录。',
      },
      limit: {
        type: 'number',
        description: '返回候选数量，默认 8。',
      },
      astPattern: {
        type: 'string',
        description: '可选 ast-grep 结构模式（如 "function $F($$$) { $$$ }"），额外做一次结构召回。',
      },
      kinds: {
        type: 'string',
        description: '可选：硬过滤，只保留这些类型（逗号分隔：code/test/doc/config/generated/other）。',
      },
      include: {
        type: 'string',
        description: '可选：硬过滤，只保留命中这些路径 glob 之一的文件（逗号分隔，* 跨目录，如 *.rs）。',
      },
      exclude: {
        type: 'string',
        description: '可选：硬过滤，命中这些路径 glob 任一即排除（逗号分隔）。',
      },
      detail: {
        type: 'boolean',
        description: '默认 false：只回扫描摘要（路径:行号 + kind/symbol/score，不含代码片段）。true：同时回每条的 snippet。',
      },
    },
    output: {
      schema: {
        type: 'object',
        additionalProperties: false,
        properties: {
          root: { type: 'string', required: true },
          documents: { type: 'number' },
          count: { type: 'number', required: true },
          candidates: {
            type: 'array',
            required: true,
            items: {
              type: 'object',
              additionalProperties: false,
              properties: {
                path: { type: 'string', required: true },
                startLine: { type: 'number', required: true },
                endLine: { type: 'number', required: true },
                score: { type: 'number', required: true },
                source: { type: 'string', required: true },
                snippet: { type: 'string' },
                kind: { type: 'string', required: true },
                symbol: { type: 'string', required: true },
              },
            },
          },
        },
      },
      render: (args: any, value: any) => [
        { type: 'text', text: args?.detail ? renderText(args, value) : renderSummary(args, value) },
      ],
    },
    async execute(args: any, exec: any) {
      let root: string = args?.root
      if (!root) {
        if (exec?.agent && ctx?.workingDirectory) root = await ctx.workingDirectory.ensure(exec.agent, exec?.signal)
        else root = process.cwd()
      }
      root = resolve(root)
      const binary = resolveBinary()

      // 一次进程完成「增量索引 + 检索」：`--refresh` 让引擎先跑增量 index，
      // 既保证 root 首次可用与会话内改动可见，又把刷新策略留在引擎一处。
      const detail = Boolean(args?.detail)
      const cliArgs = ['search', '--root', root, '--query', String(args?.query ?? ''), '--refresh']
      // 默认只回摘要：引擎跳过 snippet 生成，工具结果也不带代码片段（省上下文）。
      if (!detail) cliArgs.push('--summary')
      if (args?.limit) cliArgs.push('--limit', String(args.limit))
      if (args?.astPattern) cliArgs.push('--ast-pattern', String(args.astPattern))
      if (args?.kinds) cliArgs.push('--kinds', String(args.kinds))
      if (args?.include) cliArgs.push('--include', String(args.include))
      if (args?.exclude) cliArgs.push('--exclude', String(args.exclude))

      const result = await runEngine(binary, cliArgs, exec?.signal)
      const rawCandidates: any[] = Array.isArray(result?.candidates) ? result.candidates : []
      const candidates = detail
        ? rawCandidates
        : rawCandidates.map((c: any) => {
            const copy = { ...c }
            delete copy.snippet
            return copy
          })
      return {
        root: typeof result?.root === 'string' ? result.root : root,
        documents: typeof result?.documents === 'number' ? result.documents : 0,
        count: typeof result?.count === 'number' ? result.count : rawCandidates.length,
        candidates,
      }
    },
    presentCall(args: any) {
      return { card: 'generic', title: `检索旧实现：${String(args?.query ?? '')}`, kind: 'read', rawInput: args?.query }
    },
  })

  ctx.tools.register(codeReuseSearchTool)
}

export default { name, inject, apply }
