// 插件自检：对**构建产物** lib/index.js 跑，不依赖 dsh 运行时。
// 判据：注册出名为 relation_search 的工具，且对 fixtures/sample 检索能返回候选。
// 前置：cargo build --release 已产出引擎二进制（插件会自行解析到 ../../target/release）。
import assert from 'node:assert/strict'
import { resolve } from 'node:path'
import { apply } from './lib/index.js'

const registered = []
const ctx = {
  tools: { register: (tool) => registered.push(tool) },
  workingDirectory: { ensure: async () => process.cwd() },
}

apply(ctx)

assert.equal(registered.length, 1, '应注册且仅注册一个工具')
const tool = registered[0]
assert.equal(tool.name, 'relation_search', '工具名必须是 relation_search')

const root = resolve(process.cwd(), '..', 'fixtures', 'sample')
const value = await tool.execute(
  { query: '用户资料缓存 user profile cache', root },
  { agent: {}, signal: undefined },
)
console.log(JSON.stringify(value, null, 2))

assert.ok(value.count > 0, '应返回至少一个候选')
assert.ok(
  value.candidates.some((c) => String(c.path).includes('user_profile.rs')),
  '候选里应命中 src/user_profile.rs',
)

// 内嵌 ast-grep 结构召回：pattern 走 tree-sitter，无需外部 sg。
const astValue = await tool.execute(
  { query: 'zzz_nomatch_zzz', root, astPattern: 'def $F($$$): $$$' },
  { agent: {}, signal: undefined },
)
const astHit = astValue.candidates.find(
  (c) => c.source === 'ast' && String(c.path).includes('profile.py'),
)
assert.ok(astHit, 'ast 结构召回应命中 app/profile.py')
// 0.2.17 起 AST 命中带语法关系元数据；插件 output schema 必须容纳它们，
// 否则 additionalProperties:false 会让整次工具调用失败。
assert.equal(typeof astHit.astKind, 'string', 'ast 命中应带 astKind')
assert.equal(typeof astHit.astEnv, 'object', 'ast 命中应带 astEnv')
assert.ok('$F' in astHit.astEnv, 'astEnv 应捕获 pattern 元变量 $F 的绑定')
console.log('OK: 插件注册成功，BM25 与内嵌 ast 结构召回均返回候选')
