"use strict";

// The token opens the stream. Take it out of the address bar once read, so it stays out of history.
const token = new URLSearchParams(location.search).get("token") ?? "";
history.replaceState(null, "", location.pathname + location.hash);

// Every value from the desk is free text (goal files, log lines, pane names), so each one passes through esc().
const esc = (v) => String(v ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
const $ = (id) => document.getElementById(id);
const clock = (unix) => new Date(Number(unix) * 1000).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", hour12: false });

let desk = null;

const taskAt = (pane) => desk.tasks.find((t) => t.pane === pane);
const crewOf = (mate) => desk.tasks.filter((t) => t.role === "crew" && t.mate === mate);

// A state for a task: herdr's status when it has one, drover's phase otherwise.
function stateChip(status, phase) {
  if (phase === "gone") return `<span class="state gone">gone</span>`;
  const cls = status === "working" ? "working" : status === "blocked" ? "blocked" : status === "done" ? "done" : "";
  return `<span class="state ${cls}">${esc(status ?? phase ?? "unknown")}</span>`;
}

function renderMachines() {
  $("machines").innerHTML = desk.machines
    .map((m) => `<span class="machine"><span class="dot ok"></span><b class="mono" style="font-weight:500">${esc(m.name)}</b>${m.host ? `<span class="sub">host</span>` : ""}</span>`)
    .join("");
}

function deskView() {
  const working = desk.tasks.filter((t) => t.status === "working").length;
  const crew = desk.tasks.filter((t) => t.role === "crew").length;
  const direct = desk.tasks.filter((t) => t.role === "task");
  const escalations = desk.escalations;

  const flow = `
    <section class="flow" aria-label="The flow of work">
      <div class="stage"><div class="label">Mates</div><div class="n">${desk.mates.length}</div><div class="d">plan the goal, dispatch crew</div></div>
      <div class="stage"><div class="label">Crew</div><div class="n">${crew}</div><div class="d">one worktree and one node each, ${working} working</div></div>
      <div class="stage"><div class="label">Direct</div><div class="n">${direct.length}</div><div class="d">tasks with no mate, driven from their own thread</div></div>
      <div class="stage ${escalations.length ? "hot" : ""}"><div class="label">Desk</div><div class="n">${escalations.length}</div><div class="d">escalations waiting on you</div></div>
    </section>`;

  const needs = `
    <div class="section-h"><h2>Needs you</h2><span class="sub mono" style="font-size:12px">${escalations.length} open</span></div>
    <section class="needs" aria-label="Escalations">
      ${escalations.length
        ? escalations.map((e) => `<div class="need"><p><span class="mono sub" style="font-size:12px">${clock(e.at)}</span> <a class="mono" href="#mate-${esc(e.mate)}">${esc(e.mate)}</a> · ${esc(e.text)}</p></div>`).join("")
        : `<p class="empty">Nothing is waiting on you.</p>`}
    </section>`;

  const cards = desk.mates.map((m) => {
    const crews = crewOf(m.name);
    const goalLine = (m.goal.split("\n").find((l) => l.trim() && !l.startsWith("#")) ?? "no goal file yet").trim();
    return `
      <a class="card" href="#mate-${esc(m.name)}" aria-label="Open mate ${esc(m.name)}">
        <div class="card-h"><h3>${esc(m.name)}</h3><span class="chip mate">mate</span></div>
        <p class="goal">${esc(goalLine)}</p>
        <div class="states">${crews.map((t) => stateChip(t.status, t.phase)).join("") || `<span class="sub" style="font-size:12px">no crew running</span>`}</div>
        <div class="card-h"><span class="sub mono" style="font-size:12px">${crews.length} crew · inbox ${crews.reduce((n, t) => n + t.inbox, 0)}</span><span class="open">Open →</span></div>
      </a>`;
  }).join("");

  const directCards = direct.map((t) => `
    <div class="card">
      <div class="card-h"><h3 style="color:var(--text)">${esc(t.repo)} · ${esc(t.branch ?? "")}</h3><span class="chip">direct</span></div>
      <div class="states">${stateChip(t.status, t.phase)}<span class="chip teal">${esc(t.harness)}</span></div>
      <div class="card-h"><span class="sub mono" style="font-size:12px">${esc(t.machine)} · ${esc(t.pane)}</span><span class="sub mono" style="font-size:12px">owner user</span></div>
    </div>`).join("");

  return `
    ${flow}
    ${needs}
    <div class="section-h"><h2>Mates</h2><span class="sub" style="font-size:12px">open one to see how its crew evolved</span></div>
    <section class="cards">${cards || `<p class="empty">No mate is running. Start one with <span class="mono">drover mate &lt;name&gt;</span>.</p>`}</section>
    ${direct.length ? `<div class="section-h"><h2>Direct tasks</h2></div><section class="cards">${directCards}</section>` : ""}`;
}

// A log line is "<unix-time> <event> <detail>". Started, linked, closed and crew lines name their pane first.
const PANE_EVENTS = new Set(["started", "linked", "closed", "crew"]);
function parseLine(line) {
  const [at = "", ev = "", ...rest] = line.split(" ");
  return { at: Number(at), ev, detail: rest.join(" "), pane: PANE_EVENTS.has(ev) ? (rest[0] ?? "") : "" };
}

// What an event means for its lane, read from the event and its text.
function tone(e) {
  if (e.ev === "escalated") return "escalated";
  if (/DONE:|passed/.test(e.detail)) return "pass";
  if (/failed|failing/.test(e.detail)) return "fail";
  if (/dialog/.test(e.detail)) return "blocked";
  if (/exited/.test(e.detail)) return "gone";
  if (/QUESTION:/.test(e.detail)) return "question";
  if (e.ev === "started" || e.ev === "linked") return "start";
  if (e.ev === "crew") return "reply";
  return e.ev;
}

// One lane per crewmate, plus the mate's own lane for its notes and escalations.
function evolution(m) {
  const events = m.log.map(parseLine).filter((e) => Number.isFinite(e.at));
  if (!events.length) return `<p class="empty">No events yet.</p>`;
  const first = Math.min(...events.map((e) => e.at));
  const last = Math.max(...events.map((e) => e.at));
  const span = Math.max(last - first, 1);
  const pct = (at) => ((at - first) / span) * 100;

  const lanes = new Map();
  for (const e of events) {
    if (!lanes.has(e.pane)) lanes.set(e.pane, []);
    lanes.get(e.pane).push(e);
  }
  const label = (pane) => {
    if (pane === "") return "mate";
    const t = taskAt(pane);
    if (!t) return pane;
    return t.role === "mate" ? "mate" : `${t.repo} · ${t.branch ?? ""}`;
  };
  // the mate's own lane first, then crew in the order they first appear
  const keys = [...lanes.keys()].sort((a, b) => Number(b === "") - Number(a === ""));

  const rows = keys.map((pane) => `
    <div class="lane">
      <div class="lane-label"><b>${esc(label(pane))}</b><span class="sub mono">${esc(pane)}</span></div>
      <div class="track">${lanes.get(pane).map((e) => `<span class="mk ${tone(e)}" style="left:${pct(e.at).toFixed(2)}%" title="${esc(`${clock(e.at)} ${e.ev} ${e.detail}`)}"></span>`).join("")}</div>
    </div>`).join("");

  return `
    <div class="axis"><span>${clock(first)}</span><span>${clock(last)}</span></div>
    ${rows}
    <div class="legend">
      <span><i class="mk start"></i>started</span>
      <span><i class="mk reply"></i>replied</span>
      <span><i class="mk question"></i>question</span>
      <span><i class="mk blocked"></i>dialog</span>
      <span><i class="mk pass"></i>check passed</span>
      <span><i class="mk fail"></i>check failed</span>
      <span><i class="mk escalated"></i>escalated</span>
    </div>`;
}

function mateView(name) {
  const m = desk.mates.find((x) => x.name === name);
  if (!m) return `<p class="empty">No running mate named ${esc(name)}. <a href="#desk">Back to the desk</a></p>`;
  const crews = crewOf(name);
  const rows = crews.map((t) => `
    <tr>
      <td>${stateChip(t.status, t.phase)}</td>
      <td class="pane">${esc(t.pane)}</td>
      <td><span class="mono">${esc(t.repo)}</span> <span class="sub">${esc(t.branch ?? "")}</span></td>
      <td><span class="chip teal">${esc(t.harness)}</span></td>
      <td>${esc(t.owner ?? "")}</td>
      <td class="mono">${t.queued ? "queued" : ""}${t.inbox ? ` inbox ${t.inbox}` : ""}</td>
    </tr>`).join("");
  const log = m.log.map((line) => {
    const e = parseLine(line);
    return `<li><span class="t">${clock(e.at)}</span><span class="ev ${esc(e.ev)}">${esc(e.ev)}</span><span class="d">${esc(e.detail)}</span></li>`;
  }).join("");

  return `
    <section class="card" aria-label="Mate">
      <div class="card-h"><h3>${esc(name)}</h3><span class="chip mate">mate</span><span class="chip">${crews.length} crew</span></div>
      <pre class="goal-text">${esc(m.goal || "no goal file yet")}</pre>
    </section>
    <section class="panel" aria-label="Evolution">
      <h3>How it evolved <span class="sub mono" style="font-weight:400;font-size:12px">from the mate log, last ${m.log.length} lines</span></h3>
      <div class="evo">${evolution(m)}</div>
    </section>
    <div class="two">
      <section class="panel" aria-label="Crew">
        <h3>Crew</h3>
        <div class="table-wrap">
          <table>
            <thead><tr><th>state</th><th>pane</th><th>repo · branch</th><th>harness</th><th>owner</th><th></th></tr></thead>
            <tbody>${rows || `<tr><td colspan="6" class="sub">No crew running.</td></tr>`}</tbody>
          </table>
        </div>
      </section>
      <section class="panel" aria-label="Mate log">
        <h3>Log</h3>
        <ul class="log">${log || `<li class="sub">Empty.</li>`}</ul>
      </section>
    </div>`;
}

function render() {
  if (!desk) return;
  renderMachines();
  const mate = location.hash.match(/^#mate-([a-z0-9_-]+)$/);
  if (mate) {
    $("crumb").innerHTML = `<a href="#desk">desk</a> / <span class="mono">${esc(mate[1])}</span>`;
    document.title = `drover · ${mate[1]}`;
    $("view").innerHTML = mateView(mate[1]);
  } else {
    $("crumb").innerHTML = "";
    document.title = "drover desk";
    $("view").innerHTML = deskView();
  }
}

const live = $("live");
const events = new EventSource(`/events?token=${encodeURIComponent(token)}`);
events.onopen = () => { live.textContent = "live"; };
events.onerror = () => {
  live.textContent = events.readyState === EventSource.CLOSED ? "not authorized, check the token" : "disconnected, retrying…";
};
events.onmessage = (e) => {
  desk = JSON.parse(e.data);
  render();
};
window.addEventListener("hashchange", render);
