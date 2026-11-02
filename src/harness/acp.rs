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
    forward: &'static [(&'static str, &'static str)],
    // when the first is forwarded to a remote, the agent runs with the second set
    keyed: Option<(&'static str, &'static str)>,
}

// The key is the login on a remote: a mac's keychain is locked to an ssh session, and the
// memory store skips it (SPIKE.md §9).
pub static CURSOR: Acp = Acp {
    agent: &["cursor-agent", "acp"],
    tui: &["cursor-agent", "--resume"],
    reader_mode: Some("ask"),
    // a config dir on the remote is named apart from this machine's own
    forward: &[
        ("CURSOR_API_KEY", "CURSOR_API_KEY"),
        ("DROVER_REMOTE_CURSOR_CONFIG_DIR", "CURSOR_CONFIG_DIR"),
    ],
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
    // what the agent asked and drover has not answered: request ids, as JSON
    open: Vec<String>,
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

    fn in_flight(&self) -> bool {
        self.asked.values().any(|a| *a == Asked::Prompt)
    }

    // What the agent still waits on, answered `cancelled`, as ACP asks of a client after
    // session/cancel and as the agent drops it when its turn ends.
    fn drop_open(&mut self) -> Vec<Out> {
        if self.open.is_empty() {
            return Vec::new();
        }
        let mut out: Vec<Out> = std::mem::take(&mut self.open)
            .iter()
            .map(|id| {
                let id: Value = serde_json::from_str(id).unwrap_or_default();
                Out::Write(
                    json!({"jsonrpc": "2.0", "id": id, "result": {"outcome": {"outcome": "cancelled"}}})
                        .to_string(),
                )
            })
            .collect();
        out.push(Out::Cancel);
        out
    }

    // session/cancel, then what the agent waits on as cancelled.
    fn cancel(&mut self) -> Vec<Out> {
        let Some(session) = &self.session else {
            return Vec::new();
        };
        let cancel = json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": session.as_str()}});
        let mut out = vec![Out::Write(cancel.to_string())];
        out.extend(self.drop_open());
        out
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
            // the turn is over: what it left unanswered is moot, and a deny's reason goes next
            (Asked::Prompt, failed) => {
                let mut out = self.drop_open();
                out.push(match (self.followup.take(), failed) {
                    (Some(text), None) => Out::Write(self.prompt(&text)),
                    (_, failed) => self.reply(result, failed, turn_cost),
                });
                out
            }
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
        let id = || text(u, "toolCallId").unwrap_or_default().to_string();
        match text(u, "sessionUpdate").unwrap_or_default() {
            "usage_update" => {
                if let Some(c) = u.get("cost").filter(|c| text(c, "currency") == Some("USD")) {
                    self.cost = c.get("amount").and_then(Value::as_f64).unwrap_or(self.cost);
                    // a cost reported between turns, as on load, is no turn's
                    if !self.in_flight() {
                        *turn_cost = self.cost;
                    }
                }
                Vec::new()
            }
            _ if self.loading => Vec::new(),
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
        if matches!(
            method,
            "session/request_permission" | "cursor/ask_question" | "cursor/create_plan"
        ) {
            self.open.push(id.to_string());
        }
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

    // Before the session is set up nothing runs to cancel, and the first prompt still waits.
    fn abort(&self, p: &mut Parse) -> Abort {
        let c = &mut p.acp;
        c.followup = None;
        if !c.ready {
            c.queue.clear();
            return Abort::Kill;
        }
        let lines = c
            .cancel()
            .into_iter()
            .filter_map(|o| match o {
                Out::Write(l) => Some(l),
                _ => None,
            })
            .collect();
        Abort::Stdin(lines)
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

    fn answer(&self, d: &Decision, call: &Value, p: &mut Parse) -> Vec<Out> {
        let c = &mut p.acp;
        // a call cancelled meanwhile has had its answer
        let id = call.get("id").map(Value::to_string).unwrap_or_default();
        let Some(i) = c.open.iter().position(|o| *o == id) else {
            return Vec::new();
        };
        c.open.remove(i);
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
        let mut out = vec![Out::Write(
            json!({"jsonrpc": "2.0", "id": call.get("id"), "result": result}).to_string(),
        )];
        // the agent tells the model only "User rejected": the turn ends, and the reason is the next prompt
        if text(call, "kind") == Some("permission")
            && !d.allow
            && let Some(why) = reason
            && c.in_flight()
            && c.session.is_some()
        {
            let title = text(call, "title").unwrap_or("the call");
            c.followup = Some(format!("drover: the user denied {title}: {why}"));
            out.extend(c.cancel());
        }
        out
    }

    fn forward(&self) -> &'static [(&'static str, &'static str)] {
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

#[cfg(test)]
mod tests {
    use super::*;

    const SID: &str = "317266eb-32ce-4e67-9457-7a097c828473";

    fn spec<'a>(
        profile: Profile,
        model: Option<&'a str>,
        resume: Option<&'a SessionId>,
    ) -> StartSpec<'a> {
        StartSpec {
            headless: true,
            profile,
            model,
            permission_mode: None,
            resume,
            cwd: std::path::Path::new("/w"),
            hooks: "",
            hook_timeout: 600,
            forwarded: &[],
        }
    }

    // What the agent writes, one value a line.
    fn feed(h: &Acp, p: &mut Parse, lines: &[Value]) -> Vec<Out> {
        lines
            .iter()
            .flat_map(|l| h.events(&l.to_string(), p))
            .collect()
    }

    fn ok(id: u64, result: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": id, "result": result})
    }

    fn update(u: Value) -> Value {
        json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": SID, "update": u}})
    }

    fn chunk(text: &str) -> Value {
        update(
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}}),
        )
    }

    fn written(outs: &[Out]) -> Vec<Value> {
        outs.iter()
            .filter_map(|o| match o {
                Out::Write(l) => serde_json::from_str(l).ok(),
                _ => None,
            })
            .collect()
    }

    fn at(v: &Value, path: &str) -> Value {
        v.pointer(path).cloned().unwrap_or_default()
    }

    fn line(l: &str) -> Value {
        serde_json::from_str(l).unwrap_or_default()
    }

    // A cursor run, past its setup, with one prompt in flight as request 3.
    fn cursor() -> Result<Parse, &'static str> {
        let mut p = Parse::default();
        CURSOR.open(&spec(Profile::Actor, None, None), &mut p);
        assert!(CURSOR.send("go", &mut p).is_empty());
        feed(
            &CURSOR,
            &mut p,
            &[ok(1, json!({})), ok(2, json!({"sessionId": SID}))],
        );
        if !p.acp.ready {
            return Err("not ready");
        }
        Ok(p)
    }

    #[test]
    fn a_reader_asks_and_the_model_is_picked_before_the_first_prompt() -> Result<(), &'static str> {
        let mut p = Parse::default();
        let open = CURSOR.open(&spec(Profile::Reader, Some("auto"), None), &mut p);
        assert_eq!(
            open.first().map(|l| at(&line(l), "/method")),
            Some(json!("initialize"))
        );
        assert!(CURSOR.send("hi", &mut p).is_empty());
        let new = written(&feed(
            &CURSOR,
            &mut p,
            &[ok(1, json!({"authMethods": [{"id": "cursor_login"}]}))],
        ));
        assert_eq!(
            new.first()
                .map(|l| (at(l, "/method"), at(l, "/params/cwd"))),
            Some((json!("session/new"), json!("/w")))
        );
        let models = json!({"availableModels": [{"modelId": "default[]", "name": "Auto"}, {"modelId": "gpt-5.5[fast=false]", "name": "gpt-5.5"}]});
        let outs = feed(
            &CURSOR,
            &mut p,
            &[ok(2, json!({"sessionId": SID, "models": models}))],
        );
        assert_eq!(
            outs.first(),
            Some(&Out::Session(SessionId::parse(SID).ok_or("sid")?))
        );
        let setup = written(&outs);
        assert_eq!(
            setup.iter().map(|l| at(l, "/method")).collect::<Vec<_>>(),
            [json!("session/set_mode"), json!("session/set_model")]
        );
        assert_eq!(
            (
                setup.first().map(|l| at(l, "/params/modeId")),
                setup.get(1).map(|l| at(l, "/params/modelId"))
            ),
            (Some(json!("ask")), Some(json!("default[]")))
        );
        // the prompt waits for the whole setup
        assert!(feed(&CURSOR, &mut p, &[ok(3, json!({}))]).is_empty());
        let prompt = written(&feed(&CURSOR, &mut p, &[ok(4, json!({}))]));
        assert_eq!(
            prompt.first().map(|l| at(l, "/params/prompt/0/text")),
            Some(json!("hi"))
        );

        let mut p = Parse::default();
        CURSOR.open(&spec(Profile::Actor, Some("nope"), None), &mut p);
        feed(&CURSOR, &mut p, &[ok(1, json!({}))]);
        assert_eq!(
            feed(
                &CURSOR,
                &mut p,
                &[ok(2, json!({"sessionId": SID, "models": models}))]
            ),
            [Out::Fail("the agent has no model nope".into())]
        );
        Ok(())
    }

    // Lines as cursor-agent acp 2026.10.22 writes them, shortened.
    #[test]
    fn a_turn_becomes_events_and_its_last_message_the_reply() -> Result<(), &'static str> {
        let mut p = cursor()?;
        let outs = feed(
            &CURSOR,
            &mut p,
            &[
                update(
                    json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "hm"}}),
                ),
                chunk("Running."),
                update(
                    json!({"sessionUpdate": "tool_call", "toolCallId": "c1", "title": "`ls`", "kind": "execute", "status": "pending", "rawInput": {"command": "ls"}}),
                ),
                update(
                    json!({"sessionUpdate": "tool_call_update", "toolCallId": "c1", "status": "in_progress"}),
                ),
                update(
                    json!({"sessionUpdate": "tool_call_update", "toolCallId": "c1", "status": "completed"}),
                ),
                update(
                    json!({"sessionUpdate": "tool_call", "toolCallId": "c2", "title": "Edit File", "kind": "edit", "status": "pending"}),
                ),
                update(
                    json!({"sessionUpdate": "tool_call_update", "toolCallId": "c2", "status": "failed"}),
                ),
                chunk("PO"),
                chunk("NG"),
                ok(3, json!({"stopReason": "end_turn"})),
            ],
        );
        let shell = || "Shell".to_string();
        assert_eq!(
            outs,
            [
                Out::Text("Running.".into()),
                Out::ToolStart { tool: shell() },
                Out::ToolEnd {
                    tool: shell(),
                    ok: true
                },
                Out::ToolStart {
                    tool: "Edit".into()
                },
                Out::ToolEnd {
                    tool: "Edit".into(),
                    ok: false
                },
                Out::Text("PO".into()),
                Out::Text("NG".into()),
                Out::Reply {
                    text: "PONG".into(),
                    usage: None,
                    error: false
                },
            ]
        );
        CURSOR.send("again", &mut p);
        assert_eq!(
            feed(
                &CURSOR,
                &mut p,
                &[
                    json!({"jsonrpc": "2.0", "id": 4, "error": {"code": -32603, "message": "Internal error", "data": {"message": "rate limited"}}})
                ]
            ),
            [Out::Reply {
                text: "Internal error: rate limited".into(),
                usage: None,
                error: true
            }]
        );
        Ok(())
    }

    fn ask(id: u64, call: &str, title: &str, kind: &str) -> Value {
        let options = json!([{"optionId": "allow-once", "kind": "allow_once"}, {"optionId": "allow-always", "kind": "allow_always"}, {"optionId": "reject-once", "kind": "reject_once"}]);
        json!({"jsonrpc": "2.0", "id": id, "method": "session/request_permission", "params": {"sessionId": SID, "toolCall": {"toolCallId": call, "title": title, "kind": kind}, "options": options}})
    }

    fn decide(allow: bool, reason: Option<&str>) -> Decision {
        Decision {
            allow,
            reason: reason.map(String::from),
            ..Decision::default()
        }
    }

    fn first(calls: &[Value]) -> Value {
        calls.first().cloned().unwrap_or_default()
    }

    fn calls(outs: &[Out]) -> Vec<Value> {
        outs.iter()
            .filter_map(|o| match o {
                Out::Permission { call, .. } => Some(call.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn what_the_agent_asks_is_a_needs_answered_on_stdin() -> Result<(), &'static str> {
        let mut p = cursor()?;
        let outs = feed(
            &CURSOR,
            &mut p,
            &[
                update(
                    json!({"sessionUpdate": "tool_call", "toolCallId": "c1", "title": "`touch x`", "kind": "execute", "status": "pending", "rawInput": {"command": "touch x"}}),
                ),
                ask(0, "c1", "`touch x`", "execute"),
            ],
        );
        let Some(Out::Permission { tool, input, call }) = outs.last() else {
            return Err("no permission");
        };
        assert_eq!(
            (tool.as_str(), input),
            (
                "Shell",
                &json!({"command": "touch x", "title": "`touch x`"})
            )
        );
        let allow = written(&CURSOR.answer(&decide(true, None), call, &mut p));
        assert_eq!(
            allow,
            [
                json!({"jsonrpc": "2.0", "id": 0, "result": {"outcome": {"outcome": "selected", "optionId": "allow-once"}}})
            ]
        );
        // answered once only
        assert!(CURSOR.answer(&decide(false, None), call, &mut p).is_empty());

        // two at once, as cursor asks them; an edit names its file
        let mut edit = ask(2, "c9", "Write /w/.cursor/a.json", "edit");
        if let Some(c) = edit.pointer_mut("/params/toolCall")
            && let Value::Object(c) = c
        {
            c.insert("content".into(), json!([{"type": "diff", "path": "/w/.cursor/a.json", "oldText": null, "newText": "{}"}]));
        }
        let both = feed(
            &CURSOR,
            &mut p,
            &[ask(1, "c2", "`touch y`", "execute"), edit],
        );
        assert!(
            matches!(both.last(), Some(Out::Permission { tool, input, .. }) if tool == "Edit" && at(input, "/path") == json!("/w/.cursor/a.json"))
        );
        let both = calls(&both);
        // a deny's reason: the call is rejected, the turn cancelled with the other call, and the
        // reason is the next prompt
        let deny = CURSOR.answer(&decide(false, Some("not now")), &first(&both), &mut p);
        assert_eq!(
            written(&deny)
                .iter()
                .map(|l| (at(l, "/id"), at(l, "/method"), at(l, "/result/outcome")))
                .collect::<Vec<_>>(),
            [
                (
                    json!(1),
                    Value::Null,
                    json!({"outcome": "selected", "optionId": "reject-once"})
                ),
                (Value::Null, json!("session/cancel"), Value::Null),
                (json!(2), Value::Null, json!({"outcome": "cancelled"})),
            ]
        );
        assert_eq!(deny.last(), Some(&Out::Cancel));
        assert!(
            CURSOR
                .answer(
                    &decide(true, None),
                    &both.get(1).cloned().unwrap_or_default(),
                    &mut p
                )
                .is_empty()
        );
        let next = written(&feed(
            &CURSOR,
            &mut p,
            &[ok(3, json!({"stopReason": "cancelled"}))],
        ));
        assert_eq!(
            next.first().map(|l| at(l, "/params/prompt/0/text")),
            Some(json!("drover: the user denied `touch y`: not now"))
        );
        let reply = feed(
            &CURSOR,
            &mut p,
            &[
                chunk("not now, then."),
                ok(4, json!({"stopReason": "end_turn"})),
            ],
        );
        assert_eq!(
            reply.last(),
            Some(&Out::Reply {
                text: "not now, then.".into(),
                usage: None,
                error: false
            })
        );

        // with no turn running, a deny's reason has nothing to cancel and is not kept
        let late = calls(&feed(
            &CURSOR,
            &mut p,
            &[ask(3, "c3", "`touch z`", "execute")],
        ));
        let deny = CURSOR.answer(&decide(false, Some("no")), &first(&late), &mut p);
        assert_eq!(deny.len(), 1);
        assert!(p.acp.followup.is_none());

        // a question takes your text: an option's label picks it, other text is the answer as written
        let questions = json!([{"id": "q1", "prompt": "Which?", "options": [{"id": "a", "label": "Agent"}, {"id": "p", "label": "Plan"}]}]);
        let said = |id: u64, text: &str, p: &mut Parse| {
            let asked = feed(
                &CURSOR,
                p,
                &[
                    json!({"jsonrpc": "2.0", "id": id, "method": "cursor/ask_question", "params": {"toolCallId": "q", "questions": questions}}),
                ],
            );
            assert!(matches!(asked.last(), Some(Out::Permission { tool, .. }) if tool == QUESTION));
            written(&CURSOR.answer(&decide(true, Some(text)), &first(&calls(&asked)), p))
                .first()
                .map(|l| at(l, "/result/outcome"))
        };
        assert_eq!(
            said(4, "plan", &mut p),
            Some(
                json!({"outcome": "answered", "answers": [{"questionId": "q1", "selectedOptionIds": ["p"]}]})
            )
        );
        assert_eq!(
            said(5, "neither, use ask", &mut p),
            Some(json!({"outcome": "skipped", "reason": "neither, use ask"}))
        );

        let plan = feed(
            &CURSOR,
            &mut p,
            &[
                json!({"jsonrpc": "2.0", "id": 6, "method": "cursor/create_plan", "params": {"toolCallId": "p", "plan": "1. go", "todos": []}}),
            ],
        );
        assert!(matches!(plan.last(), Some(Out::Permission { tool, .. }) if tool == "CreatePlan"));
        let rejected = written(&CURSOR.answer(
            &decide(false, Some("smaller")),
            &first(&calls(&plan)),
            &mut p,
        ));
        assert_eq!(
            rejected
                .iter()
                .map(|l| at(l, "/result"))
                .collect::<Vec<_>>(),
            [json!({"outcome": {"outcome": "rejected", "reason": "smaller"}})]
        );

        // anything else it asks for, drover does not serve
        let fs = written(&feed(
            &CURSOR,
            &mut p,
            &[json!({"jsonrpc": "2.0", "id": 7, "method": "fs/read_text_file", "params": {}})],
        ));
        assert_eq!(
            fs.first().map(|l| (at(l, "/id"), at(l, "/error/code"))),
            Some((json!(7), json!(-32601)))
        );
        // stop: session/cancel, and what still waits is cancelled
        feed(&CURSOR, &mut p, &[ask(8, "c8", "`ls`", "execute")]);
        let Abort::Stdin(stop) = CURSOR.abort(&mut p) else {
            return Err("stop kills");
        };
        assert_eq!(
            stop.iter()
                .map(|l| (at(&line(l), "/method"), at(&line(l), "/id")))
                .collect::<Vec<_>>(),
            [
                (json!("session/cancel"), Value::Null),
                (Value::Null, json!(8))
            ]
        );
        Ok(())
    }

    // Stopped before its session is set up, the run sends nothing: the first prompt goes too.
    #[test]
    fn a_stop_during_setup_kills() {
        let mut p = Parse::default();
        CURSOR.open(&spec(Profile::Actor, None, None), &mut p);
        CURSOR.send("go", &mut p);
        assert!(matches!(CURSOR.abort(&mut p), Abort::Kill));
        assert!(p.acp.queue.is_empty());
    }

    // pi-acp's session is pi's: it loads, its history replayed as updates that are no turn's.
    #[test]
    fn a_resumed_session_loads_without_its_history() -> Result<(), &'static str> {
        let sid = SessionId::parse(SID).ok_or("sid")?;
        let mut p = Parse::default();
        PI.open(&spec(Profile::Actor, None, Some(&sid)), &mut p);
        PI.send("again", &mut p);
        let load = written(&feed(
            &PI,
            &mut p,
            &[ok(1, json!({"agentCapabilities": {"loadSession": true}}))],
        ));
        assert_eq!(
            load.first()
                .map(|l| (at(l, "/method"), at(l, "/params/sessionId"))),
            Some((json!("session/load"), json!(SID)))
        );
        let outs = feed(
            &PI,
            &mut p,
            &[
                chunk("old text"),
                update(
                    json!({"sessionUpdate": "usage_update", "used": 10, "size": 100, "cost": {"amount": 0.5, "currency": "USD"}}),
                ),
                ok(2, json!({})),
            ],
        );
        assert_eq!(outs.first(), Some(&Out::Session(sid.clone())));
        let prompt = written(&outs);
        assert_eq!(
            prompt.first().map(|l| at(l, "/params/prompt/0/text")),
            Some(json!("again"))
        );
        let reply = feed(
            &PI,
            &mut p,
            &[
                chunk("new"),
                update(
                    json!({"sessionUpdate": "usage_update", "used": 20, "size": 100, "cost": {"amount": 0.75, "currency": "USD"}}),
                ),
                ok(
                    3,
                    json!({"stopReason": "end_turn", "usage": {"totalTokens": 12, "inputTokens": 10, "outputTokens": 2}}),
                ),
            ],
        );
        assert_eq!(
            reply.last(),
            Some(&Out::Reply {
                text: "new".into(),
                usage: Some(Usage {
                    input: 10,
                    output: 2,
                    cache_read: 0,
                    cache_write: 0,
                    cost_usd: 0.25
                }),
                error: false,
            })
        );

        let mut p = Parse::default();
        PI.open(&spec(Profile::Actor, None, Some(&sid)), &mut p);
        assert_eq!(
            feed(&PI, &mut p, &[ok(1, json!({}))]),
            [Out::Fail("the agent cannot load a session".into())]
        );
        Ok(())
    }

    #[test]
    fn command_lines() -> Result<(), &'static str> {
        let sid = SessionId::parse(SID).ok_or("sid")?;
        assert_eq!(
            CURSOR.start(&spec(Profile::Actor, None, None)),
            ["cursor-agent", "acp"]
        );
        let keyed = StartSpec {
            forwarded: &["CURSOR_API_KEY"],
            ..spec(Profile::Actor, None, None)
        };
        assert_eq!(
            CURSOR.start(&keyed),
            [
                "env",
                "AGENT_CLI_CREDENTIAL_STORE=memory",
                "cursor-agent",
                "acp"
            ]
        );
        let pane = |h: &Acp| {
            h.start(&StartSpec {
                headless: false,
                ..spec(Profile::Actor, None, Some(&sid))
            })
        };
        assert_eq!(pane(&CURSOR), ["cursor-agent", "--resume", SID]);
        assert_eq!(pane(&PI), ["pi", "--session", SID]);
        assert_eq!(PI.start(&spec(Profile::Reader, None, None)), ["pi-acp"]);
        Ok(())
    }
}
