// Cursor: `cursor-agent -p` for headless runs, the TUI for pane runs. Headless takes one prompt
// per process (no stream input, and it reads stdin to EOF), so a turn is a process, resumed by
// session id. Approvals are command hooks in `.cursor/hooks.json` in the run's cwd, which run
// `drover hook <host:port/path>`: it posts to drover and prints cursor's answer. See SPIKE.md q6.

use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::{Abort, Answer, Caps, Decision, Harness, Out, Parse, StartSpec};
use crate::{
    herdr::{SessionId, quote},
    run::{Profile, Usage},
};

pub struct Cursor;

// Seconds a hook that only reports may take.
const REPORT_TIMEOUT: u64 = 10;

impl Harness for Cursor {
    // cursor reports tokens and no cost, so a run's cost_usd stays 0
    fn caps(&self) -> Caps {
        Caps {
            headless: true,
            resume: true,
            approvals: true,
            usage: true,
            hooks: true,
            warm: false,
        }
    }

    fn start(&self, spec: &StartSpec) -> Vec<String> {
        // --trust: the worktree is drover's own, and the TUI's trust dialog would take the first prompt
        let mut argv = vec!["cursor-agent".to_string(), "--trust".to_string()];
        if spec.headless {
            argv.extend(
                [
                    "-p",
                    "--output-format",
                    "stream-json",
                    "--stream-partial-output",
                ]
                .map(String::from),
            );
        }
        // read-only Q&A; an actor's shell and MCP calls are held by its hooks instead
        if spec.profile == Profile::Reader {
            argv.extend(["--mode".into(), "ask".into()]);
        }
        if let Some(m) = spec.model {
            argv.extend(["--model".into(), m.into()]);
        }
        if let Some(s) = spec.resume {
            argv.extend(["--resume".into(), s.as_str().into()]);
        }
        argv
    }

    // the prompt, then EOF
    fn send(&self, text: &str) -> String {
        text.into()
    }

    fn respond(&self, d: &Decision) -> Answer {
        let answer = if d.allow {
            json!({"permission": "allow"})
        } else {
            // the model quotes the user_message (SPIKE.md q6)
            let why = d.reason.as_deref().unwrap_or("denied by the user");
            json!({"permission": "deny", "user_message": why, "agent_message": why})
        };
        Answer::Hook(answer)
    }

    fn abort(&self) -> Abort {
        Abort::Kill
    }

    // With --stream-partial-output an assistant line is a delta (a timestamp), a whole message
    // before a tool call (a timestamp and a model_call_id), or the turn's last message (neither).
    fn events(&self, line: &str, p: &mut Parse) -> Vec<Out> {
        let Ok(l) = serde_json::from_str::<Line>(line) else {
            return Vec::new();
        };
        match (l.kind.as_str(), l.subtype.as_deref()) {
            ("system", Some("init")) => l
                .session_id
                .and_then(|s| SessionId::parse(&s))
                .map(Out::Session)
                .into_iter()
                .collect(),
            ("assistant", _) => {
                let text = l.text();
                match (l.timestamp_ms, l.model_call_id) {
                    (Some(_), None) => vec![Out::Text(text)],
                    (None, _) => {
                        p.last = text;
                        Vec::new()
                    }
                    _ => Vec::new(),
                }
            }
            ("tool_call", Some("started")) => l
                .tool()
                .map(|(tool, _)| Out::ToolStart { tool })
                .into_iter()
                .collect(),
            ("tool_call", Some("completed")) => l
                .tool()
                .map(|(tool, call)| Out::ToolEnd {
                    tool,
                    ok: call
                        .get("result")
                        .is_some_and(|r| r.get("success").is_some()),
                })
                .into_iter()
                .collect(),
            ("result", _) => {
                // all the turn's text is in `result`, the last message alone in `last`
                let last = std::mem::take(&mut p.last);
                let u = l.usage.unwrap_or_default();
                vec![Out::Reply {
                    text: if last.is_empty() {
                        l.result.unwrap_or_default()
                    } else {
                        last
                    },
                    usage: Some(Usage {
                        input: u.input_tokens,
                        output: u.output_tokens,
                        cache_read: u.cache_read_tokens,
                        cache_write: u.cache_write_tokens,
                        cost_usd: 0.0,
                    }),
                    error: l.is_error,
                }]
            }
            _ => Vec::new(),
        }
    }

    fn hook_settings(&self, spec: &StartSpec) -> Value {
        // a hook cursor times out, or that fails with anything but exit 2, lets the call run; so
        // `drover hook` gives up first, a permission hook is closed, and `|| exit 2` covers the
        // binary going missing
        let hook = |timeout: u64, closed: bool| {
            // cursor silently skips a hook command with a URL in it, so no scheme
            let at = spec.hooks.trim_start_matches("http://");
            let (exe, at, secs) = (
                quote(spec.drover),
                quote(at),
                timeout.saturating_sub(1).max(1),
            );
            let command = if closed {
                format!("{exe} hook --closed {at} {secs} || exit 2")
            } else {
                format!("{exe} hook {at} {secs}")
            };
            json!([{"command": command, "timeout": timeout}])
        };
        let mut hooks = Map::new();
        for name in ["beforeShellExecution", "beforeMCPExecution"] {
            hooks.insert(name.into(), hook(spec.hook_timeout, true));
        }
        if !spec.headless {
            for name in [
                "sessionStart",
                "beforeSubmitPrompt",
                "afterAgentResponse",
                "stop",
                "sessionEnd",
            ] {
                hooks.insert(name.into(), hook(REPORT_TIMEOUT, false));
            }
        }
        json!({"version": 1, "hooks": hooks})
    }

