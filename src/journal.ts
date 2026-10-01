// One short entry per finished task, searchable, injected into new tasks.
import { Database } from "bun:sqlite";

const db = new Database(new URL("../journal.db", import.meta.url).pathname, { create: true });
db.run("create virtual table if not exists journal using fts5(day unindexed, machine unindexed, repo, branch, text)");

export type Entry = { day: string; machine: string; repo: string; branch: string; text: string };

export const add = (e: Entry) =>
  db.query("insert into journal values ($day, $machine, $repo, $branch, $text)").run({
    $day: e.day, $machine: e.machine, $repo: e.repo, $branch: e.branch, $text: e.text,
  });

const terms = (q: string) =>
  q.toLowerCase().match(/[a-z0-9_]{3,}/g)?.slice(0, 12).map((t) => `"${t}"`).join(" OR ") ?? "";

export function search(q: string, limit = 5): Entry[] {
  const match = terms(q);
  if (!match) return [];
  return db.query("select * from journal where journal match ? order by rank limit ?").all(match, limit) as Entry[];
}

const recent = (repo: string, limit: number) =>
  db.query("select * from journal where repo = ? order by rowid desc limit ?").all(repo, limit) as Entry[];

export const format = (e: Entry) => `${e.day} ${e.repo} ${e.branch}\n${e.text}`;

// Recent entries for the repo plus matches for the prompt, capped so it never grows the context.
export function context(repo: string, prompt: string, cap = 2000) {
  const seen = new Set<string>();
  const lines: string[] = [];
  let size = 0;
  for (const e of [...recent(repo, 3), ...search(`${repo} ${prompt}`, 5)]) {
    const s = format(e);
    if (seen.has(s) || size + s.length > cap) continue;
    seen.add(s);
    lines.push(s);
    size += s.length;
  }
  return lines.join("\n\n");
}

export const ENTRY_PROMPT = `This task is done. Reply with only a journal entry, five lines at most, no preamble:
what: <one line>
why: <one line>
outcome: <one line, include commit or PR if any>
decisions: <non-obvious choices and their reason>
next: <follow-ups, or none>
If you learned something durable about this repo, also save it to your project memory before replying.`;
