// pi: `pi --mode rpc` for headless runs, the TUI for pane runs. pi has no hooks of its own, so
// both load harness/pi.ts (installed in ~/.pi/agent/extensions), which posts to drover and holds a
// tool call that changes something until drover answers. The headless stream carries the rest.

use serde::Deserialize;
use serde_json::{Value, json};

use super::{Abort, Answer, Caps, Decision, Harness, Out, Parse, StartSpec};
use crate::{
    herdr::SessionId,
    run::{Profile, Usage},
};

pub struct Pi;

impl Harness for Pi {
    fn caps(&self) -> Caps {
        Caps {
            headless: true,
            resume: true,
            approvals: true,
            usage: true,
            hooks: true,
            warm: true,
        }
    }

    fn start(&self, spec: &StartSpec) -> Vec<String> {
        let mut argv = vec!["pi".to_string()];
        if spec.headless {
            argv.extend(["--mode", "rpc"].map(String::from));
        }
        argv.extend(["--drover-hook".into(), spec.hooks.into()]);
        if spec.profile == Profile::Reader {
            argv.push("--drover-reader".into());
        }
        if let Some(m) = spec.model {
            argv.extend(["--model".into(), m.into()]);
        }
        if let Some(s) = spec.resume {
            argv.extend(["--session".into(), s.as_str().into()]);
        }
        argv
    }

    fn send(&self, text: &str) -> String {
        json!({"type": "prompt", "message": text}).to_string()
    }

    fn respond(&self, d: &Decision) -> Answer {
        Answer::Hook(json!({"allow": d.allow, "reason": d.reason}))
    }

    fn abort(&self) -> Abort {
        Abort::Kill
    }

    fn events(&self, line: &str, p: &mut Parse) -> Vec<Out> {
        let Ok(l) = serde_json::from_str::<Line>(line) else {
            return Vec::new();
        };
        match l.kind.as_str() {
            "message_update" => match l.delta {
                Some(d) if d.kind == "text_delta" => vec![Out::Text(d.delta)],
                _ => Vec::new(),
            },
            "tool_execution_start" => {
                let tool = l.tool_name.unwrap_or_default();
                if let Some(id) = l.tool_call_id {
                    p.tools.insert(id, tool.clone());
                }
                vec![Out::ToolStart { tool }]
            }
            "tool_execution_end" => {
                let tool = l
                    .tool_call_id
                    .and_then(|id| p.tools.remove(&id))
                    .or(l.tool_name)
                    .unwrap_or_default();
                vec![Out::ToolEnd {
                    tool,
                    ok: !l.is_error,
                }]
            }
            // a retry or compaction goes on in a later agent_end
            "agent_end" if !l.will_retry => {
                let mut usage = Usage::default();
                for u in l.messages.iter().filter_map(|m| m.usage.as_ref()) {
                    usage += &Usage {
                        input: u.input,
                        output: u.output,
                        cache_read: u.cache_read,
                        cache_write: u.cache_write,
                        cost_usd: u.cost.total,
                    };
                }
                let last = l.messages.iter().rfind(|m| m.role == "assistant");
                let error = last.is_some_and(|m| m.stop_reason.as_deref() == Some("error"));
                let text = last
                    .map(|m| {
                        let text = m.text();
                        match (&m.error_message, text.is_empty()) {
                            (Some(e), true) => e.clone(),
                            _ => text,
                        }
                    })
                    .unwrap_or_default();
                vec![Out::Reply {
                    text,
                    usage: Some(usage),
                    error,
                }]
            }
            // a prompt pi refused: no turn follows
            "response" if l.command.as_deref() == Some("prompt") && l.success == Some(false) => {
                vec![Out::Reply {
                    text: l.error.unwrap_or_default(),
                    usage: None,
                    error: true,
                }]
            }
            _ => Vec::new(),
        }
    }

    // harness/pi.ts is the hook, and takes the URL from the command line
    fn hook_settings(&self, _spec: &StartSpec) -> Value {
        Value::Null
    }

    fn hook(&self, body: &Value) -> Vec<Out> {
        let text = |k: &str| body.get(k).and_then(Value::as_str);
        let tool = || text("tool").unwrap_or_default().to_string();
        let mut out: Vec<Out> = text("session_id")
            .and_then(SessionId::parse)
            .map(Out::Session)
            .into_iter()
            .collect();
        out.extend(match text("event").unwrap_or_default() {
            "agent_start" => Some(Out::Prompt),
            "tool_execution_start" => Some(Out::ToolStart { tool: tool() }),
            "tool_execution_end" => Some(Out::ToolEnd {
                tool: tool(),
                ok: body.get("ok").and_then(Value::as_bool).unwrap_or(false),
            }),
            "permission" => Some(Out::Permission {
                tool: tool(),
                input: body.get("input").cloned().unwrap_or_default(),
            }),
            "agent_settled" => Some(Out::Reply {
                text: text("text").unwrap_or_default().into(),
                usage: None,
                error: body.get("error").and_then(Value::as_bool).unwrap_or(false),
            }),
            // /new, /resume and /fork replace the session in the same TUI
            "session_shutdown" if text("reason") == Some("quit") => Some(Out::End),
            "session_shutdown" => Some(Out::Rekey),
            _ => None,
        });
        out
    }
}

