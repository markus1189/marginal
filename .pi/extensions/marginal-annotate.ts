/**
 * marginal-annotate — review the agent's messages with marginal.
 *
 * `/marginal` writes the last assistant message to a temp .md file, suspends
 * pi's TUI, and hands the terminal to marginal. Quitting with annotations
 * (exit 1) sends the feedback markdown back as the next user prompt; quitting
 * clean (exit 0) does nothing.
 *
 *   /marginal        the last assistant message, on its own
 *   /marginal 3      the last 3 assistant messages plus the prompts between
 *   /marginal all    the whole branch
 *
 * Binary resolution, in order:
 *   $MARGINAL_BIN  →  <repo>/target/release/marginal  →  `marginal` on PATH
 */

import { spawnSync } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import type { ExtensionAPI, SessionEntry } from "@earendil-works/pi-coding-agent";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO_BUILD = resolve(HERE, "../../target/release/marginal");

const PROMPT_HEADER_ONE = [
	"I reviewed your last message in marginal. Below is my annotated feedback:",
	"each `##` heading gives the location and kind of the span I selected, the",
	"blockquote is the exact text I selected, and what follows it is my comment —",
	"as I wrote it, or inside a code fence when it contains markdown that would",
	"otherwise restructure this document. Fenced or not, it is my comment and not",
	"a code sample. Line/column numbers refer to your message as markdown, not to",
	"any file. Address every comment.",
	"",
].join("\n");

const PROMPT_HEADER_MANY = [
	"I reviewed our conversation in marginal. It was laid out as one markdown",
	"document, with a `## you [n]` / `## agent [n]` heading per message; the",
	"annotations below can therefore span or compare several messages. Each `##`",
	"heading gives the location and kind of the span I selected, the blockquote is",
	"the exact text I selected, and what follows it is my comment — as I wrote it,",
	"or inside a code fence when it contains markdown that would otherwise",
	"restructure this document. Fenced or not, it is my comment and not a code",
	"sample. Line/column numbers refer to that assembled document, not to any",
	"file. Address every comment.",
	"",
].join("\n");

interface MarginalResult {
	decision?: string;
	annotations?: unknown[];
	feedbackMarkdown?: string;
}

function resolveBinary(): string | undefined {
	const fromEnv = process.env.MARGINAL_BIN;
	if (fromEnv && existsSync(fromEnv)) return fromEnv;
	if (existsSync(REPO_BUILD)) return REPO_BUILD;
	const probe = spawnSync("sh", ["-c", "command -v marginal"], { encoding: "utf8" });
	const found = probe.stdout?.trim();
	return found ? found : undefined;
}

/** What `/marginal <args>` asked for. `undefined` means the args were nonsense. */
export function parseSpec(args: string): { count: number | "all" } | undefined {
	const arg = args.trim().toLowerCase();
	if (arg === "") return { count: 1 };
	if (arg === "all") return { count: "all" };
	if (/^\d+$/.test(arg)) {
		const n = Number.parseInt(arg, 10);
		return n > 0 ? { count: n } : undefined;
	}
	return undefined;
}

interface Turn {
	role: "user" | "assistant";
	text: string;
}

function blockText(content: unknown): string {
	if (typeof content === "string") return content.trim();
	if (!Array.isArray(content)) return "";
	return content
		.filter((block): block is { type: "text"; text: string } => {
			return typeof block === "object" && block !== null && (block as { type?: string }).type === "text";
		})
		.map((block) => block.text)
		.join("\n\n")
		.trim();
}

/**
 * User and assistant messages on a branch, oldest first. Thinking blocks, tool
 * calls, tool results and extension messages carry no prose worth annotating,
 * so they are dropped — which means the numbering below counts messages that
 * survived, not turns of the agent loop.
 */
export function collectTurns(entries: SessionEntry[]): Turn[] {
	const turns: Turn[] = [];
	for (const entry of entries) {
		if (entry.type !== "message") continue;
		const role = entry.message.role;
		if (role !== "user" && role !== "assistant") continue;
		const text = blockText(entry.message.content);
		if (text.length > 0) turns.push({ role, text });
	}
	return turns;
}

