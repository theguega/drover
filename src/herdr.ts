// herdr and shell access per machine. The host runs commands directly;
// other machines go through `herdr --machine <label>` and `ssh <label>`.

export type Machine = { name: string; remote: boolean };

export type Status = "idle" | "working" | "blocked" | "done" | "unknown";

export type Agent = {
  pane_id: string;
  workspace_id: string;
  agent: string;
  agent_status: Status;
  cwd: string;
  agent_session?: { value: string } | null;
};

export type Worktree = { workspaceId: string; paneId: string; path: string; branch: string };

export class HerdrError extends Error {
  constructor(public code: string, message: string) {
    super(message);
  }
}

const ROOT = "~/Developer";
const SAFE = /^[#\w./-]+$/;

async function run(cmd: string[], timeoutMs = 120_000) {
  const proc = Bun.spawn(cmd, { stdout: "pipe", stderr: "pipe" });
  const timer = setTimeout(() => proc.kill(), timeoutMs);
  const [out, err, code] = await Promise.all([
    new Response(proc.stdout).text(),
    new Response(proc.stderr).text(),
    proc.exited,
  ]);
  clearTimeout(timer);
  return { out, err, code };
}

const sh = (m: Machine, script: string, timeoutMs?: number) =>
  run(m.remote ? ["ssh", "-o", "BatchMode=yes", m.name, script] : ["sh", "-c", script], timeoutMs);

function parse(out: string, err: string, code: number) {
  if (code === 0) return JSON.parse(out).result;
  try {
    const e = JSON.parse(err || out).error;
    throw new HerdrError(e.code, e.message);
  } catch (e) {
    if (e instanceof HerdrError) throw e;
    throw new HerdrError("failed", (err || out).trim() || `exit ${code}`);
  }
}

async function herdr(m: Machine, args: string[], timeoutMs?: number) {
  const cmd = m.remote ? ["herdr", "--machine", m.name, ...args] : ["herdr", ...args];
  const { out, err, code } = await run(cmd, timeoutMs);
  return { out, err, code };
}

const json = async (m: Machine, args: string[], timeoutMs?: number) => {
  const r = await herdr(m, args, timeoutMs);
  return parse(r.out, r.err, r.code);
};

export const agents = async (m: Machine): Promise<Agent[]> => (await json(m, ["agent", "list"], 20_000)).agents;

// Same entry point as the ⌃b ⇧g popup: a new name, an existing branch, or #pr.
export async function worktree(m: Machine, repo: string, ref: string): Promise<Worktree> {
  if (!SAFE.test(repo) || !SAFE.test(ref)) throw new Error("repo and branch may only use letters, digits, # . _ / -");
  const { out, err, code } = await sh(m, `cd ${ROOT}/${repo} && ~/.local/bin/herdr-wt '${ref}' --no-focus`);
  const r = parse(out, err, code);
  return {
    workspaceId: r.workspace.workspace_id,
    paneId: r.root_pane.pane_id,
    path: r.worktree.path,
    branch: r.worktree.branch,
  };
}

// Resolves once the agent is ready or blocked at startup (folder trust and similar dialogs).
export async function start(m: Machine, name: string, kind: string, paneId: string) {
  try {
    await json(m, ["agent", "start", name, "--kind", kind, "--pane", paneId, "--timeout", "90000"], 100_000);
  } catch (e) {
    if (!(e instanceof HerdrError && e.code === "agent_not_ready")) throw e;
  }
}

export const prompt = (m: Machine, pane: string, text: string) => json(m, ["agent", "prompt", pane, text]);

export const keys = (m: Machine, pane: string, k: string[]) => json(m, ["agent", "send-keys", pane, ...k]);

export async function screen(m: Machine, pane: string, lines = 60) {
  const { out, err, code } = await herdr(m, ["agent", "read", pane, "--source", "recent-unwrapped", "--lines", String(lines)]);
  if (code !== 0) throw new HerdrError("failed", (err || out).trim());
  return out;
}

export const close = (m: Machine, workspaceId: string) => json(m, ["workspace", "close", workspaceId]);

export const remove = (m: Machine, workspaceId: string) =>
  json(m, ["worktree", "remove", "--workspace", workspaceId, "--force"]);

// Same view as `wt ls` at the desk: every repo's worktrees with their herdr state.
export async function worktrees(m: Machine) {
  const { out, err } = await sh(m, "cd ~ && ~/.local/bin/herdr-wt ls", 30_000);
  return (out || err).trimEnd();
}

// Main clones only: a worktree has a .git file, a clone a .git directory.
export async function repos(m: Machine): Promise<string[]> {
  const { out } = await sh(m, `for d in ${ROOT}/*/; do [ -d "$d.git" ] && basename "$d"; done`, 15_000);
  return out.split("\n").filter(Boolean);
}

// Last assistant text after the latest real prompt in a Claude Code transcript.
export async function reply(m: Machine, session: string): Promise<{ id: string; text: string } | null> {
  if (!/^[0-9a-f-]{36}$/.test(session)) return null;
  const { out, code } = await sh(m, `cat ~/.claude/projects/*/${session}.jsonl`, 20_000);
  if (code !== 0) return null;

  const entries = out.split("\n").flatMap((l) => {
    try { return [JSON.parse(l)]; } catch { return []; }
  });
  const isPrompt = (e: any) => {
    const c = e.type === "user" && !e.isMeta && !e.isCompactSummary && e.message?.content;
    return typeof c === "string" || (Array.isArray(c) && c.some((b: any) => b.type === "text"));
  };
  const from = entries.findLastIndex(isPrompt);

  let last: { id: string; text: string } | null = null;
  for (const e of entries.slice(Math.max(from, 0))) {
    if (e.type !== "assistant") continue;
    for (const b of e.message?.content ?? []) {
      if (b.type === "text" && b.text.trim()) last = { id: e.uuid, text: b.text.trim() };
    }
  }
  return last;
}
