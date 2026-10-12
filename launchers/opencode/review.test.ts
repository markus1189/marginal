// `bun test launchers/opencode`. Bun because opencode is a Bun binary, so the
// plugin's imports resolve here as they do there. Suspending the TUI, spawning
// marginal and prompting the session need a live opencode with a tty.

import { describe, expect, test } from "bun:test";

import { buildDocument } from "./document.ts";
import { collectTurns, outcome, type Message } from "./review.ts";
import plugin from "./tui.ts";

const MESSAGES: Message[] = [
	{ type: "agent-switched" },
	{ type: "user", text: "first question" },
	{
		type: "assistant",
		content: [
			{ type: "reasoning", text: "hmm" },
			{ type: "text", text: "first answer" },
			{ type: "tool", text: "TOOL" },
		],
	},
	{ type: "assistant", content: [{ type: "tool" }] },
	{ type: "synthetic", text: "SYNTHETIC" } as Message,
	{ type: "shell", text: "SHELL" } as Message,
	{ type: "user", text: "  second question \n" },
	{
		type: "assistant",
		content: [
			{ type: "text", text: "second" },
			{ type: "text", text: "answer" },
		],
	},
];

describe("collectTurns", () => {
	test("keeps prompts and prose, drops tool-only steps and session entries", () => {
		expect(collectTurns(MESSAGES)).toEqual([
			{ role: "user", text: "first question" },
			{ role: "assistant", text: "first answer" },
			{ role: "user", text: "second question" },
			{ role: "assistant", text: "second\n\nanswer" },
		]);
	});

	test("feeds the shared document model", () => {
		const turns = collectTurns(MESSAGES);
		expect(buildDocument(turns, 1)).toEqual({ text: "second\n\nanswer\n", label: "assistant-message" });
		expect(buildDocument(turns, 2)?.text.startsWith("## you [1]\n\nfirst question\n\n## agent [2]")).toBe(true);
	});
});

const RESULT = (fields: object) => JSON.stringify({ annotations: [{}, {}], feedbackMarkdown: "## x\n\ncomment", ...fields });

describe("outcome", () => {
	test("exit 1 with annotations sends them", () => {
		const out = outcome(1, RESULT({ final: true }), "assistant-message", "m");
		expect(out.send?.startsWith("My annotations on your last message")).toBe(true);
		expect(out.send?.endsWith("## x\n\ncomment\n")).toBe(true);
		expect(out.toast).toEqual({ message: "Sent 2 annotations.", variant: "info" });
	});

	test("a clean quit sends nothing", () => {
		expect(outcome(0, RESULT({ annotations: [], final: true }), "conversation", "m").send).toBeUndefined();
	});

	test("an interrupted review is sent even after exit 2, under its notice", () => {
		const out = outcome(2, RESULT({ final: false }), "conversation", "m");
		expect(out.send?.startsWith("Note: this review was interrupted.")).toBe(true);
		expect(out.toast.variant).toBe("warning");
	});

	test("an empty interrupted review is not an approval", () => {
		const out = outcome(2, RESULT({ annotations: [], final: false }), "conversation", "m");
		expect(out.send).toBeUndefined();
		expect(out.toast.message).toContain("interrupted");
	});

	test("failures send nothing", () => {
		expect(outcome(null, undefined, "conversation", "/bin/m").toast.message).toBe("Could not run /bin/m.");
		expect(outcome(2, undefined, "conversation", "m").toast.message).toContain("held on screen");
		expect(outcome(1, "{trunc", "conversation", "m").toast.message).toContain("unreadable");
		expect(outcome(2, RESULT({ final: true }), "conversation", "m").send).toBeUndefined();
	});
});

test("registers one global command: slash, palette and key", () => {
	const layers: any[] = [];
	plugin.setup({ keymap: { layer: (factory: () => unknown) => layers.push(factory()) } } as any);
	// Not the default base mode, which is inactive while the prompt has focus.
	expect(layers[0].mode).toBe("global");
	const [command] = layers[0].commands;
	expect(command).toMatchObject({
		id: "marginal.annotate",
		bind: "<leader>m",
		palette: true,
		slash: { name: "marginal", arguments: true },
	});
});