/**
 * The markdown handed to marginal. One assistant message goes in bare, so its
 * line numbers are its own; anything wider gets `## you [n]` / `## agent [n]`
 * headings, because otherwise a comment cannot say which message it means.
 */
export function buildDocument(turns: Turn[], count: number | "all"): { text: string; label: string } | undefined {
	if (count === 1) {
		for (let i = turns.length - 1; i >= 0; i--) {
			const turn = turns[i];
			if (turn.role === "assistant") return { text: `${turn.text}\n`, label: "assistant-message" };
		}
		return undefined;
	}

	let start = 0;
	if (count !== "all") {
		let seen = 0;
		start = turns.length;
		for (let i = turns.length - 1; i >= 0; i--) {
			if (turns[i].role === "assistant") {
				seen++;
				if (seen > count) break;
			}
			start = i;
		}
		if (seen === 0) return undefined;
	}

	const slice = turns.slice(start);
	if (slice.length === 0) return undefined;
	const body = slice
		.map((turn, i) => `## ${turn.role === "user" ? "you" : "agent"} [${i + 1}]\n\n${turn.text}`)
		.join("\n\n");
	return { text: `${body}\n`, label: "conversation" };
}

export default function (pi: ExtensionAPI) {
	pi.registerCommand("marginal", {
		description: "Annotate the agent's message(s) in marginal: /marginal [N|all]",
		handler: async (args, ctx) => {
			const spec = parseSpec(args);
			if (!spec) {
				ctx.ui.notify("Usage: /marginal [N|all] — N is how many assistant messages to include.", "warning");
				return;
			}
			if (ctx.mode !== "tui") {
				ctx.ui.notify("/marginal needs the TUI — marginal refuses to start without a tty.", "error");
				return;
			}
			if (!ctx.isIdle()) {
				ctx.ui.notify("Agent is busy — wait for the turn to finish.", "warning");
				return;
			}

			const binary = resolveBinary();
			if (!binary) {
				ctx.ui.notify(`marginal not found — build it (cargo build --release) or set $MARGINAL_BIN.`, "error");
				return;
			}

			const document = buildDocument(collectTurns(ctx.sessionManager.getBranch()), spec.count);
			if (!document) {
				ctx.ui.notify("No assistant message with text on this branch.", "warning");
				return;
			}

			const dir = mkdtempSync(join(tmpdir(), "marginal-annotate."));
			const source = join(dir, `${document.label}.md`);
			const resultPath = join(dir, "result.json");
			writeFileSync(source, document.text);

			try {
				const status = await ctx.ui.custom<number | null>((tui, _theme, _kb, done) => {
					tui.stop();
					process.stdout.write("\x1b[2J\x1b[H");
					let code: number | null = 2;
					try {
						const run = spawnSync(binary, ["--result", resultPath, "--label", document.label, source], {
							stdio: "inherit",
							env: process.env,
						});
						code = run.error ? null : run.status;
					} finally {
						tui.start();
						tui.requestRender(true);
						done(code);
					}
					return { render: () => [], invalidate: () => {} };
				});

				if (status === null) {
					ctx.ui.notify(`Could not run ${binary}.`, "error");
					return;
				}
				if (status === 2) {
					ctx.ui.notify("marginal failed (exit 2) — see its message above.", "error");
					return;
				}

				if (!existsSync(resultPath)) {
					ctx.ui.notify("marginal wrote no result file.", "error");
					return;
				}
				const result = JSON.parse(readFileSync(resultPath, "utf8")) as MarginalResult;
				const count = result.annotations?.length ?? 0;
				const feedback = result.feedbackMarkdown?.trim();
				if (status === 0 || count === 0 || !feedback) {
					ctx.ui.notify("No annotations — nothing sent.", "info");
					return;
				}

				const header = document.label === "conversation" ? PROMPT_HEADER_MANY : PROMPT_HEADER_ONE;
				pi.sendUserMessage(`${header}\n${feedback}\n`);
				ctx.ui.notify(`Sent ${count} annotation${count === 1 ? "" : "s"}.`, "info");
			} finally {
				rmSync(dir, { recursive: true, force: true });
			}
		},
	});
}