    // a reader is in ask mode: nothing for hooks to hold
    fn hook_file(&self, profile: Profile) -> Option<&'static str> {
        (profile == Profile::Actor).then_some(".cursor/hooks.json")
    }

    fn hook(&self, body: &Value) -> Vec<Out> {
        let text = |k: &str| body.get(k).and_then(Value::as_str);
        let mut out: Vec<Out> = text("session_id")
            .and_then(SessionId::parse)
            .map(Out::Session)
            .into_iter()
            .collect();
        out.extend(match text("hook_event_name").unwrap_or_default() {
            "sessionStart" => None,
            "beforeSubmitPrompt" => Some(Out::Prompt),
            "beforeShellExecution" => Some(Out::Permission {
                tool: "Shell".into(),
                input: json!({"command": text("command").unwrap_or_default()}),
            }),
            "beforeMCPExecution" => Some(Out::Permission {
                tool: format!("mcp:{}", text("tool_name").unwrap_or_default()),
                // the arguments arrive as a JSON string
                input: match body.get("tool_input") {
                    Some(Value::String(s)) => serde_json::from_str(s).unwrap_or_default(),
                    other => other.cloned().unwrap_or_default(),
                },
            }),
            // once a turn, with all of its text; stop has none, and ends a turn that had no response
            "afterAgentResponse" => Some(Out::Reply {
                text: text("text").unwrap_or_default().into(),
                usage: None,
                error: false,
            }),
            "stop" if text("status") != Some("completed") => Some(Out::Reply {
                text: String::new(),
                usage: None,
                error: text("status") == Some("error"),
            }),
            "sessionEnd" => Some(Out::End),
            _ => None,
        });
        out
    }
}

#[derive(Deserialize)]
struct Line {
    #[serde(rename = "type")]
    kind: String,
    subtype: Option<String>,
    session_id: Option<String>,
    message: Option<Message>,
    model_call_id: Option<String>,
    timestamp_ms: Option<u64>,
    // `{"shellToolCall": {…}}`: the tool is the key
    tool_call: Option<Map<String, Value>>,
    result: Option<String>,
    #[serde(default)]
    is_error: bool,
    usage: Option<Tokens>,
}

#[derive(Deserialize)]
struct Message {
    #[serde(default)]
    content: Vec<Block>,
}

#[derive(Deserialize)]
struct Block {
    text: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct Tokens {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_read_tokens: u64,
    #[serde(default)]
    cache_write_tokens: u64,
}

impl Line {
    fn text(&self) -> String {
        self.message
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|b| b.text.as_deref())
            .collect()
    }

    // `shellToolCall` is Shell, `readToolCall` Read, …
    fn tool(&self) -> Option<(String, &Value)> {
        let (key, call) = self.tool_call.as_ref()?.iter().next()?;
        let name = key.strip_suffix("ToolCall").unwrap_or(key);
        let mut chars = name.chars();
        let first = chars.next()?.to_uppercase();
        Some((first.chain(chars).collect(), call))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(headless: bool, profile: Profile) -> StartSpec<'static> {
        StartSpec {
            headless,
            profile,
            model: Some("auto"),
            resume: None,
            hooks: "http://127.0.0.1:7341/hooks/r-00000a",
            hook_timeout: 600,
            drover: "/opt/drover/bin/drover",
        }
    }

    #[test]
    fn a_reader_asks_and_an_actor_is_held_by_hooks() {
        let reader = Cursor.start(&spec(true, Profile::Reader));
        assert!(reader.windows(2).any(|w| w == ["--mode", "ask"]));
        assert_eq!(Cursor.hook_file(Profile::Reader), None);

        let actor = Cursor.start(&spec(true, Profile::Actor));
        assert_eq!(actor.get(2).map(String::as_str), Some("-p"));
        assert!(!actor.iter().any(|a| a == "--mode" || a == "--force"));
        assert_eq!(Cursor.hook_file(Profile::Actor), Some(".cursor/hooks.json"));
        let pane = Cursor.start(&spec(false, Profile::Actor));
        assert_eq!(pane, ["cursor-agent", "--trust", "--model", "auto"]);
    }

