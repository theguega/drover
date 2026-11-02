// ACP, the Agent Client Protocol (agentclientprotocol.com): JSON-RPC 2.0, one message a line,
// over the agent's stdio. Cursor (`cursor-agent acp`) and pi (`pi-acp`) speak it. One warm
// process per run: initialize, session/new or session/load, the reader's mode and the model, then
// a session/prompt per turn, whose session/update notifications are the turn's events. What the
// agent asks the user is a request on its stdout, answered on its stdin. See SPIKE.md §9.

use std::collections::HashMap;

use serde_json::{Map, Value, json};

use super::{Abort, Caps, Decision, Harness, Out, Parse, StartSpec};
use crate::{
    herdr::SessionId,
    run::{Profile, Usage},
};

pub struct Acp {
    agent: &'static [&'static str],
    // the agent's TUI, to which a pane run's session id is appended
    tui: &'static [&'static str],
    // the session mode a reader runs in; pi's reader is its cwd's .pi/settings.json instead
    reader_mode: Option<&'static str>,
    forward: &'static [&'static str],
    // when the first is forwarded to a remote, the agent runs with the second set
    keyed: Option<(&'static str, &'static str)>,
}

// The key is the login on a remote: a mac's keychain is locked to an ssh session, and the
// memory store skips it (SPIKE.md §9).
pub static CURSOR: Acp = Acp {
    agent: &["cursor-agent", "acp"],
    tui: &["cursor-agent", "--resume"],
    reader_mode: Some("ask"),
    forward: &["CURSOR_API_KEY", "CURSOR_CONFIG_DIR"],
    keyed: Some(("CURSOR_API_KEY", "AGENT_CLI_CREDENTIAL_STORE=memory")),
};

// pi-acp runs `pi --mode rpc`; its session id is pi's own.
pub static PI: Acp = Acp {
    agent: &["pi-acp"],
    tui: &["pi", "--session"],
    reader_mode: None,
    forward: &[],
    keyed: None,
};

// A question the agent asks is answered with `drover send`.
pub const QUESTION: &str = "AskQuestion";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Asked {
    Init,
    Session,
    Setup,
    Prompt,
}

// One connection's state, as `Parse::acp`.
#[derive(Default)]
pub struct Conn {
    next: u64,
    asked: HashMap<u64, Asked>,
    cwd: String,
    resume: Option<SessionId>,
    mode: Option<&'static str>,
    model: Option<String>,
    session: Option<SessionId>,
    // the session is set up: prompts go out, else they wait in `queue`
    ready: bool,
    queue: Vec<String>,
    // session/load replays the history as updates, which are no turn's
    loading: bool,
    // the turn's text since its last tool call: the reply
    text: String,
    // tool call id to its tool and input
    tools: HashMap<String, (String, Value)>,
    // the session's cost so far, as usage_update reports it
    cost: f64,
    // the prompt that follows a turn cancelled to carry a deny's reason
    followup: Option<String>,
}

impl Conn {
    fn request(&mut self, what: Asked, method: &str, params: Value) -> String {
        self.next += 1;
        self.asked.insert(self.next, what);
        json!({"jsonrpc": "2.0", "id": self.next, "method": method, "params": params}).to_string()
    }

    fn prompt(&mut self, text: &str) -> String {
        self.text.clear();
        let session = self.session.as_ref().map(SessionId::as_str);
        self.request(
            Asked::Prompt,
            "session/prompt",
            json!({"sessionId": session, "prompt": [{"type": "text", "text": text}]}),
        )
    }

    fn cancel(&self) -> Option<String> {
        let session = self.session.as_ref()?;
        Some(
            json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": session.as_str()}})
                .to_string(),
        )
    }

    // Once nothing of the setup is left: the prompts that waited go out.
    fn flush(&mut self) -> Vec<Out> {
        if self.ready || self.asked.values().any(|a| *a == Asked::Setup) {
            return Vec::new();
        }
        self.ready = true;
        let queue = std::mem::take(&mut self.queue);
        queue.iter().map(|t| Out::Write(self.prompt(t))).collect()
    }

