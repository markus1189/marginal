// The document model of marginal's pi extension (.pi/extensions/
// marginal-annotate.ts), fed from $.session.messages() instead of pi's branch.

export type Turn = { role: 'user' | 'assistant'; text: string }

/** What `/marginal <args>` asked for; `undefined` when the args are nonsense. */
export function parseSpec(args: string): { count: number | 'all' } | undefined {
  const arg = args.trim().toLowerCase()
  if (arg === '') return { count: 1 }
  if (arg === 'all') return { count: 'all' }
  if (/^\d+$/.test(arg)) {
    const n = Number.parseInt(arg, 10)
    return n > 0 ? { count: n } : undefined
  }
  return undefined
}

// Claude Code writes its own traffic as user messages; the launcher's jq
// filter (marginal-last) drops the same tags. A real prompt starting with
// `<div>` must survive, hence names rather than any `^<tag>`.
const PLUMBING =
  /^<(task-notification|bash-notification|bash-stdout|bash-stderr|local-command-stdout|local-command-stderr|local-command-caveat|system-reminder|command-name|command-message|command-args|bash-input|user-memory-input|background-task-input)>|^\[Request interrupted by user[^\]]*\]$/

// The engine's stand-ins for a reply the model never gave.
const SYNTHETIC = /^(No response requested\.|API Error: )/

export function collectTurns(messages: readonly { role: 'user' | 'assistant'; text: string }[]): Turn[] {
  const turns: Turn[] = []
  for (const { role, text: raw } of messages) {
    const text = raw.trim()
    if (text === '') continue
    if (role === 'user' && PLUMBING.test(text)) continue
    if (role === 'assistant' && SYNTHETIC.test(text)) continue
    turns.push({ role, text })
  }
  return turns
}

/**
 * One assistant message goes in bare, so its line numbers are its own; anything
 * wider gets `## you [n]` / `## agent [n]` headings, or a comment could not say
 * which message it means.
 */
export function buildDocument(
  turns: Turn[],
  count: number | 'all',
): { text: string; label: 'assistant-message' | 'conversation' } | undefined {
  if (count === 1) {
    const last = turns.findLast(turn => turn.role === 'assistant')
    return last && { text: `${last.text}\n`, label: 'assistant-message' }
  }

  let start = 0
  if (count !== 'all') {
    let seen = 0
    start = turns.length
    for (let i = turns.length - 1; i >= 0; i--) {
      if (turns[i].role === 'assistant' && ++seen > count) break
      start = i
    }
    if (seen === 0) return undefined
  }

  const slice = turns.slice(start)
  if (slice.length === 0) return undefined
  const body = slice
    .map((turn, i) => `## ${turn.role === 'user' ? 'you' : 'agent'} [${i + 1}]\n\n${turn.text}`)
    .join('\n\n')
  return { text: `${body}\n`, label: 'conversation' }
}

export const HEADER_ONE = `My annotations on your last message, from marginal. Under each \`##\` heading:
the exact text I selected as a blockquote, then my comment (fenced when it
holds markdown; still my comment, not a code sample). Line/column numbers
refer to your message, not to a file; \`· general\` means the whole message.
Address every comment.
`

export const HEADER_MANY = `My annotations on our conversation, from marginal. It was laid out as one
document with a \`## you [n]\` / \`## agent [n]\` heading per message, so a comment
may span several. Under each \`##\` heading: the exact text I selected as a
blockquote, then my comment (fenced when it holds markdown; still my comment,
not a code sample). Line/column numbers refer to that document, not to a file;
\`· general\` means the whole conversation. Address every comment.
`

export const INTERRUPTED = `Note: this review was interrupted. marginal ended before I quit it, so these
are the comments I had committed up to then: my own, but possibly not all of
them, and not a sign-off on the rest. Address them, then ask me whether I had
more to say.
`

export type MarginalResult = {
  annotations?: unknown[]
  feedbackMarkdown?: string
  /** false on autosaves and abnormal endings; absent from a quit-only marginal, so final. */
  final?: boolean
}

/** The prompt to send for a review, or why nothing is sent. */
export function verdict(
  result: MarginalResult,
  label: 'assistant-message' | 'conversation',
): { prompt: string; count: number; interrupted: boolean } | { none: string } {
  const count = result.annotations?.length ?? 0
  const feedback = result.feedbackMarkdown?.trim()
  const interrupted = result.final === false
  if (count === 0 || !feedback) {
    return {
      none: interrupted
        ? 'The review was interrupted before you quit marginal, with no annotations — nothing sent.'
        : 'No annotations — nothing sent.',
    }
  }
  const header = label === 'conversation' ? HEADER_MANY : HEADER_ONE
  const prompt = `${interrupted ? `${INTERRUPTED}\n` : ''}${header}\n${feedback}\n`
  return { prompt, count, interrupted }
}
