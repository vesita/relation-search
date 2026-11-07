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
 * 每次检索前都会先跑一次增量 `index`（未变化的文件几乎瞬时），保证结果反映当前源码，
 * 而不是「同一进程只建一次索引、会话内改了代码仍读旧结果」。
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

export function apply(ctx: any): void {
  const codeReuseSearchTool = defineTool({
    name: 'relation_search',
    description:
      '在写新功能前检索本仓库里已有的相似实现（tantivy BM25 全文 + 代码分词 + 可选 ast-grep 结构匹配）。默认代码优先（文档/配置/生成物降权、每文件最多 2 块）；可选 kinds/include/exclude 做硬过滤。返回候选的文件:行、代码片段、文件类型 kind 与所在 symbol、命中来源，用于判断能否复用旧实现、避免重复造轮子。',
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
    },
    output: {
      schema: {
        type: 'object',
        additionalProperties: false,
        properties: {
          root: { type: 'string', required: true },
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
                snippet: { type: 'string', required: true },
                kind: { type: 'string', required: true },
                symbol: { type: 'string', required: true },
              },
            },
          },
        },
      },
      render: (args: any, value: any) => [{ type: 'text', text: renderText(args, value) }],
    },
    async execute(args: any, exec: any) {
      let root: string = args?.root
      if (!root) {
        if (exec?.agent && ctx?.workingDirectory) root = await ctx.workingDirectory.ensure(exec.agent, exec?.signal)
        else root = process.cwd()
      }
      root = resolve(root)
      const binary = resolveBinary()

      // 每次检索前做一次增量 index：只处理新增/修改/删除的文件（未变文件几乎瞬时），
      // 既保证 root 首次可用，也保证会话内改动的代码能被搜到。
      await runEngine(binary, ['index', '--root', root], exec?.signal)

      const cliArgs = ['search', '--root', root, '--query', String(args?.query ?? '')]
      if (args?.limit) cliArgs.push('--limit', String(args.limit))
      if (args?.astPattern) cliArgs.push('--ast-pattern', String(args.astPattern))
      if (args?.kinds) cliArgs.push('--kinds', String(args.kinds))
      if (args?.include) cliArgs.push('--include', String(args.include))
      if (args?.exclude) cliArgs.push('--exclude', String(args.exclude))

      const result = await runEngine(binary, cliArgs, exec?.signal)
      return {
        root: typeof result?.root === 'string' ? result.root : root,
        count: typeof result?.count === 'number' ? result.count : Array.isArray(result?.candidates) ? result.candidates.length : 0,
        candidates: Array.isArray(result?.candidates) ? result.candidates : [],
      }
    },
    presentCall(args: any) {
      return { card: 'generic', title: `检索旧实现：${String(args?.query ?? '')}`, kind: 'read', rawInput: args?.query }
    },
  })

  ctx.tools.register(codeReuseSearchTool)
}

export default { name, inject, apply }
