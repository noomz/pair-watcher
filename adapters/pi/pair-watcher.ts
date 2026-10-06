// pair-watcher pi adapter (I9): forwards wake events to the hook shim (I5, V58).
// Fail-silent; never blocks pi. Delivery (followUp) arrives with T9.
import { spawn } from "node:child_process";
import { homedir } from "node:os";
import { join } from "node:path";

const shim = join(homedir(), ".pair-watcher", "bin", "pw-hook");

function forward(event: string, ctx: any, extra: Record<string, unknown> = {}) {
  if (process.env.PAIR_WATCHER_GUARD) return;
  const sm = ctx?.sessionManager;
  const payload = {
    hook_event_name: event,
    session_id: sm?.getSessionId?.(),
    session_file: sm?.getSessionFile?.(),
    ...extra,
  };
  try {
    const child = spawn(shim, [event], { stdio: ["pipe", "ignore", "ignore"] });
    child.on("error", () => {});
    child.stdin.on("error", () => {});
    child.stdin.end(JSON.stringify(payload));
  } catch {}
}

export default function (pi: any) {
  let last: any;
  pi.on("session_start", (_e: any, ctx: any) => {
    last = ctx;
    forward("session_start", ctx);
  });
  pi.on("agent_settled", (_e: any, ctx: any) => forward("agent_settled", ctx ?? last));
  pi.events?.on?.("herdr:blocked", (data: any) => forward("herdr:blocked", last, { blocked: data }));
}
