// node --test adapters/pi/test/ — the pi extension against a fake `pw-hook` shim.
import { afterEach, test } from "node:test";
import assert from "node:assert/strict";
import { chmodSync, mkdirSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { setTimeout as sleep } from "node:timers/promises";

const home = mkdtempSync(join(tmpdir(), "pw-pi-"));
const calls = join(home, "calls.txt");
const ITEM = { id: "c1", text: "[pair-watcher · Challenge c1 · from the Watcher, not the human]", token: "t1" };
mkdirSync(join(home, ".pair-watcher", "bin"), { recursive: true });
const shim = join(home, ".pair-watcher", "bin", "pw-hook");
writeFileSync(
  shim,
  // A slow `agent_settled` append: it lands 1.2 s after the event.
  `#!/bin/sh\n[ "$1" = agent_settled ] && sleep 1.2\necho "$*" >> '${calls}'\n[ "$2" = poll ] && printf '%s\\n' '${JSON.stringify({ status: null, commentary: [], item: ITEM })}'\nexit 0\n`,
);
chmodSync(shim, 0o700);
process.env.HOME = home;
process.env.PAIR_WATCHER_SESSION = "s1";
const { default: extension } = await import("../pair-watcher.ts");

// Each fake session's poll timer, stopped after its test.
const timers: ReturnType<typeof setInterval>[] = [];
const realSetInterval = globalThis.setInterval;
globalThis.setInterval = ((fn: () => void, ms: number) => {
  const t = realSetInterval(fn, ms);
  timers.push(t);
  return t;
}) as typeof setInterval;
afterEach(() => timers.splice(0).forEach(clearInterval));

function fakePi(idle: boolean) {
  const handlers = new Map<string, (e: any, ctx: any) => void>();
  const sent: [string, unknown][] = [];
  const pi = {
    on: (ev: string, fn: (e: any, ctx: any) => void) => handlers.set(ev, fn),
    sendUserMessage: (text: string, opts: unknown) => sent.push([text, opts]),
    events: { on: () => {} },
  };
  extension(pi);
  const ctx = { isIdle: () => idle, sessionManager: { getSessionId: () => "pi-1", getSessionFile: () => "/s.jsonl" } };
  return { emit: (ev: string) => handlers.get(ev)?.({}, ctx), sent };
}

const polls = () =>
  readFileSync(calls, "utf8")
    .split("\n")
    .filter((l) => l.startsWith("adapter poll"));

test("idle: the claimed item goes out as a followUp, then is acked (V27, I9)", async () => {
  writeFileSync(calls, "");
  const pi = fakePi(true);
  pi.emit("session_start");
  await sleep(1500);
  assert.deepEqual(pi.sent[0], [ITEM.text, { deliverAs: "followUp" }]);
  assert.ok(polls()[0].endsWith("--session s1 --route pi --worker idle"), polls()[0]);
  assert.ok(readFileSync(calls, "utf8").includes("adapter ack --session s1 --id c1 --token t1"));
});

test("a running agent is never sent to: worker=busy, nothing sent (V25)", async () => {
  writeFileSync(calls, "");
  const pi = fakePi(false);
  pi.emit("session_start");
  pi.emit("agent_start");
  await sleep(1500);
  assert.deepEqual(pi.sent, []);
  assert.ok(polls().length > 0 && polls().every((l) => l.endsWith("--worker busy")), polls().join("|"));
});

test("idle only after the agent_settled record has landed (V21)", async () => {
  writeFileSync(calls, "");
  const pi = fakePi(true);
  pi.emit("session_start");
  pi.emit("agent_start");
  pi.emit("agent_settled");
  await sleep(3000);
  const lines = readFileSync(calls, "utf8").split("\n");
  const settled = lines.findIndex((l) => l.startsWith("agent_settled"));
  const firstIdle = lines.findIndex((l) => l.endsWith("--worker idle"));
  assert.ok(settled >= 0 && firstIdle > settled, lines.join("|"));
});

test("under the Watcher guard nothing polls (V58)", async () => {
  writeFileSync(calls, "");
  process.env.PAIR_WATCHER_GUARD = "1";
  const pi = fakePi(true);
  pi.emit("session_start");
  await sleep(1500);
  delete process.env.PAIR_WATCHER_GUARD;
  assert.deepEqual(pi.sent, []);
  assert.deepEqual(polls(), []);
});
