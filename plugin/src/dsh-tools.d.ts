// 宿主 dsh 运行时提供的包，不写进 package.json（从 profile 的 node_modules 解析）。
// 这里只给 tsc 一份最小类型垫片，避免 TS2307；运行时由宿主那份实现。
declare module '@deepseek-ai/dsh-tools' {
  export function defineTool(options: any): any
}
