// Claude Code: `claude -p` with stream-json both ways for headless runs, the TUI for pane runs.
// Both get `http` hooks to drover through --settings: a pane run all of its events, a headless
// run only PermissionRequest, since its stream carries the rest. See SPIKE.md.

use serde::Deserialize;
use serde_json::{Value, json};

use super::{Abort, Answer, Caps, Decision, Harness, Out, Parse, StartSpec};
use crate::{
    herdr::SessionId,
    run::{Profile, Usage},
};

pub struct Claude;

// A reader reads the brain and files, nothing else. --restricted drops your own settings files,
// so your allowlists do not reach it; dontAsk denies whatever these rules do not allow.
const READER_ALLOW: [&str; 5] = [
    "Bash(brain find *)",
    "Bash(brain get *)",
    "Bash(brain pages *)",
    "Bash(brain code *)",
    "Read",
];

// Seconds a hook that only reports may take.
const REPORT_TIMEOUT: u64 = 10;

impl Harness for Claude {
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
        let mut argv = vec!["claude".to_string()];
        if spec.headless {
            argv.extend(
                [
                    "-p",
                    "--input-format",
                    "stream-json",
                    "--output-format",
                    "stream-json",
                    "--verbose",
                    "--include-partial-messages",
                ]
                .map(String::from),
            );
        }
        // the user's default may be auto mode, which would answer for them (SPIKE.md q5)
        let profile: &[&str] = match spec.profile {
            Profile::Actor => &["--permission-mode", "default"],
            Profile::Reader => &[
                "--restricted",
                "--tools",
                "Bash,Read",
                "--permission-mode",
                "dontAsk",
            ],
        };
        argv.extend(profile.iter().map(|s| s.to_string()));
        if let Some(m) = spec.model {
            argv.extend(["--model".into(), m.into()]);
        }
        if let Some(s) = spec.resume {
            argv.extend(["--resume".into(), s.as_str().into()]);
        }
        argv.extend(["--settings".into(), self.hook_settings(spec).to_string()]);
        argv
    }

    fn send(&self, text: &str) -> String {
        json!({"type": "user", "message": {"role": "user", "content": text}}).to_string()
    }

    fn respond(&self, d: &Decision) -> Answer {
        let decision = match (d.allow, &d.reason) {
            (true, _) => json!({"behavior": "allow"}),
            (false, reason) => json!({
                "behavior": "deny",
                "message": reason.as_deref().unwrap_or("denied by the user"),
            }),
        };
        Answer::Hook(json!({
            "hookSpecificOutput": {"hookEventName": "PermissionRequest", "decision": decision}
        }))
    }

    fn abort(&self) -> Abort {
        Abort::Kill
    }

    fn events(&self, line: &str, p: &mut Parse) -> Vec<Out> {
        let Ok(l) = serde_json::from_str::<Line>(line) else {
            return Vec::new();
        };
        match l.kind.as_str() {
            "system" if l.subtype.as_deref() == Some("init") => l
                .session_id
                .and_then(|s| SessionId::parse(&s))
                .map(Out::Session)
                .into_iter()
                .collect(),
            "stream_event" => {
                let delta = l.event.as_ref().and_then(|e| e.get("delta"));
                match delta.and_then(|d| Some((d.get("type")?.as_str()?, d.get("text")?.as_str()?)))
                {
                    Some(("text_delta", text)) => vec![Out::Text(text.into())],
                    _ => Vec::new(),
                }
            }
            "assistant" => l
                .blocks()
                .filter(|b| b.kind == "tool_use")
                .filter_map(|b| {
                    let (id, tool) = (b.id.clone()?, b.name.clone()?);
                    p.tools.insert(id, tool.clone());
                    Some(Out::ToolStart { tool })
                })
                .collect(),
            "user" => l
                .blocks()
                .filter(|b| b.kind == "tool_result")
                .map(|b| Out::ToolEnd {
                    tool: b
                        .tool_use_id
                        .as_ref()
                        .and_then(|id| p.tools.remove(id))
                        .unwrap_or_default(),
                    ok: !b.is_error,
                })
                .collect(),
            "result" => {
                // usage is the turn's, total_cost_usd the process's so far
                let total = l.total_cost_usd.unwrap_or(p.cost);
                let u = l.usage.unwrap_or_default();
                let usage = Usage {
                    input: u.input_tokens,
                    output: u.output_tokens,
                    cache_read: u.cache_read_input_tokens,
                    cache_write: u.cache_creation_input_tokens,
                    cost_usd: (total - p.cost).max(0.0),
                };
                p.cost = total;
                vec![Out::Reply {
                    text: l.result.unwrap_or_default(),
                    usage: Some(usage),
                    error: l.is_error,
                }]
            }
            _ => Vec::new(),
        }
    }

    fn hook_settings(&self, spec: &StartSpec) -> Value {
        let hook = |timeout: u64| json!([{"hooks": [{"type": "http", "url": spec.hooks, "timeout": timeout}]}]);
        let mut hooks = serde_json::Map::new();
        if spec.profile == Profile::Actor {
            hooks.insert("PermissionRequest".into(), hook(spec.hook_timeout));
        }
        if !spec.headless {
            for name in [
                "UserPromptSubmit",
                "PreToolUse",
                "PostToolUse",
                "PostToolUseFailure",
                "Stop",
                "SessionEnd",
            ] {
                hooks.insert(name.into(), hook(REPORT_TIMEOUT));
            }
        }
        let mut settings = json!({ "hooks": hooks });
        if spec.profile == Profile::Reader
            && let Value::Object(s) = &mut settings
        {
            s.insert("permissions".into(), json!({ "allow": READER_ALLOW }));
        }
        settings
    }

    fn hook(&self, body: &Value) -> Vec<Out> {
        let text = |k: &str| body.get(k).and_then(Value::as_str);
        let tool = || text("tool_name").unwrap_or_default().to_string();
        let mut out: Vec<Out> = text("session_id")
            .and_then(SessionId::parse)
            .map(Out::Session)
            .into_iter()
            .collect();
        out.extend(match text("hook_event_name").unwrap_or_default() {
            "UserPromptSubmit" => Some(Out::Prompt),
            "PreToolUse" => Some(Out::ToolStart { tool: tool() }),
            "PostToolUse" => Some(Out::ToolEnd {
                tool: tool(),
                ok: true,
            }),
            "PostToolUseFailure" => Some(Out::ToolEnd {
                tool: tool(),
                ok: false,
            }),
            "PermissionRequest" => Some(Out::Permission {
                tool: tool(),
                input: body.get("tool_input").cloned().unwrap_or_default(),
            }),
            "Stop" => Some(Out::Reply {
                text: text("last_assistant_message").unwrap_or_default().into(),
                usage: None,
                error: false,
            }),
            // /clear starts a new session in the same TUI; its next prompt brings the new id
            "SessionEnd" if text("reason") == Some("clear") => Some(Out::Rekey),
            "SessionEnd" => Some(Out::End),
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
    event: Option<Value>,
    message: Option<Message>,
    result: Option<String>,
    #[serde(default)]
    is_error: bool,
    total_cost_usd: Option<f64>,
    usage: Option<Tokens>,
}

#[derive(Deserialize)]
struct Message {
    content: Option<Content>,
}

// A prompt echoed back is a string; only blocks matter.
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
    id: Option<String>,
    name: Option<String>,
    tool_use_id: Option<String>,
    #[serde(default)]
    is_error: bool,
}

#[derive(Deserialize, Default)]
struct Tokens {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
}

impl Line {
    fn blocks(&self) -> impl Iterator<Item = &Block> {
        let blocks = match self.message.as_ref().and_then(|m| m.content.as_ref()) {
            Some(Content::Blocks(b)) => b.as_slice(),
            _ => &[],
        };
        blocks.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(headless: bool, profile: Profile) -> StartSpec<'static> {
        StartSpec {
            headless,
            profile,
            model: Some("haiku"),
            resume: None,
            hooks: "http://127.0.0.1:7341/hooks/r-00000a",
            hook_timeout: 600,
            drover: "drover",
        }
    }

    fn flag<'a>(argv: &'a [String], name: &str) -> Option<&'a str> {
        let i = argv.iter().position(|a| a == name)?;
        argv.get(i + 1).map(String::as_str)
    }

    // Claude's Bash rule `Bash(prefix *)`: the command starts with the prefix and a space.
    fn allows(rules: &[&str], tool: &str, command: &str) -> bool {
        rules.iter().any(
            |r| match r.strip_prefix("Bash(").and_then(|r| r.strip_suffix(" *)")) {
                Some(prefix) => tool == "Bash" && command.starts_with(&format!("{prefix} ")),
                None => *r == tool,
            },
        )
    }

    // A reader run cannot ssh or write to the brain: none of its rules allow it, dontAsk denies
    // what they do not allow, and --restricted keeps your own allowlists out.
    #[test]
    fn a_reader_cannot_act() -> Result<(), Box<dyn std::error::Error>> {
        for headless in [true, false] {
            let argv = Claude.start(&spec(headless, Profile::Reader));
            assert_eq!(flag(&argv, "--permission-mode"), Some("dontAsk"));
            assert_eq!(flag(&argv, "--tools"), Some("Bash,Read"));
            assert!(argv.iter().any(|a| a == "--restricted"));
            let settings: Value =
                serde_json::from_str(flag(&argv, "--settings").unwrap_or_default())?;
            let rules: Vec<&str> = settings
                .pointer("/permissions/allow")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            assert_eq!(rules, READER_ALLOW);
            assert!(
                settings.pointer("/permissions/deny").is_none()
                    && settings.pointer("/permissions/defaultMode").is_none()
            );
            // nothing for a reader to ask: it never reaches you
            assert!(settings.pointer("/hooks/PermissionRequest").is_none());
            for denied in [
                "ssh dog",
                "ssh -o x dog true",
                "brain add x",
                "brain label x y",
                "brain log",
                "brain wiki merge a b",
                "curl https://x",
            ] {
                assert!(!allows(&rules, "Bash", denied), "{denied}");
            }
            for tool in ["Edit", "Write", "WebFetch"] {
                assert!(!allows(&rules, tool, ""), "{tool}");
            }
            assert!(allows(&rules, "Bash", "brain find robots") && allows(&rules, "Read", ""));
        }
        Ok(())
    }

    #[test]
    fn an_actor_asks_drover() -> Result<(), Box<dyn std::error::Error>> {
        let headless = Claude.start(&spec(true, Profile::Actor));
        assert_eq!(headless.get(1).map(String::as_str), Some("-p"));
        assert_eq!(flag(&headless, "--permission-mode"), Some("default"));
        assert_eq!(flag(&headless, "--model"), Some("haiku"));
        let s: Value = serde_json::from_str(flag(&headless, "--settings").unwrap_or_default())?;
        assert_eq!(
            s.pointer("/hooks/PermissionRequest/0/hooks/0/timeout"),
            Some(&json!(600))
        );
        assert_eq!(
            s.get("hooks")
                .and_then(Value::as_object)
                .map(serde_json::Map::len),
            Some(1)
        );

        let pane = Claude.start(&spec(false, Profile::Actor));
        assert!(!pane.iter().any(|a| a == "-p"));
        let s: Value = serde_json::from_str(flag(&pane, "--settings").unwrap_or_default())?;
        assert_eq!(
            s.pointer("/hooks/Stop/0/hooks/0/url"),
            Some(&json!("http://127.0.0.1:7341/hooks/r-00000a"))
        );
        Ok(())
    }

    // Lines as `claude -p` 2.1 writes them, shortened.
    #[test]
    fn the_stream_becomes_events() -> Result<(), &'static str> {
        let lines = [
            r#"{"type":"system","subtype":"init","session_id":"317266eb-32ce-4e67-9457-7a097c828473"}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hi"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"{"}}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{}}]}}"#,
            r#"{"type":"user","message":{"content":[{"tool_use_id":"t1","type":"tool_result","content":"no","is_error":true}]}}"#,
            r#"{"type":"user","message":{"content":"a prompt echoed back"}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"Done.","total_cost_usd":0.5,"usage":{"input_tokens":4,"output_tokens":86,"cache_read_input_tokens":10,"cache_creation_input_tokens":20}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"Again.","total_cost_usd":0.75,"usage":{"input_tokens":1}}"#,
            "not json",
        ];
        let mut p = Parse::default();
        let out: Vec<Out> = lines
            .iter()
            .flat_map(|l| Claude.events(l, &mut p))
            .collect();
        let usage = |input, output, cache_read, cache_write, cost_usd| {
            Some(Usage {
                input,
                output,
                cache_read,
                cache_write,
                cost_usd,
            })
        };
        assert_eq!(
            out,
            [
                Out::Session(
                    SessionId::parse("317266eb-32ce-4e67-9457-7a097c828473").ok_or("sid")?
                ),
                Out::Text("Hi".into()),
                Out::ToolStart {
                    tool: "Bash".into()
                },
                Out::ToolEnd {
                    tool: "Bash".into(),
                    ok: false
                },
                Out::Reply {
                    text: "Done.".into(),
                    usage: usage(4, 86, 10, 20, 0.5),
                    error: false
                },
                Out::Reply {
                    text: "Again.".into(),
                    usage: usage(1, 0, 0, 0, 0.25),
                    error: false
                },
            ]
        );
        Ok(())
    }

    #[test]
    fn hooks_become_events() {
        let sid = "317266eb-32ce-4e67-9457-7a097c828473";
        let hook = |name: &str, extra: Value| {
            let mut b = json!({"session_id": sid, "hook_event_name": name});
            if let (Value::Object(b), Value::Object(e)) = (&mut b, extra) {
                b.extend(e);
            }
            Claude.hook(&b).into_iter().last()
        };
        assert_eq!(hook("UserPromptSubmit", json!({})), Some(Out::Prompt));
        assert_eq!(
            hook(
                "PermissionRequest",
                json!({"tool_name": "Bash", "tool_input": {"command": "touch x"}})
            ),
            Some(Out::Permission {
                tool: "Bash".into(),
                input: json!({"command": "touch x"})
            })
        );
        assert_eq!(
            hook("Stop", json!({"last_assistant_message": "Done."})),
            Some(Out::Reply {
                text: "Done.".into(),
                usage: None,
                error: false
            })
        );
        assert_eq!(
            hook("SessionEnd", json!({"reason": "prompt_input_exit"})),
            Some(Out::End)
        );
        assert_eq!(
            hook("SessionEnd", json!({"reason": "clear"})),
            Some(Out::Rekey)
        );
        let deny = Claude.respond(&Decision {
            allow: false,
            reason: Some("not now".into()),
        });
        assert!(
            matches!(deny, Answer::Hook(v) if v.pointer("/hookSpecificOutput/decision/message") == Some(&json!("not now")))
        );
    }
}