    fn session_ready(&mut self, result: &Value) -> Vec<Out> {
        self.loading = false;
        let id = text(result, "sessionId")
            .and_then(SessionId::parse)
            .or_else(|| self.resume.clone());
        let Some(id) = id else {
            return vec![Out::Fail("the agent gave no session id".into())];
        };
        self.session = Some(id.clone());
        let mut out = vec![Out::Session(id.clone())];
        if let Some(mode) = self.mode {
            let line = self.request(
                Asked::Setup,
                "session/set_mode",
                json!({"sessionId": id.as_str(), "modeId": mode}),
            );
            out.push(Out::Write(line));
        }
        if let Some(want) = self.model.clone() {
            match pick(result.get("models"), &want) {
                Some(model) => {
                    let line = self.request(
                        Asked::Setup,
                        "session/set_model",
                        json!({"sessionId": id.as_str(), "modelId": model}),
                    );
                    out.push(Out::Write(line));
                }
                None => return vec![Out::Fail(format!("the agent has no model {want}"))],
            }
        }
        out.extend(self.flush());
        out
    }

    fn response(
        &mut self,
        id: u64,
        result: Option<&Value>,
        error: Option<&Value>,
        turn_cost: &mut f64,
    ) -> Vec<Out> {
        let Some(what) = self.asked.remove(&id) else {
            return Vec::new();
        };
        let failed = error.map(|e| {
            let msg = text(e, "message").unwrap_or("error");
            match e.pointer("/data/message").and_then(Value::as_str) {
                Some(more) => format!("{msg}: {more}"),
                None => msg.into(),
            }
        });
        let result = result.unwrap_or(&Value::Null);
        match (what, failed) {
            (Asked::Prompt, None) if self.followup.is_some() => {
                let text = self.followup.take().unwrap_or_default();
                vec![Out::Write(self.prompt(&text))]
            }
            (Asked::Prompt, failed) => vec![self.reply(result, failed, turn_cost)],
            (Asked::Init, Some(e)) => vec![Out::Fail(format!("initialize: {e}"))],
            (Asked::Session, Some(e)) => vec![Out::Fail(format!("session: {e}"))],
            (Asked::Setup, Some(e)) => vec![Out::Fail(format!("session setup: {e}"))],
            (Asked::Init, None) => {
                let params = json!({"cwd": self.cwd, "mcpServers": []});
                let line = match self.resume.clone() {
                    Some(s)
                        if result.pointer("/agentCapabilities/loadSession")
                            == Some(&json!(true)) =>
                    {
                        self.loading = true;
                        let mut params = params;
                        if let Value::Object(p) = &mut params {
                            p.insert("sessionId".into(), s.as_str().into());
                        }
                        self.request(Asked::Session, "session/load", params)
                    }
                    Some(_) => return vec![Out::Fail("the agent cannot load a session".into())],
                    None => self.request(Asked::Session, "session/new", params),
                };
                vec![Out::Write(line)]
            }
            (Asked::Session, None) => self.session_ready(result),
            (Asked::Setup, None) => self.flush(),
        }
    }

    // The turn's last message, and its tokens and cost when the agent reports them.
    fn reply(&mut self, result: &Value, failed: Option<String>, turn_cost: &mut f64) -> Out {
        let tokens = |k: &str| {
            result
                .pointer(&format!("/usage/{k}"))
                .and_then(Value::as_u64)
        };
        let cost = (self.cost - std::mem::replace(turn_cost, self.cost)).max(0.0);
        let usage = (result.get("usage").is_some() || cost > 0.0).then(|| Usage {
            input: tokens("inputTokens").unwrap_or_default(),
            output: tokens("outputTokens").unwrap_or_default(),
            cache_read: tokens("cachedReadTokens").unwrap_or_default(),
            cache_write: tokens("cachedWriteTokens").unwrap_or_default(),
            cost_usd: cost,
        });
        let text = std::mem::take(&mut self.text);
        Out::Reply {
            error: failed.is_some(),
            text: failed.unwrap_or(text),
            usage,
        }
    }

