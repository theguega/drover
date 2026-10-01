import {
  ActionRowBuilder, AttachmentBuilder, ButtonBuilder, ButtonStyle, ChannelType, Client, Events,
  GatewayIntentBits, MessageFlags, REST, Routes, SlashCommandBuilder,
  type ChatInputCommandInteraction, type Interaction, type Message, type ThreadChannel,
} from "discord.js";
import * as herdr from "./herdr";
import * as journal from "./journal";
import type { Machine } from "./herdr";

// ── config ─────────────────────────────────────────────────

const env = (k: string, d?: string) => {
  const v = process.env[k] ?? d;
  if (v === undefined) throw new Error(`missing env ${k}`);
  return v;
};
const TOKEN = env("DISCORD_TOKEN");
const GUILD = env("DISCORD_GUILD_ID");
const CHANNEL = env("DISCORD_CHANNEL_ID");
const ALLOWED = new Set(env("ALLOWED_USER_IDS").split(",").map((s) => s.trim()));
const POLL_MS = Number(env("POLL_MS", "4000"));
const STATE = new URL("../state.json", import.meta.url).pathname;

const MACHINES: Machine[] = [
  { name: env("HOST_NAME", "mac"), remote: false },
  ...env("REMOTES", "meerkat").split(",").filter(Boolean).map((name) => ({ name, remote: true })),
];
const machine = (name: string) => {
  const m = MACHINES.find((x) => x.name === name);
  if (!m) throw new Error(`unknown machine ${name}`);
  return m;
};

// ── state: one thread per herdr pane ───────────────────────

type Task = {
  machine: string;
  pane: string;
  workspace: string;
  kind: string;
  repo: string;
  branch: string;
  owned: boolean; // worktree opened by drover, so /done may remove it
  status?: string;
  session?: string;
  lastReply?: string;
  expecting?: boolean; // a prompt from Discord is waiting for its reply
  pending?: string; // first prompt, sent once the agent is ready
  promptMsg?: string; // message that carries the state reaction
  dialogMsg?: string;
  ending?: { remove: boolean }; // /done sent, next reply is the journal entry
  gone?: boolean;
};
let tasks: Record<string, Task> = {};
try { tasks = await Bun.file(STATE).json(); } catch {}
const save = () => Bun.write(STATE, JSON.stringify(tasks, null, 2));

// ── discord ui ─────────────────────────────────────────────

const client = new Client({
  intents: [GatewayIntentBits.Guilds, GatewayIntentBits.GuildMessages, GatewayIntentBits.MessageContent],
});

const thread = async (id: string) => (await client.channels.fetch(id).catch(() => null)) as ThreadChannel | null;
const tail = (s: string, n: number) => (s.length > n ? "…" + s.slice(-n) : s);
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

async function post(t: ThreadChannel, text: string) {
  if (text.length > 6000) {
    await t.send({ content: text.slice(0, 1500) + "…", files: [new AttachmentBuilder(Buffer.from(text), { name: "reply.md" })] });
    return;
  }
  for (let i = 0; i < text.length; i += 1900) await t.send(text.slice(i, i + 1900));
}

const KEYS = [["1", "2", "3", "up", "down"], ["enter", "esc", "tab", "ctrl+c", "refresh"]];
const keyRows = () => KEYS.map((row) =>
  new ActionRowBuilder<ButtonBuilder>().addComponents(row.map((k) =>
    new ButtonBuilder().setCustomId(`key:${k}`).setLabel(k)
      .setStyle(k === "enter" ? ButtonStyle.Primary : ButtonStyle.Secondary))));

async function screen(t: Task, note = "") {
  const raw = await herdr.screen(machine(t.machine), t.pane).catch((e) => `read failed: ${e.message}`);
  const text = raw.split("\n").filter((l) => l.trim()).slice(-30).join("\n").replaceAll("```", "ˋˋˋ");
  return { content: note + "```\n" + tail(text, 1800) + "\n```", components: keyRows() };
}

// State lives in one reaction on the latest prompt: 👀 working, ⏸ needs you, ✅ done.
async function mark(th: ThreadChannel, t: Task, emoji: string) {
  if (!t.promptMsg) return;
  const msg = await th.messages.fetch(t.promptMsg).catch(() => null);
  if (!msg) return;
  for (const r of msg.reactions.cache.values()) if (r.me && r.emoji.name !== emoji) await r.users.remove(client.user!.id).catch(() => {});
  await msg.react(emoji).catch(() => {});
}

// ── commands ───────────────────────────────────────────────

const machineOpt = (o: any) => o.setName("machine").setDescription("where it runs").setRequired(true)
  .addChoices(...MACHINES.map((m) => ({ name: m.name, value: m.name })));

