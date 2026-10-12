// The opencode-specific, pure half of the plugin: what goes into marginal and
// what comes back out. tui.ts holds the half that needs a live TUI.

import { verdict, type MarginalResult, type Turn } from "./document.ts";

/** The fields of @opencode/client's SessionMessageInfo that this reads. */
export type Message =
	| { type: "user"; text: string }
	| { type: "assistant"; content: readonly { type: string; text?: string }[] }
	| { type: string };

/**
 * Typed prompts and the model's prose, oldest first. Reasoning, tool parts,
 * and the session's synthetic, system, skill, shell and compaction entries are
 * dropped, so an assistant step that only called tools vanishes and the
 * numbering counts what survived — as in pi.
 */
export function collectTurns(messages: readonly Message[]): Turn[] {
	const turns: Turn[] = [];
	for (const message of messages) {
		let text = "";
		if (message.type === "user" && "text" in message) text = message.text;
		else if (message.type === "assistant" && "content" in message)
			text = message.content
				.filter((part) => part.type === "text" && typeof part.text === "string")
				.map((part) => part.text)
				.join("\n\n");
		else continue;
		text = text.trim();
		if (text !== "") turns.push({ role: message.type as Turn["role"], text });
	}
	return turns;
}

export type Toast = { message: string; variant: "info" | "warning" | "error" };

/**
 * What a finished review amounts to. `status` is marginal's exit code (`null`
 * when it never ran), `raw` the result file's contents (`undefined` when there
 * is none). The order is pi's: an interrupted review's comments are sent even
 * when marginal exited 2, because they are the human's committed work.
 */
export function outcome(
	status: number | null,
	raw: string | undefined,
	label: "assistant-message" | "conversation",
	binary: string,
): { send?: string; toast: Toast } {
	if (status === null) return { toast: { message: `Could not run ${binary}.`, variant: "error" } };
	if (raw === undefined) {
		return {
			toast: {
				message:
					status === 2
						? "marginal failed (exit 2) — nothing was sent. Its output was held on screen before opencode resumed; a review it rescued there is the only copy."
						: "marginal wrote no result file.",
				variant: "error",
			},
		};
	}
	let result: MarginalResult;
	try {
		result = JSON.parse(raw) as MarginalResult;
	} catch (err) {
		return { toast: { message: `marginal's result file is unreadable (${String(err)}) — nothing sent.`, variant: "error" } };
	}
	if (result.final !== false) {
		if (status === 2) return { toast: { message: "marginal failed (exit 2) — nothing was sent.", variant: "error" } };
		if (status === 0) return { toast: { message: "No annotations — nothing sent.", variant: "info" } };
	}
	const out = verdict(result, label);
	if ("none" in out) return { toast: { message: out.none, variant: result.final === false ? "warning" : "info" } };
	const plural = out.count === 1 ? "" : "s";
	return out.interrupted
		? { send: out.prompt, toast: { message: `Sent ${out.count} annotation${plural} from an interrupted review.`, variant: "warning" } }
		: { send: out.prompt, toast: { message: `Sent ${out.count} annotation${plural}.`, variant: "info" } };
}
