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
import { chmodSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import register, { binaryRuns, buildDocument, collectTurns, parseSpec } from "./marginal-annotate.ts";

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
	// Rejected rather than clamped: "/marginal 0" is a typo, and silently
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

test("a binary is chosen only if it actually runs", () => {
	// The repo build whose nix loader was garbage-collected: executable, and
	// exec fails with ENOENT. A dead shebang interpreter fails the same way.
	const dir = mkdtempSync(join(tmpdir(), "marginal-annotate-test."));
	try {
		const script = (name, head, body) => {
			const path = join(dir, name);
			writeFileSync(path, `${head}\n${body}\n`);
			chmodSync(path, 0o755);
			return path;
		};
		const node = `#!${process.execPath}`;
		assert.equal(binaryRuns(script("ok", node, "process.exit(0)")), true);
		assert.equal(binaryRuns(script("stale", "#!/nix/store/0000000000000000000000000000000-gone/bin/ld.so", "")), false);
		assert.equal(binaryRuns(script("fails", node, "process.exit(3)")), false);
		assert.equal(binaryRuns(join(dir, "absent")), false);
	} finally {
		rmSync(dir, { recursive: true, force: true });
	}
});

/**
 * Run `/marginal` against a stand-in pi and a stand-in marginal: a node script
 * that answers --help, then runs `body` with `result` bound to the --result
 * path. Returns what the extension told the user and what it sent.
 */
async function runCommand(body) {
	const dir = mkdtempSync(join(tmpdir(), "marginal-annotate-cmd."));
	const bin = join(dir, "fake-marginal");
	writeFileSync(
		bin,
		`#!${process.execPath}\nconst a = process.argv;\nif (a.includes("--help")) process.exit(0);\nconst result = a[a.indexOf("--result") + 1];\nconst fs = require("node:fs");\n${body}\n`,
	);
	chmodSync(bin, 0o755);
	const saved = process.env.MARGINAL_BIN;
	process.env.MARGINAL_BIN = bin;
	let handler;
	const notes = [];
	const sent = [];
	const tui = { started: 0, stop() {}, start() { this.started++; }, requestRender() {} };
	register({
		registerCommand: (_name, spec) => { handler = spec.handler; },
		sendUserMessage: (text) => sent.push(text),
	});
	const ctx = {
		mode: "tui",
		isIdle: () => true,
		sessionManager: { getBranch: () => BRANCH },
		ui: {
			notify: (text, level) => notes.push(`${level}: ${text}`),
			custom: (fn) => new Promise((resolve) => { fn(tui, {}, {}, resolve); }),
		},
	};
	try {
		await handler("", ctx);
	} finally {
		if (saved === undefined) delete process.env.MARGINAL_BIN;
		else process.env.MARGINAL_BIN = saved;
		rmSync(dir, { recursive: true, force: true });
	}
	return { notes, sent, restarted: tui.started };
}

test("a result file that is not JSON is reported, not thrown", async () => {
	const r = await runCommand(`fs.writeFileSync(result, "{ truncated"); process.exit(1);`);
	assert.equal(r.sent.length, 0);
	assert.equal(r.notes.length, 1);
	assert.match(r.notes[0], /^error: marginal's result file is unreadable/);
	assert.equal(r.restarted, 1, "the host TUI is back either way");
});

test("exit 2 sends nothing and says the rescued review is the only copy", async () => {
	// The pause before pi repaints waits only on a tty, which this is not; what
	// is asserted is that nothing is sent and pi comes back.
	const r = await runCommand(`process.stdout.write("# Review feedback (rescued)\\n"); process.exit(2);`);
	assert.equal(r.sent.length, 0);
	assert.equal(r.notes.length, 1);
	assert.match(r.notes[0], /^error: marginal failed \(exit 2\).*only copy/);
	assert.equal(r.restarted, 1);
});

test("an interrupted review hands its annotations back, labelled", async () => {
	// What marginal leaves after a signal: its last autosave, final: false, exit 2.
	const r = await runCommand(
		`fs.writeFileSync(result, JSON.stringify({ final: false, annotations: [{}], feedbackMarkdown: "## x · paragraph\\n\\nhalf done" })); process.exit(2);`,
	);
	assert.equal(r.sent.length, 1);
	assert.match(r.sent[0], /^Note: this review was interrupted/);
	assert.match(r.sent[0], /half done/);
	assert.deepEqual(r.notes, ["warning: Sent 1 annotation from an interrupted review."]);
});

test("an interrupted review with nothing in it is not an approval", async () => {
	// An annotation added and removed again leaves final: false and zero
	// annotations, with `decision: "approved"` in the file.
	const r = await runCommand(
		`fs.writeFileSync(result, JSON.stringify({ final: false, decision: "approved", annotations: [], feedbackMarkdown: "" })); process.exit(2);`,
	);
	assert.equal(r.sent.length, 0);
	assert.equal(r.notes.length, 1);
	assert.match(r.notes[0], /^warning: The review was interrupted/);
});

test("annotations come back as the next prompt", async () => {
	const r = await runCommand(
		`fs.writeFileSync(result, JSON.stringify({ annotations: [{}], feedbackMarkdown: "## x · paragraph\\n\\nfix it" })); process.exit(1);`,
	);
	assert.equal(r.sent.length, 1);
	assert.match(r.sent[0], /fix it/);
	assert.deepEqual(r.notes, ["info: Sent 1 annotation."]);
});
