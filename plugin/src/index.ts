/**
 * dsh-code-search — 把 Rust 引擎（tantivy BM25 + jieba + 可选 ast-grep）包成一个
 * agent 可调用的工具 `code_search`。
 *
 * 它与用户级 skill `search-before-implement` 是**策略 + 能力**的一对：
 * skill 说"开发新功能前先检索旧实现、不复用要写原因"；本插件提供**自动召回**这个硬机制，
 * 让"检索"这一步不必靠模型记得手动 rg。
 *
 * 二进制解析顺序（现读，不写死逐机路径）：
 *   1. 环境变量 CODE_SEARCH_BIN；
 *   2. 包内 `bin/code-search`（发布 tarball 时随包带上）；
 *   3. 仓库开发态 `../../target/release/code-search`；
 *   4. 退回 PATH 里的 `code-search`。
 *
 * 首次对一个 root 检索时自动建索引（引擎的 `index` 子命令），之后复用缓存索引目录。
 * 索引目录由引擎按 `${CODE_SEARCH_CACHE_DIR:-$HOME/.cache/code-search}/<root 哈希>` 决定。
 */

import { spawn } from 'node:child_process'
import { existsSync } from 'node:fs'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { defineTool } from '@deepseek-ai/dsh-tools'

export const name = 'code-search'
export const inject = ['tools', 'workingDirectory']

/** 本文件编译后位于 <pkg>/lib/index.js，故 HERE = <pkg>/lib。 */
const HERE = dirname(fileURLToPath(import.meta.url))

/** 解析引擎二进制路径（见文件头注释的四级顺序）。 */
function resolveBinary(): string {
  const fromEnv = process.env.CODE_SEARCH_BIN
  if (fromEnv && existsSync(fromEnv)) return fromEnv
  const bundled = resolve(HERE, '..', 'bin', 'code-search')
  if (existsSync(bundled)) return bundled
  const dev = resolve(HERE, '..', '..', 'target', 'release', 'code-search')
  if (existsSync(dev)) return dev
  return 'code-search'
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
      reject(new Error(`无法启动 code-search（${binary}）：${error.message}。请先构建：cargo build --release`))
    })
    child.on('close', (code) => {
      if (signal) signal.removeEventListener('abort', onAbort)
      if (code !== 0) {
        reject(new Error(`code-search 退出码 ${String(code)}：${(err || out).trim().slice(0, 500)}`))
        return
      }
      const lastLine = out.trim().split('\n').filter(Boolean).pop() ?? ''
      try {
        resolvePromise(JSON.parse(lastLine))
      } catch {
        reject(new Error(`无法解析 code-search 输出：${lastLine.slice(0, 300)}`))
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
    lines.push('')
    lines.push(`[${index + 1}] ${String(c?.path)}:${String(c?.startLine)}-${String(c?.endLine)}  (score ${score} · ${String(c?.source)})`)
    lines.push('```')
    lines.push(String(c?.snippet ?? ''))
    lines.push('```')
  })
  lines.push('')
  lines.push('逐条判定：直接复用 / 改造复用 / 不复用（不复用写清具体差异，指到 文件:行）。')
  return lines.join('\n')
}

export function apply(ctx: any): void {
  /** 每个 root 只自动建一次索引（同一插件实例内）。 */
  const indexedRoots = new Set<string>()

  const codeReuseSearchTool = defineTool({
    name: 'code_search',
    description:
      '在写新功能前检索本仓库里已有的相似实现（tantivy BM25 全文 + 代码分词 + 可选 ast-grep 结构匹配）。返回候选的文件:行、代码片段与命中来源，用于判断能否复用旧实现、避免重复造轮子。',
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

      if (!indexedRoots.has(root)) {
        await runEngine(binary, ['index', '--root', root], exec?.signal)
        indexedRoots.add(root)
      }

      const cliArgs = ['search', '--root', root, '--query', String(args?.query ?? '')]
      if (args?.limit) cliArgs.push('--limit', String(args.limit))
      if (args?.astPattern) cliArgs.push('--ast-pattern', String(args.astPattern))

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