    #[test]
    fn hooks_run_drover_with_the_runs_url() {
        let s = Cursor.hook_settings(&spec(true, Profile::Actor));
        let command =
            "'/opt/drover/bin/drover' hook --closed '127.0.0.1:7341/hooks/r-00000a' 599 || exit 2";
        for name in ["beforeShellExecution", "beforeMCPExecution"] {
            assert_eq!(
                s.pointer(&format!("/hooks/{name}/0/command")),
                Some(&json!(command))
            );
        }
        assert_eq!(
            s.pointer("/hooks/beforeShellExecution/0/timeout"),
            Some(&json!(600))
        );
        assert!(s.pointer("/hooks/stop").is_none());
        let pane = Cursor.hook_settings(&spec(false, Profile::Actor));
        // a report that fails lets the turn go on
        assert_eq!(
            pane.pointer("/hooks/stop/0/command"),
            Some(&json!(
                "'/opt/drover/bin/drover' hook '127.0.0.1:7341/hooks/r-00000a' 9"
            ))
        );
        assert_eq!(pane.pointer("/hooks/stop/0/timeout"), Some(&json!(10)));
    }

    // Lines as `cursor-agent -p --stream-partial-output` 2026.10 writes them, shortened.
    #[test]
    fn the_stream_becomes_events() -> Result<(), &'static str> {
        let lines = [
            r#"{"type":"system","subtype":"init","session_id":"317266eb-32ce-4e67-9457-7a097c828473","model":"Auto"}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"go"}]}}"#,
            r#"{"type":"thinking","subtype":"delta","text":"hm"}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Run"}]},"timestamp_ms":1}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Running."}]},"model_call_id":"m-0","timestamp_ms":2}"#,
            r#"{"type":"tool_call","subtype":"started","call_id":"c1","tool_call":{"shellToolCall":{"args":{}}}}"#,
            r#"{"type":"tool_call","subtype":"completed","call_id":"c1","tool_call":{"shellToolCall":{"result":{"rejected":{}}}}}"#,
            r#"{"type":"tool_call","subtype":"completed","call_id":"c2","tool_call":{"readToolCall":{"result":{"success":{}}}}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"PONG"}]}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"Running.PONG","usage":{"inputTokens":10,"outputTokens":2,"cacheReadTokens":5,"cacheWriteTokens":1}}"#,
            "not json",
        ];
        let mut p = Parse::default();
        let out: Vec<Out> = lines
            .iter()
            .flat_map(|l| Cursor.events(l, &mut p))
            .collect();
        assert_eq!(
            out,
            [
                Out::Session(
                    SessionId::parse("317266eb-32ce-4e67-9457-7a097c828473").ok_or("sid")?
                ),
                Out::Text("Run".into()),
                Out::ToolStart {
                    tool: "Shell".into()
                },
                Out::ToolEnd {
                    tool: "Shell".into(),
                    ok: false
                },
                Out::ToolEnd {
                    tool: "Read".into(),
                    ok: true
                },
                Out::Reply {
                    text: "PONG".into(),
                    usage: Some(Usage {
                        input: 10,
                        output: 2,
                        cache_read: 5,
                        cache_write: 1,
                        cost_usd: 0.0
                    }),
                    error: false
                },
            ]
        );
        Ok(())
    }

    #[test]
    fn hooks_become_events_and_answers() {
        let sid = "317266eb-32ce-4e67-9457-7a097c828473";
        let hook = |extra: Value| {
            let mut b = json!({"session_id": sid});
            if let (Value::Object(b), Value::Object(e)) = (&mut b, extra) {
                b.extend(e);
            }
            Cursor.hook(&b).into_iter().last()
        };
        assert_eq!(
            hook(
                json!({"hook_event_name": "beforeShellExecution", "command": "touch x", "cwd": ""})
            ),
            Some(Out::Permission {
                tool: "Shell".into(),
                input: json!({"command": "touch x"})
            })
        );
        assert_eq!(
            hook(
                json!({"hook_event_name": "beforeMCPExecution", "tool_name": "lookup", "tool_input": "{\"q\":1}"})
            ),
            Some(Out::Permission {
                tool: "mcp:lookup".into(),
                input: json!({"q": 1})
            })
        );
        assert_eq!(
            hook(json!({"hook_event_name": "afterAgentResponse", "text": "PONG"})),
            Some(Out::Reply {
                text: "PONG".into(),
                usage: None,
                error: false
            })
        );
        // the turn's reply came with afterAgentResponse
        assert!(matches!(
            hook(json!({"hook_event_name": "stop", "status": "completed"})),
            Some(Out::Session(_))
        ));
        assert_eq!(
            hook(json!({"hook_event_name": "stop", "status": "error"})),
            Some(Out::Reply {
                text: String::new(),
                usage: None,
                error: true
            })
        );
        let deny = Cursor.respond(&Decision {
            allow: false,
            reason: Some("not now".into()),
        });
        assert!(matches!(deny, Answer::Hook(v)
            if v == json!({"permission": "deny", "user_message": "not now", "agent_message": "not now"})));
        let allow = Cursor.respond(&Decision {
            allow: true,
            reason: None,
        });
        assert!(matches!(allow, Answer::Hook(v) if v == json!({"permission": "allow"})));
    }
}