const commands = [
  new SlashCommandBuilder().setName("new").setDescription("Worktree + agent in a new thread")
    .addStringOption(machineOpt)
    .addStringOption((o) => o.setName("repo").setDescription("repo in ~/Developer").setRequired(true).setAutocomplete(true))
    .addStringOption((o) => o.setName("branch").setDescription("new name, existing branch, or #pr").setRequired(true))
    .addStringOption((o) => o.setName("prompt").setDescription("first instruction").setRequired(true))
    .addStringOption((o) => o.setName("agent").setDescription("default claude")
      .addChoices(...["claude", "codex", "opencode", "pi"].map((k) => ({ name: k, value: k })))),
  new SlashCommandBuilder().setName("attach").setDescription("Follow an agent already running in herdr")
    .addStringOption(machineOpt)
    .addStringOption((o) => o.setName("agent").setDescription("running agent").setRequired(true).setAutocomplete(true)),
  new SlashCommandBuilder().setName("agents").setDescription("Agents on every machine"),
  new SlashCommandBuilder().setName("worktrees").setDescription("Worktrees on every machine"),
  new SlashCommandBuilder().setName("screen").setDescription("This thread's terminal"),
  new SlashCommandBuilder().setName("keys").setDescription("Send keys to this thread's agent")
    .addStringOption((o) => o.setName("keys").setDescription("e.g. esc, shift+tab enter").setRequired(true)),
  new SlashCommandBuilder().setName("done").setDescription("Write the journal entry and close the workspace")
    .addBooleanOption((o) => o.setName("remove").setDescription("also delete the worktree checkout")),
  new SlashCommandBuilder().setName("recall").setDescription("Search the journal")
    .addStringOption((o) => o.setName("query").setDescription("words").setRequired(true)),
].map((c) => c.toJSON());

const slug = (s: string) => s.toLowerCase().replace(/[^a-z0-9_-]+/g, "-").replace(/^-+|-+$/g, "");
const agentName = (s: string) => `d-${slug(s).slice(0, 24)}-${Math.random().toString(36).slice(2, 6)}`;

async function openThread(name: string) {
  const ch = await client.channels.fetch(CHANNEL);
  if (ch?.type !== ChannelType.GuildText) throw new Error("DISCORD_CHANNEL_ID is not a text channel");
  return ch.threads.create({ name: name.slice(0, 100), autoArchiveDuration: 10080 });
}

const live = (m: string, pane: string) =>
  Object.entries(tasks).find(([, t]) => t.machine === m && t.pane === pane && !t.gone)?.[0];

async function cmdNew(i: ChatInputCommandInteraction) {
  const m = machine(i.options.getString("machine", true));
  const repo = i.options.getString("repo", true);
  const ref = i.options.getString("branch", true);
  const text = i.options.getString("prompt", true);
  const kind = i.options.getString("agent") ?? "claude";

  await i.deferReply();
  const wt = await herdr.worktree(m, repo, ref);
  const bound = live(m.name, wt.paneId);
  if (bound) return void i.editReply(`already open in <#${bound}>`);

  const th = await openThread(`${repo} · ${wt.branch} · ${m.name}`);
  await i.editReply(`${th}`);
  const head = await th.send(`\`${wt.path}\`\n> ${tail(text, 1500).replaceAll("\n", "\n> ")}`);

  const memory = journal.context(repo, text);
  tasks[th.id] = {
    machine: m.name, pane: wt.paneId, workspace: wt.workspaceId, kind, repo, branch: wt.branch, owned: true,
    pending: memory ? `${text}\n\nContext from earlier tasks (journal, may be stale):\n${memory}` : text,
    promptMsg: head.id,
  };
  await save();
  await head.react("👀");
  // a startup dialog shows up through the poller; the prompt goes out once the agent is idle
  await herdr.start(m, agentName(wt.branch), kind, wt.paneId);
}

async function cmdAttach(i: ChatInputCommandInteraction) {
  const m = machine(i.options.getString("machine", true));
  const pane = i.options.getString("agent", true);
  const a = (await herdr.agents(m)).find((x) => x.pane_id === pane);
  if (!a) throw new Error(`no agent in ${pane} on ${m.name}`);
  const bound = live(m.name, pane);
  if (bound) return void i.reply(`already open in <#${bound}>`);

  await i.deferReply();
  const repo = a.cwd.split("/").pop() ?? pane;
  const th = await openThread(`${repo} · ${a.agent} · ${m.name}`);
  const t: Task = {
    machine: m.name, pane, workspace: a.workspace_id, kind: a.agent, repo, branch: "", owned: false,
    status: a.agent_status, session: a.agent_session?.value,
  };
  if (t.session) t.lastReply = (await herdr.reply(m, t.session))?.id;
  tasks[th.id] = t;
  await save();
  await i.editReply(`${th}`);
  await th.send(await screen(t));
}

