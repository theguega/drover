// Claude Code: `claude -p` with stream-json both ways for headless runs, the TUI for pane runs.
// Both get `http` hooks to drover through --settings: a pane run all of its events, a headless
// run only PermissionRequest, since its stream carries the rest. See SPIKE.md.

use serde::Deserialize;
use serde_json::{Value, json};

use super::{Abort, Caps, Decision, Harness, Out, Parse, StartSpec};
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
            pane: true,
            resume: true,
            approvals: true,
            usage: true,
            hooks: true,
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
        // auto: the classifier runs harmless calls and drover's hook gets what it escalates (SPIKE.md §8)
        let profile: Vec<&str> = match spec.profile {
            Profile::Actor => vec!["--permission-mode", spec.permission_mode.unwrap_or("auto")],
            Profile::Reader => vec![
                "--restricted",
                "--tools",
                "Bash,Read",
                "--permission-mode",
                "dontAsk",
            ],
        };
        argv.extend(profile.iter().map(|s| s.to_string()));
        // -p offers AskUserQuestion and ExitPlanMode only with a permission host. The hook still
        // answers every call first, and Claude then cancels the request it wrote to stdout.
        if spec.headless && spec.profile == Profile::Actor {
            argv.extend(["--permission-prompt-tool".into(), "stdio".into()]);
        }
        if let Some(m) = spec.model {
            argv.extend(["--model".into(), m.into()]);
        }
        if let Some(s) = spec.resume {
            argv.extend(["--resume".into(), s.as_str().into()]);
        }
        argv.extend(["--settings".into(), self.hook_settings(spec).to_string()]);
        argv
    }

    fn send(&self, text: &str, _p: &mut Parse) -> Vec<String> {
        vec![json!({"type": "user", "message": {"role": "user", "content": text}}).to_string()]
    }

    fn respond(&self, d: &Decision) -> Value {
        let decision = match (d.allow, &d.reason) {
            // these two run only with their input back. Not the rest: an updated input is checked
            // against the ask rules again, and a pane then asks in its own dialog
            (true, _) => match &d.call {
                Some((tool, input))
                    if ["ExitPlanMode", "AskUserQuestion"].contains(&tool.as_str()) =>
                {
                    json!({"behavior": "allow", "updatedInput": answered(input, &d.answers)})
                }
                _ => json!({"behavior": "allow"}),
            },
            (false, reason) => json!({
                "behavior": "deny",
                "message": reason.as_deref().unwrap_or("denied by the user"),
            }),
        };
        json!({
            "hookSpecificOutput": {"hookEventName": "PermissionRequest", "decision": decision}
        })
    }

    fn questions(&self, tool: &str, input: &Value) -> Option<usize> {
        let questions = input.get("questions").and_then(Value::as_array);
        (tool == "AskUserQuestion").then(|| questions.map_or(0, Vec::len))
    }

    // Only the backstop's deny comes here: the hook answers every call that reaches drover.
    fn answer(&self, d: &Decision, call: &Value, p: &mut Parse) -> Vec<Out> {
        let Some(id) = call.get("request_id").and_then(Value::as_str) else {
            return Vec::new();
        };
        if !p.held.remove(id) {
            return Vec::new();
        }
        let decision = match d.allow {
            true => json!({"behavior": "allow", "updatedInput": call.get("input")}),
            false => {
                json!({"behavior": "deny", "message": d.reason.as_deref().unwrap_or("denied")})
            }
        };
        vec![Out::Write(
            json!({"type": "control_response", "response": {"subtype": "success", "request_id": id, "response": decision}})
                .to_string(),
        )]
    }

    fn abort(&self, _p: &mut Parse) -> Abort {
        Abort::Kill
    }

    fn events(&self, line: &str, p: &mut Parse) -> Vec<Out> {
        let Ok(l) = serde_json::from_str::<Line>(line) else {
            return Vec::new();
        };
        // a turn's first output: a turn Claude starts itself (a background task ended) is one too
        let mut out = Vec::new();
        if matches!(l.kind.as_str(), "stream_event" | "assistant")
            && l.parent_tool_use_id.is_none()
            && !p.turn
        {
            p.turn = true;
            out.push(Out::Prompt);
        }
        out.extend(match l.kind.as_str() {
            "system" if l.subtype.as_deref() == Some("init") => l
                .session_id
                .and_then(|s| SessionId::parse(&s))
                .map(Out::Session)
                .into_iter()
                .collect(),
            "system" if l.subtype.as_deref() == Some("background_tasks_changed") => {
                vec![Out::Background(l.tasks.map_or(0, |t| t.len()))]
            }
            // written with each PermissionRequest hook, and cancelled once the hook answers; one a
            // hook leaves undecided (drover unreachable, no decision) would hang the turn
            "control_request"
                if l.request.as_ref().and_then(|r| r.get("subtype")?.as_str())
                    == Some("can_use_tool") =>
            {
                let id = l.request_id.unwrap_or_default();
                p.held.insert(id.clone());
                let input = l.request.as_ref().and_then(|r| r.get("input")).cloned();
                vec![Out::Backstop(json!({"request_id": id, "input": input}))]
            }
            "control_cancel_request" => {
                if let Some(id) = &l.request_id {
                    p.held.remove(id);
                }
                Vec::new()
            }
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
            // a task's notification that a peer message already delivered: Claude ends it with no
            // turn, and drover saw nothing begin
            "result"
                if !std::mem::take(&mut p.turn)
                    && l.num_turns == Some(0)
                    && l.origin.as_ref().and_then(|o| o.get("kind")?.as_str())
                        == Some("task-notification") =>
            {
                Vec::new()
            }
            "result" => {
                // modelUsage and total_cost_usd are the session's totals, subagents and earlier
                // processes included; usage is the turn's main loop only
                let total = match l.model_usage {
                    Some(models) => models.values().fold(Usage::default(), |mut t, m| {
                        t += &Usage {
                            input: m.input_tokens,
                            output: m.output_tokens,
                            cache_read: m.cache_read_input_tokens,
                            cache_write: m.cache_creation_input_tokens,
                            cost_usd: 0.0,
                        };
                        t
                    }),
                    None => {
                        let u = l.usage.unwrap_or_default();
                        let mut t = p.total.clone();
                        t += &Usage {
                            input: u.input_tokens,
                            output: u.output_tokens,
                            cache_read: u.cache_read_input_tokens,
                            cache_write: u.cache_creation_input_tokens,
                            cost_usd: 0.0,
                        };
                        t
                    }
                };
                let total = Usage {
                    cost_usd: l.total_cost_usd.unwrap_or(p.total.cost_usd),
                    ..total
                };
                let usage = since(&total, &p.total);
                p.total = total;
                vec![Out::Reply {
                    text: l.result.unwrap_or_default(),
                    usage: Some(usage),
                    error: l.is_error,
                }]
            }
            _ => Vec::new(),
        });
        out
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
                call: Value::Null,
            }),
            "Stop" => Some(Out::Reply {
                text: text("last_assistant_message").unwrap_or_default().into(),
                usage: None,
                error: false,
            }),
            // /compact is a turn with no prompt or Stop hook; it replies empty, as headless does.
            // An automatic one runs inside a turn, which goes on.
            "PreCompact" if text("trigger") == Some("manual") => Some(Out::Prompt),
            "PostCompact" if text("trigger") == Some("manual") => Some(Out::Reply {
                text: String::new(),
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

impl Claude {
    // The settings that make claude post its hooks to drover, given with --settings.
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
                "PreCompact",
                "PostCompact",
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
    #[serde(rename = "modelUsage")]
    model_usage: Option<std::collections::HashMap<String, ModelTokens>>,
    tasks: Option<Vec<serde::de::IgnoredAny>>,
    parent_tool_use_id: Option<String>,
    request_id: Option<String>,
    request: Option<Value>,
    num_turns: Option<u64>,
    origin: Option<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelTokens {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
}

// A turn's usage: what the session's totals grew by since the last.
fn since(total: &Usage, before: &Usage) -> Usage {
    Usage {
        input: total.input.saturating_sub(before.input),
        output: total.output.saturating_sub(before.output),
        cache_read: total.cache_read.saturating_sub(before.cache_read),
        cache_write: total.cache_write.saturating_sub(before.cache_write),
        cost_usd: (total.cost_usd - before.cost_usd).max(0.0),
    }
}

// AskUserQuestion's input with the owner's answers, one per question in order; any other input as is.
fn answered(input: &Value, answers: &[String]) -> Value {
    let questions = input.get("questions").and_then(Value::as_array);
    match (input, questions) {
        (Value::Object(i), Some(qs)) if !answers.is_empty() => {
            let map: serde_json::Map<String, Value> = qs
                .iter()
                .filter_map(|q| q.get("question")?.as_str())
                .zip(answers)
                .map(|(q, a)| (q.to_string(), a.as_str().into()))
                .collect();
            let mut i = i.clone();
            i.insert("answers".into(), map.into());
            Value::Object(i)
        }
        _ => input.clone(),
    }
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
            permission_mode: None,
            resume: None,
            cwd: std::path::Path::new("/w"),
            hooks: "http://127.0.0.1:7341/hooks/r-00000a",
            hook_timeout: 600,
            remote: false,
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
            // nobody answers a reader's stdin: a request there would hang the turn
            assert_eq!(flag(&argv, "--permission-prompt-tool"), None);
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
        assert_eq!(flag(&headless, "--permission-prompt-tool"), Some("stdio"));
        assert_eq!(flag(&headless, "--permission-mode"), Some("auto"));
        let asked = StartSpec {
            permission_mode: Some("default"),
            ..spec(false, Profile::Actor)
        };
        assert_eq!(
            flag(&Claude.start(&asked), "--permission-mode"),
            Some("default")
        );
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
                Out::Prompt,
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
        // a /compact in a pane is a turn of its own; an automatic one is inside a turn
        assert_eq!(
            hook("PreCompact", json!({"trigger": "manual"})),
            Some(Out::Prompt)
        );
        assert!(matches!(
            hook("PostCompact", json!({"trigger": "manual"})),
            Some(Out::Reply { .. })
        ));
        assert!(matches!(
            hook("PreCompact", json!({"trigger": "auto"})),
            Some(Out::Session(_))
        ));
        assert_eq!(
            hook(
                "PermissionRequest",
                json!({"tool_name": "Bash", "tool_input": {"command": "touch x"}})
            ),
            Some(Out::Permission {
                tool: "Bash".into(),
                input: json!({"command": "touch x"}),
                call: Value::Null,
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
            ..Decision::default()
        });
        assert_eq!(
            deny.pointer("/hookSpecificOutput/decision/message"),
            Some(&json!("not now"))
        );
    }

    // ExitPlanMode and AskUserQuestion run only on an allow that carries their input back, and
    // AskUserQuestion's with the answers keyed by question. Any other allow is a plain one.
    #[test]
    fn an_allow_carries_the_input_and_the_answers() {
        let allowed = |tool: &str, input: Value, answers: &[&str]| {
            Claude
                .respond(&Decision {
                    allow: true,
                    call: Some((tool.into(), input)),
                    answers: answers.iter().map(|a| a.to_string()).collect(),
                    ..Decision::default()
                })
                .pointer("/hookSpecificOutput/decision/updatedInput")
                .cloned()
        };
        let plan = json!({"plan": "touch x", "planFilePath": "/p.md"});
        assert_eq!(allowed("ExitPlanMode", plan.clone(), &[]), Some(plan));
        assert_eq!(allowed("Bash", json!({"command": "touch x"}), &[]), None);
        let q = |text: &str| json!({"question": text, "header": "h", "options": [{"label": "Red"}, {"label": "Blue"}], "multiSelect": false});
        let ask = json!({"questions": [q("Which color?"), q("Which size?")]});
        assert_eq!(
            allowed("AskUserQuestion", ask.clone(), &["Blue", "a big one"])
                .and_then(|i| i.get("answers").cloned()),
            Some(json!({"Which color?": "Blue", "Which size?": "a big one"}))
        );
        assert_eq!(Claude.questions("AskUserQuestion", &ask), Some(2));
        assert_eq!(Claude.questions("Bash", &json!({"command": "ls"})), None);
    }

    // After its turn, a background subagent's lines are no turn of the run's; the turn Claude starts
    // when it ends is, and the empty one for a notification already delivered is not.
    #[test]
    fn a_turn_claude_starts_itself() {
        let mut p = Parse::default();
        let mut read = |line: &str| Claude.events(line, &mut p);
        assert_eq!(
            read(r#"{"type":"assistant","message":{"content":[{"type":"text","text":"later"}]}}"#),
            [Out::Prompt]
        );
        assert!(matches!(
            read(r#"{"type":"result","result":"later"}"#).as_slice(),
            [Out::Reply { .. }]
        ));
        assert_eq!(
            read(
                r#"{"type":"assistant","parent_tool_use_id":"t1","message":{"content":[{"type":"tool_use","id":"t2","name":"Bash"}]}}"#
            ),
            [Out::ToolStart {
                tool: "Bash".into()
            }]
        );
        assert_eq!(
            read(
                r#"{"type":"stream_event","event":{"delta":{"type":"text_delta","text":"Done"}}}"#
            ),
            [Out::Prompt, Out::Text("Done".into())]
        );
        assert!(matches!(read(r#"{"type":"result","result":"Done","num_turns":1,"origin":{"kind":"task-notification"}}"#).as_slice(), [Out::Reply { .. }]));
        assert_eq!(
            read(
                r#"{"type":"result","result":"","num_turns":0,"origin":{"kind":"task-notification"}}"#
            ),
            []
        );
    }

    // Claude writes each permission onto stdout too, and cancels it once the hook answers. One
    // still open after the hook's timeout is denied there; a cancelled one is not answered.
    #[test]
    fn a_permission_no_hook_answered_is_denied_on_stdin() {
        let mut p = Parse::default();
        let req = |id: &str| {
            format!(
                r#"{{"type":"control_request","request_id":"{id}","request":{{"subtype":"can_use_tool","tool_name":"Bash","input":{{"command":"touch x"}}}}}}"#
            )
        };
        let call = |id: &str| json!({"request_id": id, "input": {"command": "touch x"}});
        assert_eq!(Claude.events(&req("a"), &mut p), [Out::Backstop(call("a"))]);
        Claude.events(&req("b"), &mut p);
        Claude.events(
            r#"{"type":"control_cancel_request","request_id":"b"}"#,
            &mut p,
        );
        let deny = Decision {
            allow: false,
            reason: Some("no answer".into()),
            ..Decision::default()
        };
        assert_eq!(Claude.answer(&deny, &call("b"), &mut p), []);
        let written: Vec<Value> = Claude
            .answer(&deny, &call("a"), &mut p)
            .into_iter()
            .filter_map(|o| match o {
                Out::Write(l) => serde_json::from_str(&l).ok(),
                _ => None,
            })
            .collect();
        assert_eq!(
            written,
            [
                json!({"type": "control_response", "response": {"subtype": "success", "request_id": "a", "response": {"behavior": "deny", "message": "no answer"}}})
            ]
        );
        // once only
        assert_eq!(Claude.answer(&deny, &call("a"), &mut p), []);
    }

    // A resumed process reports the session's totals from its start, subagents included: a turn is
    // what they grew by since the run's own usage.
    #[test]
    fn a_turn_is_what_the_session_totals_grew_by() {
        let base = Usage {
            input: 10,
            output: 100,
            cache_read: 1000,
            cache_write: 50,
            cost_usd: 0.5,
        };
        let mut p = Parse {
            total: base,
            ..Parse::default()
        };
        let line = r#"{"type":"result","result":"R","total_cost_usd":0.75,"usage":{"input_tokens":1,"output_tokens":2},"modelUsage":{"claude-haiku-5-5":{"inputTokens":11,"outputTokens":130,"cacheReadInputTokens":1200,"cacheCreationInputTokens":50,"costUSD":0.6},"claude-opus-5-5":{"inputTokens":1,"outputTokens":20,"costUSD":0.15}}}"#;
        assert_eq!(
            Claude.events(line, &mut p),
            [Out::Reply {
                text: "R".into(),
                usage: Some(Usage {
                    input: 2,
                    output: 50,
                    cache_read: 200,
                    cache_write: 0,
                    cost_usd: 0.25
                }),
                error: false
            }]
        );
        assert_eq!(
            Claude.events(
                r#"{"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"a"}]}"#,
                &mut p
            ),
            [Out::Background(1)]
        );
        assert_eq!(
            Claude.events(
                r#"{"type":"system","subtype":"background_tasks_changed","tasks":[]}"#,
                &mut p
            ),
            [Out::Background(0)]
        );
    }
}