    fn update(&mut self, u: &Value, turn_cost: &mut f64) -> Vec<Out> {
        if self.loading {
            return Vec::new();
        }
        let id = || text(u, "toolCallId").unwrap_or_default().to_string();
        match text(u, "sessionUpdate").unwrap_or_default() {
            "agent_message_chunk" => match u.pointer("/content/text").and_then(Value::as_str) {
                Some(t) => {
                    self.text.push_str(t);
                    vec![Out::Text(t.into())]
                }
                None => Vec::new(),
            },
            "tool_call" => {
                let tool = tool_name(u);
                let input = u.get("rawInput").cloned().unwrap_or_default();
                self.tools.insert(id(), (tool.clone(), input));
                // the reply is what the agent says after its last tool call
                self.text.clear();
                let mut out = vec![Out::ToolStart { tool: tool.clone() }];
                out.extend(self.tool_end(&id(), text(u, "status")));
                out
            }
            "tool_call_update" => {
                if let (Some(raw), Some((_, input))) =
                    (u.get("rawInput"), self.tools.get_mut(&id()))
                {
                    *input = raw.clone();
                }
                self.tool_end(&id(), text(u, "status"))
                    .into_iter()
                    .collect()
            }
            "usage_update" => {
                if let Some(c) = u.get("cost").filter(|c| text(c, "currency") == Some("USD")) {
                    self.cost = c.get("amount").and_then(Value::as_f64).unwrap_or(self.cost);
                    // a cost reported between turns, as on load, is no turn's
                    if !self.asked.values().any(|a| *a == Asked::Prompt) {
                        *turn_cost = self.cost;
                    }
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn tool_end(&mut self, id: &str, status: Option<&str>) -> Option<Out> {
        let ok = match status {
            Some("completed") => true,
            Some("failed") => false,
            _ => return None,
        };
        let (tool, _) = self.tools.remove(id)?;
        Some(Out::ToolEnd { tool, ok })
    }

    // A request from the agent: what it asks the user, or a method drover does not serve.
    fn asks(&mut self, method: &str, id: Value, params: &Value) -> Out {
        let permission =
            |tool: String, input: Value, call: Value| Out::Permission { tool, input, call };
        match method {
            "session/request_permission" => {
                let call = params.get("toolCall").unwrap_or(&Value::Null);
                let title = text(call, "title").unwrap_or_default();
                let known = self.tools.get(text(call, "toolCallId").unwrap_or_default());
                let tool = known.map_or_else(|| tool_name(call), |(t, _)| t.clone());
                let mut input = match known.map(|(_, i)| i).or(call.get("rawInput")) {
                    Some(Value::Object(m)) => m.clone(),
                    _ => Map::new(),
                };
                input.insert("title".into(), title.into());
                let diff = call.get("content").and_then(Value::as_array).and_then(|c| {
                    c.iter().find_map(|b| (text(b, "type") == Some("diff")).then(|| b.get("path")).flatten())
                });
                if let Some(path) = diff {
                    input.entry("path").or_insert_with(|| path.clone());
                }
                let call = json!({"id": id, "kind": "permission", "title": title, "options": params.get("options")});
                permission(tool, Value::Object(input), call)
            }
            "cursor/ask_question" => permission(
                QUESTION.into(),
                json!({"title": params.get("title"), "questions": params.get("questions")}),
                json!({"id": id, "kind": "question", "questions": params.get("questions")}),
            ),
            "cursor/create_plan" => permission(
                "CreatePlan".into(),
                json!({"name": params.get("name"), "overview": params.get("overview"), "plan": params.get("plan")}),
                json!({"id": id, "kind": "plan"}),
            ),
            _ => Out::Write(
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("drover does not serve {method}")}})
                    .to_string(),
            ),
        }
    }
}

fn text<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

// The kind ACP gives a tool call, as a tool's name; `execute` is a shell.
fn tool_name(call: &Value) -> String {
    match text(call, "kind") {
        Some("execute") => "Shell".into(),
        Some(k) if k != "other" => {
            let mut c = k.chars();
            c.next()
                .map(|f| f.to_uppercase().chain(c).collect())
                .unwrap_or_default()
        }
        _ => text(call, "title").unwrap_or("Other").into(),
    }
}

// The model asked for, among the session's: by id, by id up to its parameters, or by name.
fn pick(models: Option<&Value>, want: &str) -> Option<String> {
    let Some(available) = models
        .and_then(|m| m.get("availableModels"))
        .and_then(Value::as_array)
    else {
        // an agent that lists none takes the name as it is
        return Some(want.into());
    };
    let want = want.to_lowercase();
    available.iter().find_map(|m| {
        let id = text(m, "modelId")?;
        let name = text(m, "name").unwrap_or_default().to_lowercase();
        let base = id.split('[').next().unwrap_or(id).to_lowercase();
        (id.to_lowercase() == want || base == want || name == want).then(|| id.to_string())
    })
}

impl Harness for Acp {
    fn caps(&self) -> Caps {
        Caps {
            headless: true,
            pane: false,
            resume: true,
            approvals: true,
            // only when the agent reports it: cursor does not
            usage: false,
            hooks: false,
        }
    }

    // A pane is the agent's own TUI on the run's session, which `attach` opens.
    fn start(&self, spec: &StartSpec) -> Vec<String> {
        let words = |w: &[&str]| w.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        if !spec.headless {
            let mut argv = words(self.tui);
            match spec.resume {
                Some(s) => argv.push(s.as_str().into()),
                None => argv.truncate(1),
            }
            return argv;
        }
        let mut argv = Vec::new();
        if let Some((key, set)) = self.keyed
            && spec.forwarded.contains(&key)
        {
            argv.extend(words(&["env", set]));
        }
        argv.extend(words(self.agent));
        argv
    }