const ICON: Record<string, string> = { working: "●", blocked: "◆", done: "✓", idle: "○" };

async function cmdAgents(i: ChatInputCommandInteraction) {
  await i.deferReply();
  const lines: string[] = [];
  for (const m of MACHINES) {
    const list = await herdr.agents(m).catch((e: Error) => e);
    lines.push(`**${m.name}**`);
    if (list instanceof Error) { lines.push(`unreachable: ${list.message}`); continue; }
    for (const a of list) {
      const bound = live(m.name, a.pane_id);
      const where = a.cwd.replace(/^\/(Users|home)\/[^/]+/, "~");
      lines.push(`\`${ICON[a.agent_status] ?? "?"} ${a.pane_id}\` ${a.agent} ${where}${bound ? ` <#${bound}>` : ""}`);
    }
  }
  await i.editReply(tail(lines.join("\n"), 1990));
}

async function cmdWorktrees(i: ChatInputCommandInteraction) {
  await i.deferReply();
  const parts = await Promise.all(MACHINES.map(async (m) =>
    `**${m.name}**\n\`\`\`\n${await herdr.worktrees(m).catch((e) => `unreachable: ${e.message}`) || "none"}\n\`\`\``));
  await i.editReply(tail(parts.join("\n"), 1990));
}

function taskIn(channelId: string) {
  const t = tasks[channelId];
  if (!t) throw new Error("run this inside a task thread");
  return t;
}

async function cmdDone(i: ChatInputCommandInteraction) {
  const t = taskIn(i.channelId);
  const remove = i.options.getBoolean("remove") ?? false;
  if (remove && !t.owned) throw new Error("drover didn't open this worktree, remove it by hand");
  await herdr.prompt(machine(t.machine), t.pane, journal.ENTRY_PROMPT);
  t.ending = { remove };
  t.expecting = true;
  await save();
  const msg = await i.reply({ content: "writing the journal entry", fetchReply: true });
  t.promptMsg = msg.id;
  await msg.react("👀");
}

async function finish(th: ThreadChannel, id: string, t: Task, entry: string) {
  const m = machine(t.machine);
  journal.add({ day: new Date().toISOString().slice(0, 10), machine: t.machine, repo: t.repo, branch: t.branch, text: entry });
  await th.send("```\n" + entry.replaceAll("```", "") + "\n```");
  await (t.ending!.remove ? herdr.remove(m, t.workspace) : herdr.close(m, t.workspace))
    .catch((e) => th.send(`close failed: ${e.message}`));
  delete tasks[id];
  await th.setArchived(true).catch(() => {});
}

async function onCommand(i: ChatInputCommandInteraction) {
  switch (i.commandName) {
    case "new": return cmdNew(i);
    case "attach": return cmdAttach(i);
    case "agents": return cmdAgents(i);
    case "worktrees": return cmdWorktrees(i);
    case "done": return cmdDone(i);
    case "screen": return void i.reply(await screen(taskIn(i.channelId)));
    case "keys": {
      const t = taskIn(i.channelId);
      await herdr.keys(machine(t.machine), t.pane, i.options.getString("keys", true).split(/\s+/));
      await sleep(700);
      return void i.reply(await screen(t));
    }
    case "recall": {
      const hits = journal.search(i.options.getString("query", true));
      return void i.reply(hits.length ? tail(hits.map(journal.format).join("\n\n"), 1990) : "nothing");
    }
  }
}

async function onAutocomplete(i: Interaction) {
  if (!i.isAutocomplete()) return;
  const m = machine(i.options.getString("machine") ?? MACHINES[0]!.name);
  const q = i.options.getFocused().toLowerCase();
  const choices = i.commandName === "new"
    ? (await herdr.repos(m).catch(() => [])).map((r) => ({ name: r, value: r }))
    : (await herdr.agents(m).catch(() => [])).map((a) => ({
      name: `${a.pane_id} ${a.agent} ${a.agent_status} ${a.cwd.split("/").slice(-2).join("/")}`.slice(0, 100),
      value: a.pane_id,
    }));
  await i.respond(choices.filter((c) => c.name.toLowerCase().includes(q)).slice(0, 25)).catch(() => {});
}

async function onButton(i: Interaction) {
  if (!i.isButton() || !i.customId.startsWith("key:")) return;
  const t = taskIn(i.channelId);
  const k = i.customId.slice(4);
  await i.deferUpdate();
  if (k !== "refresh") {
    await herdr.keys(machine(t.machine), t.pane, [k]);
    await sleep(700);
  }
  await i.editReply(await screen(t));
}

// A plain message in a task thread is the next prompt.
async function onMessage(msg: Message) {
  if (msg.author.bot || !ALLOWED.has(msg.author.id)) return;
  const t = tasks[msg.channelId];
  if (!t) return;
  const text = [msg.content, ...msg.attachments.map((a) => `attachment: ${a.url}`)].join("\n").trim();
  if (!text) return;
  if (t.gone) return void msg.reply("agent exited, /attach a running one or /new");
  try {
    await herdr.prompt(machine(t.machine), t.pane, text);
  } catch (e: any) {
    if (e.code !== "agent_blocked") throw e;
    return void msg.reply(await screen(t, "answer the dialog first, then resend\n"));
  }
  t.expecting = true;
  t.promptMsg = msg.id;
  await save();
  await msg.react("👀");
}

// ── poller: herdr state changes become reactions and replies ──

async function tick() {
  const lists = new Map<string, herdr.Agent[] | null>();
  for (const [id, t] of Object.entries(tasks)) {
    const m = MACHINES.find((x) => x.name === t.machine);
    if (t.gone || !m) continue; // a machine this host doesn't drive
    if (!lists.has(m.name)) lists.set(m.name, await herdr.agents(m).catch(() => null));
    const list = lists.get(m.name);
    if (!list) continue; // unreachable, try again next tick

    const th = await thread(id);
    if (!th) { delete tasks[id]; continue; }
    const a = list.find((x) => x.pane_id === t.pane);
    if (!a) {
      if (t.status !== undefined) { t.gone = true; await th.send("agent exited"); }
      continue; // or still starting
    }

    const prev = t.status;
    const now = a.agent_status;
    t.status = now;
    if (a.agent_session?.value) t.session = a.agent_session.value;
    const ready = now === "idle" || now === "done";

    if (now === "blocked" && prev !== "blocked") {
      t.dialogMsg = (await th.send(await screen(t))).id;
      await mark(th, t, "⏸️");
    } else if (prev === "blocked" && now !== "blocked") {
      if (t.dialogMsg) await th.messages.delete(t.dialogMsg).catch(() => {});
      t.dialogMsg = undefined;
      await mark(th, t, "👀");
    }

    if (ready && t.pending) {
      const p = t.pending;
      t.pending = undefined;
      if (t.session) t.lastReply = (await herdr.reply(m, t.session))?.id;
      await herdr.prompt(m, t.pane, p).then(() => { t.expecting = true; })
        .catch((e) => th.send(`first prompt failed: ${e.message}`));
      continue;
    }

    if (!ready || !(t.expecting || prev === "working")) continue;
    if (t.kind === "claude") {
      const r = t.session ? await herdr.reply(m, t.session) : null;
      if (!r || r.id === t.lastReply) continue;
      t.lastReply = r.id;
      t.expecting = false;
      await mark(th, t, "✅");
      if (t.ending) await finish(th, id, t, r.text);
      else await post(th, r.text);
    } else if (prev === "working") {
      t.expecting = false;
      await mark(th, t, "✅");
      await post(th, "```\n" + tail(await herdr.screen(m, t.pane, 80), 5000) + "\n```");
    }
  }
  await save();
}

// ── wiring ─────────────────────────────────────────────────

client.on(Events.InteractionCreate, async (i) => {
  if (!ALLOWED.has(i.user.id)) {
    if (i.isRepliable()) await i.reply({ content: "not allowed", flags: MessageFlags.Ephemeral });
    return;
  }
  try {
    if (i.isAutocomplete()) await onAutocomplete(i);
    else if (i.isButton()) await onButton(i);
    else if (i.isChatInputCommand()) await onCommand(i);
  } catch (e: any) {
    console.error(e);
    if (!i.isRepliable()) return;
    const content = String(e.message ?? e).slice(0, 1900);
    if (i.deferred || i.replied) await i.editReply({ content }).catch(() => {});
    else await i.reply({ content, flags: MessageFlags.Ephemeral }).catch(() => {});
  }
});

client.on(Events.MessageCreate, (msg) => onMessage(msg).catch(async (e) => {
  console.error(e);
  await msg.reply(String(e.message ?? e).slice(0, 1900)).catch(() => {});
}));

client.once(Events.ClientReady, async (c) => {
  await new REST().setToken(TOKEN).put(Routes.applicationGuildCommands(c.user.id, GUILD), { body: commands });
  console.log(`drover ready as ${c.user.tag} on ${MACHINES.map((m) => m.name).join(", ")}`);
  let busy = false;
  setInterval(async () => {
    if (busy) return;
    busy = true;
    try { await tick(); } catch (e) { console.error("tick", e); } finally { busy = false; }
  }, POLL_MS);
});

await client.login(TOKEN);
