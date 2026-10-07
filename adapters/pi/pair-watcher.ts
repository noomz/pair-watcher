// pair-watcher pi adapter (I9): forwards wake events to the hook shim (I5, V58) and delivers
// Challenges as follow-ups at idle (V27). Fail-silent; never blocks pi.
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
  return new Promise((resolve) => {
    try {
      execFile(shim, ["adapter", ...args], { timeout: 10_000 }, (err, stdout) => resolve(err ? "" : String(stdout)));
    } catch {
      resolve("");
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
  pi.on("agent_start", () => {
    running = true;
  });
  // Idle only once the turn end is in the inbox: until then the stale-check gate (V21) cannot
  // see it, and a poll would deliver ahead of the turn-end wake.
  pi.on("agent_settled", async (_e: any, ctx: any) => {
    await forward("agent_settled", ctx ?? last);
    running = false;
  });
  pi.events?.on?.("herdr:blocked", (data: any) => forward("herdr:blocked", last, { blocked: data }));
}