    fn open(&self, spec: &StartSpec, p: &mut Parse) -> Vec<String> {
        let c = &mut p.acp;
        c.cwd = spec.cwd.to_string_lossy().into();
        c.resume = spec.resume.cloned();
        c.mode = self.reader_mode.filter(|_| spec.profile == Profile::Reader);
        c.model = spec.model.map(String::from);
        let init = json!({
            "protocolVersion": 1,
            "clientCapabilities": {"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false},
            "clientInfo": {"name": "drover", "version": env!("CARGO_PKG_VERSION")},
        });
        vec![c.request(Asked::Init, "initialize", init)]
    }

    fn send(&self, text: &str, p: &mut Parse) -> Vec<String> {
        let c = &mut p.acp;
        if !c.ready {
            c.queue.push(text.into());
            return Vec::new();
        }
        vec![c.prompt(text)]
    }

    fn abort(&self, p: &mut Parse) -> Abort {
        p.acp.followup = None;
        p.acp.cancel().map_or(Abort::Kill, Abort::Stdin)
    }

    fn events(&self, line: &str, p: &mut Parse) -> Vec<Out> {
        let Ok(Value::Object(m)) = serde_json::from_str::<Value>(line) else {
            return Vec::new();
        };
        let params = m.get("params").unwrap_or(&Value::Null);
        match (m.get("method").and_then(Value::as_str), m.get("id")) {
            (Some(method), Some(id)) => vec![p.acp.asks(method, id.clone(), params)],
            (Some("session/update"), None) => match params.get("update") {
                Some(u) => p.acp.update(u, &mut p.cost),
                None => Vec::new(),
            },
            (None, Some(id)) => match id.as_u64() {
                Some(id) => p
                    .acp
                    .response(id, m.get("result"), m.get("error"), &mut p.cost),
                None => Vec::new(),
            },
            _ => Vec::new(),
        }
    }

    fn answer(&self, d: &Decision, call: &Value, p: &mut Parse) -> Vec<String> {
        let c = &mut p.acp;
        let reason = d.reason.as_deref();
        let result = match text(call, "kind").unwrap_or_default() {
            "permission" => {
                let want = if d.allow { "allow_once" } else { "reject_once" };
                let option = call
                    .get("options")
                    .and_then(Value::as_array)
                    .and_then(|o| o.iter().find(|o| text(o, "kind") == Some(want)))
                    .and_then(|o| text(o, "optionId"))
                    .unwrap_or(if d.allow { "allow-once" } else { "reject-once" });
                json!({"outcome": {"outcome": "selected", "optionId": option}})
            }
            "question" => question(call.get("questions"), reason),
            _ if d.allow => json!({"outcome": {"outcome": "accepted"}}),
            _ => json!({"outcome": {"outcome": "rejected", "reason": reason}}),
        };
        let mut lines =
            vec![json!({"jsonrpc": "2.0", "id": call.get("id"), "result": result}).to_string()];
        // the agent tells the model only "User rejected": the turn ends, and the reason is the next prompt
        if text(call, "kind") == Some("permission")
            && !d.allow
            && let Some(why) = reason
            && let Some(cancel) = c.cancel()
        {
            let title = text(call, "title").unwrap_or("the call");
            c.followup = Some(format!("drover: the user denied {title}: {why}"));
            lines.push(cancel);
        }
        lines
    }

    fn forward(&self) -> &'static [&'static str] {
        self.forward
    }
}

// Your text picks an option of each question, by id or label; any other text is the answer as
// written, which the agent gets as the reason the questions were skipped.
fn question(questions: Option<&Value>, reason: Option<&str>) -> Value {
    let said = reason.unwrap_or_default().trim().to_lowercase();
    let answers: Vec<Value> = questions
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|q| {
            let option = q.get("options")?.as_array()?.iter().find(|o| {
                [text(o, "id"), text(o, "label")]
                    .into_iter()
                    .flatten()
                    .any(|s| s.to_lowercase() == said)
            })?;
            Some(json!({"questionId": q.get("id"), "selectedOptionIds": [option.get("id")]}))
        })
        .collect();
    if answers.is_empty() {
        json!({"outcome": {"outcome": "skipped", "reason": reason}})
    } else {
        json!({"outcome": {"outcome": "answered", "answers": answers}})
    }
}
