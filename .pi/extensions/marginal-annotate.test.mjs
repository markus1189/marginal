/**
 * Unit tests for the pure half of marginal-annotate: what goes into the
 * document marginal is handed. The half that matters and cannot be tested
 * here — suspending the host TUI, spawning the binary, sending the prompt —
 * needs a live pi session with a tty.
 *
 *   node --test .pi/extensions/marginal-annotate.test.mjs
 *
 * (`node --test .pi/extensions/` does not work: the runner skips dot-directories
 * and then tries to import the path as a module.)
 *
 * Node imports the .ts directly (type stripping, >= 22.18); there is no build
 * step and no dependency on pi's loader.
 */

import assert from "node:assert/strict";
import { test } from "node:test";
import { buildDocument, collectTurns, parseSpec } from "./marginal-annotate.ts";

const msg = (role, content) => ({ type: "message", id: "x", parentId: null, timestamp: "t", message: { role, content } });

const BRANCH = [
	msg("user", "first question"),
	msg("assistant", [
		{ type: "thinking", thinking: "hmm" },
		{ type: "text", text: "first answer" },
	]),
	msg("user", [{ type: "text", text: "second question" }]),
	msg("assistant", [
		{ type: "text", text: "tool preamble" },
		{ type: "toolCall", id: "1", name: "bash", arguments: {} },
	]),
	{
		type: "message",
		id: "y",
		parentId: null,
		timestamp: "t",
		message: { role: "toolResult", toolCallId: "1", toolName: "bash", content: [{ type: "text", text: "OUTPUT" }], isError: false },
	},
	{ type: "model_change", id: "z", parentId: null, timestamp: "t", provider: "p", modelId: "m" },
	msg("assistant", [{ type: "text", text: "second answer" }]),
];

test("parseSpec accepts nothing, a count, and all", () => {
	assert.deepEqual(parseSpec(""), { count: 1 });
	assert.deepEqual(parseSpec("  "), { count: 1 });
	assert.deepEqual(parseSpec(" ALL "), { count: "all" });
	assert.deepEqual(parseSpec("3"), { count: 3 });
});

test("parseSpec rejects what it cannot honour", () => {
	// Rejected rather than clamped: "/annotate 0" is a typo, and silently
	// showing one message would look like the flag was ignored.
	assert.equal(parseSpec("0"), undefined);
	assert.equal(parseSpec("last two"), undefined);
	assert.equal(parseSpec("-1"), undefined);
});

test("collectTurns keeps prose and drops everything else", () => {
	// Thinking blocks, tool calls, tool results and non-message entries carry
	// nothing a human would annotate, and a toolResult's role is neither
	// user nor assistant — a filter on role alone would let it through.
	assert.deepEqual(
		collectTurns(BRANCH).map((t) => `${t.role}:${t.text}`),
		["user:first question", "assistant:first answer", "user:second question", "assistant:tool preamble", "assistant:second answer"],
	);
});

test("collectTurns handles a string content body", () => {
	assert.deepEqual(collectTurns([msg("user", "plain string")]), [{ role: "user", text: "plain string" }]);
});

test("one message goes in bare, so its line numbers are its own", () => {
	assert.deepEqual(buildDocument(collectTurns(BRANCH), 1), { text: "second answer\n", label: "assistant-message" });
});

test("a wider document is headed per message", () => {
	// Counting is by assistant message; the user prompt in front of the first
	// one comes along, because a comment about an answer usually means the
	// question too.
	const doc = buildDocument(collectTurns(BRANCH), 2);
	assert.equal(doc.label, "conversation");
	assert.equal(doc.text, "## you [1]\n\nsecond question\n\n## agent [2]\n\ntool preamble\n\n## agent [3]\n\nsecond answer\n");
});

test("all reaches the first message, and an oversized count is the same thing", () => {
	const all = buildDocument(collectTurns(BRANCH), "all");
	assert.ok(all.text.startsWith("## you [1]\n\nfirst question"));
	assert.equal(buildDocument(collectTurns(BRANCH), 99).text, all.text);
});

test("nothing to annotate yields no document", () => {
	// The caller distinguishes this from a failure, so it must not throw or
	// hand marginal an empty file to open.
	assert.equal(buildDocument([], 1), undefined);
	assert.equal(buildDocument([], "all"), undefined);
	assert.equal(buildDocument(collectTurns([msg("user", "hi")]), 1), undefined);
	assert.equal(buildDocument(collectTurns([msg("user", "hi")]), 2), undefined);
});
