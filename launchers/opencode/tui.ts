/**
 * marginal for opencode 2 — review the agent's messages with marginal.
 *
 *   /marginal        the last assistant message, on its own
 *   /marginal 3      the last 3 assistant messages plus the prompts between
 *   /marginal all    the whole session
 *
 * Also on the palette and on `<leader>m`; cli.json's `keybinds` rebinds it
 * under the command id `marginal.annotate`. Quitting with annotations sends
 * the feedback as the next user prompt; quitting clean sends nothing.
 *
 * Binary resolution, in order:
 *   $MARGINAL_BIN  →  <repo>/target/release/marginal  →  the packaged binary
 *   →  `marginal` on PATH
 */

import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
// Type-only, so erased at load: nothing installs @opencode/plugin beside a
// copy linked in from the nix store.
import type { Context } from "@opencode/plugin/tui";

import { buildDocument, parseSpec } from "./document.ts";
import { collectTurns, outcome, type Message } from "./review.ts";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO_BUILD = resolve(HERE, "../../target/release/marginal");
// Rewritten by the flake's postInstall; see the pi extension for why it
// outranks PATH.
const PACKAGED_BIN = "@marginalBin@";

/** Exists and exits 0 on `--help`: a GC'd loader leaves a repo build that exists but cannot exec. */
export function binaryRuns(bin: string): boolean {
	if (!existsSync(bin)) return false;
	const probe = spawnSync(bin, ["--help"], { stdio: "ignore" });
	return !probe.error && probe.status === 0;
}

function resolveBinary(): string | undefined {
	const probe = spawnSync("sh", ["-c", "command -v marginal"], { encoding: "utf8" });
	const candidates = [process.env.MARGINAL_BIN, REPO_BUILD, PACKAGED_BIN, probe.stdout?.trim()];
	return candidates.find((bin): bin is string => !!bin && binaryRuns(bin));
}

function run(binary: string, args: string[]): Promise<number | null> {
	return new Promise((done) => {
		const child = spawn(binary, args, { stdio: "inherit" });
		child.on("error", () => done(null));
		child.on("exit", (code) => done(code));
	});
}

// Exit 2 after a session means marginal printed the review to the tty as its
// last copy; resume() repaints over it, so hold the screen first.
const HOLD =
	'printf "\\n[marginal exited 2 — anything it printed above, a rescued review included, is not saved anywhere. Press Enter to return to opencode.] "; read -r _ || true';

export default {
	id: "marginal",
	setup(context: Context) {
		// A second review would fight the first for the terminal.
		let reviewing = false;

		const toast = (message: string, variant: "info" | "warning" | "error") =>
			context.ui.toast.show({ message, variant });

		async function annotate(input?: string) {
			const spec = parseSpec(input ?? "");
			if (!spec) return toast("Usage: /marginal [N|all] — N is how many assistant messages to include.", "warning");
			if (reviewing) return toast("A marginal review is already open.", "warning");
			const route = context.ui.router.current();
			if (route.type !== "session") return toast("Open a session first.", "warning");
			const { sessionID } = route;
			if (context.data.session.status(sessionID) === "running")
				return toast("Agent is busy — wait for the turn to finish.", "warning");

			const binary = resolveBinary();
			if (!binary) return toast("marginal not found — build it (cargo build --release) or set $MARGINAL_BIN.", "error");

			reviewing = true;
			const dir = mkdtempSync(join(tmpdir(), "marginal-annotate."));
			try {
				await context.data.session.message.sync(sessionID);
				const messages = context.data.session.message.list(sessionID) as readonly Message[];
				const document = buildDocument(collectTurns(messages), spec.count);
				if (!document) return toast("No assistant message with text in this session.", "warning");

				const source = join(dir, `${document.label}.md`);
				const resultPath = join(dir, "result.json");
				writeFileSync(source, document.text);

				// opencode's own $EDITOR handoff, plus the screen clear an editor
				// does for itself and marginal does not: without it opencode's last
				// frame shows through every cell marginal leaves blank.
				const renderer = context.renderer;
				await renderer.suspend();
				renderer.currentRenderBuffer.clear();
				process.stdout.write("\x1b[2J\x1b[H");
				let status: number | null = null;
				try {
					status = await run(binary, ["--result", resultPath, "--label", document.label, source]);
					if (status === 2 && process.stdin.isTTY && !existsSync(resultPath)) await run("sh", ["-c", HOLD]);
				} finally {
					renderer.currentRenderBuffer.clear();
					await renderer.resume();
					renderer.requestRender();
				}

				const raw = existsSync(resultPath) ? readFileSync(resultPath, "utf8") : undefined;
				const out = outcome(status, raw, document.label, binary);
				if (out.send !== undefined) await context.client.session.prompt({ sessionID, text: out.send });
				toast(out.toast.message, out.toast.variant);
			} catch (err) {
				toast(`marginal failed: ${String(err)}`, "error");
			} finally {
				reviewing = false;
				rmSync(dir, { recursive: true, force: true });
			}
		}

		// Without "global" the layer sits in the base mode, which is not active
		// while the prompt has focus: no slash completion, no key. /btw does the same.
		context.keymap.layer(() => ({
			mode: "global",
			commands: [
				{
					id: "marginal.annotate",
					title: "Annotate in marginal",
					description: "Review the agent's last message(s) in marginal: /marginal [N|all]",
					group: "Session",
					bind: "<leader>m",
					palette: true,
					slash: { name: "marginal", arguments: true },
					run: annotate,
				},
			],
		}));
	},
};
