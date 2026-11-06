// 插件自检：对**构建产物** lib/index.js 跑，不依赖 dsh 运行时。
// 判据：注册出名为 code_reuse_search 的工具，且对 fixtures/sample 检索能返回候选。
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
assert.equal(tool.name, 'code_reuse_search', '工具名必须是 code_reuse_search')

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
console.log('OK: 插件注册成功，检索返回候选')
