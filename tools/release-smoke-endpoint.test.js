import { createRequire } from 'node:module'
import { describe, expect, it, vi } from 'vitest'

const { readEndpoint } = createRequire(import.meta.url)('./release-smoke-endpoint.cjs')
const ready = '59582\r\n/devtools/browser/test-browser\r\n'

describe('native smoke endpoint readiness', () => {
  it('waits for the next change when the writer still owns the port file', () => {
    const read = vi.fn()
      .mockImplementationOnce(() => { throw Object.assign(new Error('locked'), { code: 'EBUSY' }) })
      .mockReturnValueOnce(ready)
    expect(readEndpoint('port-file', read)).toBeNull()
    expect(read).toHaveBeenCalledTimes(1)
    expect(readEndpoint('port-file', read)).toEqual({ port: 59582, browserPath: '/devtools/browser/test-browser' })
  })

  it('waits when creation has not yet completed', () => {
    expect(readEndpoint('port-file', () => { throw Object.assign(new Error('missing'), { code: 'ENOENT' }) })).toBeNull()
  })

  it.each(['', '59582\n', 'not-a-port\n/devtools/browser/test'])('does not accept partial endpoint data: %j', contents => {
    expect(readEndpoint('port-file', () => contents)).toBeNull()
  })

  it('propagates unrelated read failures so cleanup and failure reporting run', () => {
    const denied = Object.assign(new Error('access denied'), { code: 'EACCES' })
    expect(() => readEndpoint('port-file', () => { throw denied })).toThrow(denied)
  })
})
