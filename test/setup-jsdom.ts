/**
 * jsdom 环境的通用 setup：静音 jsdom 自己往 stderr 打的 "Not implemented"。
 *
 * 注意这里必须用 `process.stderr.write` 之外的手段——jsdom 的 `notImplemented()`
 * 直接写 Node 的 stderr（`console.error` 拦不到），所以 hook 的是 stream。
 */
const NOT_IMPLEMENTED = /Not implemented:/

const originalWrite = process.stderr.write.bind(process.stderr)
process.stderr.write = ((chunk: any, ...rest: any[]) => {
  if (typeof chunk === 'string' && NOT_IMPLEMENTED.test(chunk))
    return true
  return originalWrite(chunk, ...rest)
}) as typeof process.stderr.write
