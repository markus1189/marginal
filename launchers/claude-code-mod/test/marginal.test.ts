import { describe, expect, test } from 'claude-code/testing'
import type { TestBody } from 'claude-code/testing'

import { buildDocument, collectTurns, parseSpec } from '../hooks/document'

const ROWS = [
  { role: 'user', text: 'first question' },
  { role: 'assistant', text: 'first answer' },
  { role: 'user', text: '<command-name>/marginal</command-name>' },
  { role: 'user', text: '<div>a real prompt</div>' },
  { role: 'assistant', text: '' },
  { role: 'assistant', text: 'second answer' },
] as const

describe('document', () => {
  test('parses the argument', () => {
    expect(parseSpec('')).toEqual({ count: 1 })
    expect(parseSpec(' ALL ')).toEqual({ count: 'all' })
    expect(parseSpec('3')).toEqual({ count: 3 })
    expect(parseSpec('0')).toBe(undefined)
    expect(parseSpec('x')).toBe(undefined)
  })

  test('drops plumbing and empty rows, keeps an html prompt', () => {
    expect(collectTurns(ROWS).map(t => t.text)).toEqual([
      'first question',
      'first answer',
      '<div>a real prompt</div>',
      'second answer',
    ])
  })

  test('one message bare, several under headings', () => {
    const turns = collectTurns(ROWS)
    expect(buildDocument(turns, 1)).toEqual({ text: 'second answer\n', label: 'assistant-message' })
    expect(buildDocument(turns, 2)?.text).toBe(
      '## you [1]\n\nfirst question\n\n## agent [2]\n\nfirst answer\n\n## you [3]\n\n<div>a real prompt</div>\n\n## agent [4]\n\nsecond answer\n',
    )
    expect(buildDocument([{ role: 'user', text: 'q' }], 1)).toBe(undefined)
  })
})

type Engine = Parameters<TestBody>[0]
type On = Parameters<TestBody>[1]

describe('/marginal', () => {
  const run = (stdout: string, stderr = '') =>
    async ($: Engine, on: On) => {
      const sent: string[] = []
      const toasts: string[] = []
      let settle!: () => void
      const done = new Promise<void>(resolve => (settle = resolve))
      let fed = ''

      on('session.messages', () => ({ value: ROWS.map(r => ({ ...r, toolUses: [] })) }))
      on('process.spawn', async function* (_$, e) {
        fed = e.input ?? ''
        if (stdout) yield { stream: 'stdout' as const, text: stdout }
        if (stderr) yield { stream: 'stderr' as const, text: stderr }
        return { value: { code: stdout ? 0 : 2, signal: null } }
      })
      on('prompt.submit', (_$, e) => {
        sent.push(e.text)
        return { text: e.text }
      })
      on('ui.toast', (_$, e) => {
        toasts.push(e.text)
        settle()
        return { value: undefined }
      })

      const shown = await $.command.run({
        command: 'marginal',
        args: '',
        origin: { kind: 'composer' },
        presentation: { isFullscreen: false, columns: 120 },
      })
      await done
      return { shown, sent, toasts, fed }
    }

  test('sends the feedback as the person', async ($, on) => {
    const result = JSON.stringify({ annotations: [{}, {}], feedbackMarkdown: '## L1\n\n> second\n\nwrong', final: true })
    const { shown, sent, toasts, fed } = await run(result)($, on)
    expect(shown.text).toContain('last message')
    expect(fed).toBe('second answer\n')
    expect(sent.length).toBe(1)
    expect(sent[0]).toContain('I reviewed your last message')
    expect(sent[0]).toContain('wrong')
    expect(toasts).toEqual(['Sent 2 annotations.'])
  })

  test('sends nothing for a clean quit', async ($, on) => {
    const { sent, toasts } = await run(JSON.stringify({ annotations: [], final: true }))($, on)
    expect(sent).toEqual([])
    expect(toasts).toEqual(['No annotations — nothing sent.'])
  })

  test('says why the launcher failed', async ($, on) => {
    const { sent, toasts } = await run('', 'no way to reach a terminal')($, on)
    expect(sent).toEqual([])
    expect(toasts[0]).toContain('no way to reach a terminal')
  })
})