#[derive(Deserialize)]
struct Line {
    #[serde(rename = "type")]
    kind: String,
    #[serde(rename = "assistantMessageEvent")]
    delta: Option<Delta>,
    #[serde(rename = "toolCallId")]
    tool_call_id: Option<String>,
    #[serde(rename = "toolName")]
    tool_name: Option<String>,
    #[serde(rename = "isError", default)]
    is_error: bool,
    #[serde(rename = "willRetry", default)]
    will_retry: bool,
    #[serde(default)]
    messages: Vec<Message>,
    command: Option<String>,
    success: Option<bool>,
    error: Option<String>,
}

#[derive(Deserialize)]
struct Delta {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    delta: String,
}

#[derive(Deserialize)]
struct Message {
    role: String,
    content: Option<Content>,
    usage: Option<Tokens>,
    #[serde(rename = "stopReason")]
    stop_reason: Option<String>,
    #[serde(rename = "errorMessage")]
    error_message: Option<String>,
}

// A user message's content may be a string; only an assistant's blocks matter.
#[derive(Deserialize)]
#[serde(untagged)]
enum Content {
    Blocks(Vec<Block>),
    Other(serde::de::IgnoredAny),
}

#[derive(Deserialize)]
struct Block {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: String,
}

#[derive(Deserialize)]
struct Tokens {
    #[serde(default)]
    input: u64,
    #[serde(default)]
    output: u64,
    #[serde(rename = "cacheRead", default)]
    cache_read: u64,
    #[serde(rename = "cacheWrite", default)]
    cache_write: u64,
    #[serde(default)]
    cost: Cost,
}

#[derive(Deserialize, Default)]
struct Cost {
    #[serde(default)]
    total: f64,
}

impl Message {
    fn text(&self) -> String {
        match &self.content {
            Some(Content::Blocks(b)) => b
                .iter()
                .filter(|b| b.kind == "text")
                .map(|b| b.text.as_str())
                .collect(),
            _ => String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(headless: bool, profile: Profile) -> StartSpec<'static> {
        StartSpec {
            headless,
            profile,
            model: Some("m"),
            resume: None,
            hooks: "http://127.0.0.1:7341/hooks/r-00000a",
            hook_timeout: 600,
            drover: "drover",
        }
    }

    #[test]
    fn the_command_line() {
        let a = Pi.start(&spec(true, Profile::Actor));
        assert_eq!(
            a.get(1..3),
            Some(&["--mode".to_string(), "rpc".to_string()][..])
        );
        assert!(a.contains(&"--drover-hook".to_string()) && !a.contains(&"--drover-reader".into()));
        let r = Pi.start(&spec(false, Profile::Reader));
        assert!(!r.contains(&"--mode".to_string()) && r.contains(&"--drover-reader".into()));
    }

    // Lines as `pi --mode rpc` 0.85 writes them, shortened.
    #[test]
    fn the_stream_becomes_events() {
        let lines = [
            r#"{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"Hi"}}"#,
            r#"{"type":"message_update","assistantMessageEvent":{"type":"toolcall_delta","delta":"{"}}"#,
            r#"{"type":"tool_execution_start","toolCallId":"c1","toolName":"bash","args":{}}"#,
            r#"{"type":"tool_execution_end","toolCallId":"c1","toolName":"bash","isError":true}"#,
            r#"{"type":"agent_end","willRetry":true,"messages":[]}"#,
            r#"{"type":"agent_end","willRetry":false,"messages":[
                {"role":"user","content":"go"},
                {"role":"assistant","content":[{"type":"text","text":"a "}],"usage":{"input":4,"output":2,"cacheRead":1,"cacheWrite":3,"cost":{"total":0.5}}},
                {"role":"assistant","content":[{"type":"text","text":"Done."}],"usage":{"input":1,"output":1,"cost":{"total":0.25}}}]}"#,
            r#"{"type":"response","command":"prompt","success":false,"error":"no model"}"#,
        ];
        let mut p = Parse::default();
        let out: Vec<Out> = lines
            .iter()
            .flat_map(|l| Pi.events(&l.replace('\n', ""), &mut p))
            .collect();
        assert_eq!(
            out,
            [
                Out::Text("Hi".into()),
                Out::ToolStart {
                    tool: "bash".into()
                },
                Out::ToolEnd {
                    tool: "bash".into(),
                    ok: false
                },
                Out::Reply {
                    text: "Done.".into(),
                    usage: Some(Usage {
                        input: 5,
                        output: 3,
                        cache_read: 1,
                        cache_write: 3,
                        cost_usd: 0.75
                    }),
                    error: false
                },
                Out::Reply {
                    text: "no model".into(),
                    usage: None,
                    error: true
                },
            ]
        );
    }

    #[test]
    fn hooks_become_events() {
        let sid = "317266eb-32ce-4e67-9457-7a097c828473";
        let hook = |mut b: Value| {
            if let Value::Object(o) = &mut b {
                o.insert("session_id".into(), json!(sid));
            }
            Pi.hook(&b)
        };
        assert_eq!(hook(json!({"event": "session_start"})).len(), 1);
        assert_eq!(
            hook(json!({"event": "permission", "tool": "bash", "input": {"command": "ls"}})).last(),
            Some(&Out::Permission {
                tool: "bash".into(),
                input: json!({"command": "ls"})
            })
        );
        assert_eq!(
            hook(json!({"event": "session_shutdown", "reason": "quit"})).last(),
            Some(&Out::End)
        );
        assert_eq!(
            hook(json!({"event": "session_shutdown", "reason": "new"})).last(),
            Some(&Out::Rekey)
        );
        let deny = Pi.respond(&Decision {
            allow: false,
            reason: Some("not now".into()),
        });
        assert!(
            matches!(deny, Answer::Hook(v) if v == json!({"allow": false, "reason": "not now"}))
        );
    }
}
