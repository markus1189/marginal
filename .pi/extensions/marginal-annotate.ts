/**
 * marginal-annotate — review the agent's last message with marginal.
 *
 * `/annotate` writes the last assistant message to a temp .md file, suspends
 * pi's TUI, and hands the terminal to marginal. Quitting with annotations
 * (exit 1) sends the feedback markdown back as the next user prompt; quitting
 * clean (exit 0) does nothing.
 *
 * Binary resolution, in order:
 *   $MARGINAL_BIN  →  <repo>/target/release/marginal  →  `marginal` on PATH
 */

import { spawnSync } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import type { ExtensionAPI, ExtensionCommandContext } from "@earendil-works/pi-coding-agent";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO_BUILD = resolve(HERE, "../../target/release/marginal");

const PROMPT_HEADER = [
	"I reviewed your last message in marginal. Below is my annotated feedback:",
	"each `##` heading gives the location and kind of the span I selected, the",
	"blockquote is the exact text I selected, and the prose under it is my comment.",
	"Line/column numbers refer to your message as markdown, not to any file.",
	"Address every comment.",
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

/** Last assistant message on the current branch, as plain markdown text. */
function lastAssistantMarkdown(ctx: ExtensionCommandContext): string | undefined {
	const entries = ctx.sessionManager.getBranch();
	for (let i = entries.length - 1; i >= 0; i--) {
		const entry = entries[i];
		if (entry.type !== "message") continue;
		const message = entry.message;
		if (message.role !== "assistant") continue;
		const text = message.content
			.filter((block): block is { type: "text"; text: string } => block.type === "text")
			.map((block) => block.text)
			.join("\n\n")
			.trim();
		if (text.length > 0) return text;
	}
	return undefined;
}

export default function (pi: ExtensionAPI) {
	pi.registerCommand("annotate", {
		description: "Annotate the agent's last message in marginal, then send the feedback back",
		handler: async (_args, ctx) => {
			if (ctx.mode !== "tui") {
				ctx.ui.notify("/annotate needs the TUI — marginal refuses to start without a tty.", "error");
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

			const markdown = lastAssistantMarkdown(ctx);
			if (!markdown) {
				ctx.ui.notify("No assistant message with text on this branch.", "warning");
				return;
			}

			const dir = mkdtempSync(join(tmpdir(), "marginal-annotate."));
			const source = join(dir, "reply.md");
			const resultPath = join(dir, "result.json");
			writeFileSync(source, markdown.endsWith("\n") ? markdown : `${markdown}\n`);

			try {
				const status = await ctx.ui.custom<number | null>((tui, _theme, _kb, done) => {
					tui.stop();
					process.stdout.write("\x1b[2J\x1b[H");
					let code: number | null = 2;
					try {
						const run = spawnSync(binary, ["--result", resultPath, "--label", "assistant-message", source], {
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

				pi.sendUserMessage(`${PROMPT_HEADER}\n${feedback}\n`);
				ctx.ui.notify(`Sent ${count} annotation${count === 1 ? "" : "s"}.`, "info");
			} finally {
				rmSync(dir, { recursive: true, force: true });
			}
		},
	});
}
