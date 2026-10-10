// pair-watcher pi adapter (I9): forwards wake events to the hook shim (I5, V58), delivers
// Challenges as follow-ups at idle (V27) and runs `/pw` (`watch`/`stop` this session, V112).
// Fail-silent; never blocks pi.
import { execFile, spawn } from "node:child_process";
import { homedir } from "node:os";
import { join } from "node:path";

const shim = join(homedir(), ".pair-watcher", "bin", "pw-hook");
const POLL_MS = 1000;

// Resolves once the shim exits, so the event is in the inbox before anything that relies on it.
function forward(event: string, ctx: any, extra: Record<string, unknown> = {}): Promise<void> {
  if (process.env.PAIR_WATCHER_GUARD) return Promise.resolve();
  const sm = ctx?.sessionManager;
  const payload = {
    hook_event_name: event,
    session_id: sm?.getSessionId?.(),
    session_file: sm?.getSessionFile?.(),
    ...extra,
  };
  return new Promise((resolve) => {
    try {
      const child = spawn(shim, [event], { stdio: ["pipe", "ignore", "ignore"] });
      child.on("error", () => resolve());
      child.on("close", () => resolve());
      child.stdin.on("error", () => {});
      child.stdin.end(JSON.stringify(payload));
    } catch {
      resolve();
    }
  });
}

function adapter(args: string[]): Promise<string> {
  return run(args).then((r) => (r.ok ? r.stdout : ""));
}

function run(args: string[]): Promise<{ ok: boolean; stdout: string; stderr: string }> {
  return new Promise((resolve) => {
    try {
      execFile(shim, ["adapter", ...args], { timeout: 10_000 }, (err, stdout, stderr) =>
        resolve({ ok: !err, stdout: String(stdout ?? ""), stderr: String(stderr ?? "") }),
      );
    } catch {
      resolve({ ok: false, stdout: "", stderr: "pair-watcher not installed" });
    }
  });
}

export default function (pi: any) {
  let last: any;
  let running = false;
  let polling = false;
  let timer: ReturnType<typeof setInterval> | undefined;
  const session = () => process.env.PAIR_WATCHER_SESSION ?? last?.sessionManager?.getSessionId?.();
  const idle = () => !running && last?.isIdle?.() !== false;

  // Never a steer, never mid-turn (V25): only an idle Worker gets an item, as a follow-up.
  async function tick() {
    const id = session();
    if (polling || !id || process.env.PAIR_WATCHER_GUARD) return;
    polling = true;
    try {
      const out = await adapter(["poll", "--session", id, "--route", "pi", "--worker", idle() ? "idle" : "busy"]);
      const item = out.trim() ? JSON.parse(out).item : null;
      if (!item) return;
      const claim = ["--session", id, "--id", item.id, "--token", item.token];
      // The agent started since the poll: hand the claim back for the next idle.
      if (!idle()) return void (await adapter(["release", ...claim]));
      pi.sendUserMessage(item.text, { deliverAs: "followUp" });
      await adapter(["ack", ...claim]);
    } catch {
    } finally {
      polling = false;
    }
  }

  pi.on("session_start", (_e: any, ctx: any) => {
    last = ctx;
    forward("session_start", ctx);
    // One poll timer per load, however often the session starts again.
    if (!timer && !process.env.PAIR_WATCHER_GUARD) timer = setInterval(tick, POLL_MS);
    timer?.unref?.();
  });
  // Bumped by each agent start: a settle whose append finishes after the next start is stale.
  let starts = 0;
  pi.on("agent_start", () => {
    running = true;
    starts++;
  });
  // Idle only once the turn end is in the inbox: until then the stale-check gate (V21) cannot
  // see it, and a poll would deliver ahead of the turn-end wake.
  pi.on("agent_settled", async (_e: any, ctx: any) => {
    const at = starts;
    await forward("agent_settled", ctx ?? last);
    if (starts === at) running = false;
  });
  // The answer is a notification for the human; nothing reaches the Worker.
  pi.registerCommand?.("pw", {
    description: "pair-watcher: watch, stop, rule <id> watcher|worker|dismiss, undo, keep, mute <focus>, nochallenge, pause, primary",
    handler: async (args: string, ctx: any) => {
      last = ctx ?? last;
      const id = session();
      if (!id) return;
      const words = args.trim().split(/\s+/).filter(Boolean);
      // A session with no file yet is watched from its harness and cwd (V113).
      const hint = words[0] === "watch" ? ["--harness", "pi", "--cwd", ctx?.cwd ?? process.cwd()] : [];
      const r = await run(["pw", "--session", id, ...hint, "--", ...words]);
      const text = (r.stdout.trim() || r.stderr.trim()).split("\n")[0];
      if (text) ctx?.ui?.notify?.(`pair-watcher: ${text}`, r.ok ? "info" : "warning");
    },
  });
  pi.events?.on?.("herdr:blocked", (data: any) => forward("herdr:blocked", last, { blocked: data }));
}
