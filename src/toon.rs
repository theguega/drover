// JSON to TOON (toonformat.dev), what the terminal prints. Comma delimiter, two-space indent.
// Fields come out in the order they were built (serde_json's preserve_order).
// Strings with spaces in an array are listed one per line, which TOON also reads.

use serde_json::{Map, Value};

#[must_use]
pub fn encode(v: &Value) -> String {
    let mut out = String::new();
    match v {
        Value::Object(map) => object(&mut out, 0, map),
        Value::Array(items) => array(&mut out, 0, "", items),
        _ => line(&mut out, 0, &primitive(v)),
    }
    out
}

fn line(out: &mut String, depth: usize, text: &str) {
    out.push_str(&"  ".repeat(depth));
    out.push_str(text);
    out.push('\n');
}

fn field(out: &mut String, depth: usize, key: &str, v: &Value) {
    let key = quote_key(key);
    match v {
        Value::Object(map) => {
            line(out, depth, &format!("{key}:"));
            object(out, depth + 1, map);
        }
        Value::Array(items) => array(out, depth, &key, items),
        _ => line(out, depth, &format!("{key}: {}", primitive(v))),
    }
}

fn object(out: &mut String, depth: usize, map: &Map<String, Value>) {
    for (k, v) in map {
        field(out, depth, k, v);
    }
}

fn array(out: &mut String, depth: usize, key: &str, items: &[Value]) {
    let n = items.len();
    // a list of phrases, such as help commands, reads better one per line
    let phrases = items
        .iter()
        .any(|v| v.as_str().is_some_and(|s| s.contains(' ')));
    if items.iter().all(is_primitive) && !phrases {
        let values: Vec<String> = items.iter().map(primitive).collect();
        let sep = if n == 0 { "" } else { " " };
        line(out, depth, &format!("{key}[{n}]:{sep}{}", values.join(",")));
    } else if let Some(fields) = table(items) {
        let head: Vec<String> = fields.iter().map(|f| quote_key(f)).collect();
        line(out, depth, &format!("{key}[{n}]{{{}}}:", head.join(",")));
        for item in items {
            let row: Vec<String> = fields
                .iter()
                .map(|f| item.get(f).map_or_else(String::new, primitive))
                .collect();
            line(out, depth + 1, &row.join(","));
        }
    } else {
        line(out, depth, &format!("{key}[{n}]:"));
        for item in items {
            list_item(out, depth + 1, item);
        }
    }
}

fn list_item(out: &mut String, depth: usize, v: &Value) {
    let start = out.len();
    match v {
        Value::Object(map) if map.is_empty() => return line(out, depth, "-"),
        Value::Object(map) => object(out, depth + 1, map),
        Value::Array(items) => array(out, depth + 1, "", items),
        _ => return line(out, depth, &format!("- {}", primitive(v))),
    }
    // the first line moves onto the hyphen, and the rest line up under it
    let at = start + 2 * depth;
    out.replace_range(at..at + 2, "- ");
}

// The shared fields when every item is an object with the same keys and only primitive values.
fn table(items: &[Value]) -> Option<Vec<String>> {
    let first = items.first()?.as_object()?;
    if first.is_empty() {
        return None;
    }
    let fields: Vec<String> = first.keys().cloned().collect();
    items
        .iter()
        .all(|item| {
            item.as_object().is_some_and(|m| {
                m.len() == fields.len() && fields.iter().all(|f| m.get(f).is_some_and(is_primitive))
            })
        })
        .then_some(fields)
}

fn is_primitive(v: &Value) -> bool {
    !matches!(v, Value::Array(_) | Value::Object(_))
}

fn primitive(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => number(n),
        Value::String(s) => quote(s),
        Value::Array(_) | Value::Object(_) => String::new(),
    }
}

// Plain decimal: no exponent, and -0 is 0.
fn number(n: &serde_json::Number) -> String {
    match n.as_f64() {
        Some(f) if !n.is_i64() && !n.is_u64() => {
            if f == 0.0 {
                "0".into()
            } else {
                f.to_string()
            }
        }
        _ => n.to_string(),
    }
}

fn quote(s: &str) -> String {
    let needs = s.is_empty()
        || s != s.trim()
        || matches!(s, "true" | "false" | "null")
        || numeric(s)
        || s.starts_with('-')
        || s.chars()
            .any(|c| matches!(c, ':' | '"' | '\\' | '[' | ']' | '{' | '}' | ',') || c.is_control());
    if needs { quoted(s) } else { s.into() }
}

fn quoted(s: &str) -> String {
    let mut out = String::from('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// Would read back as a number: -?digits(.digits)?(e[+-]?digits)?, or a leading zero like 05.
fn numeric(s: &str) -> bool {
    let digits = |t: &str| !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit());
    let body = s.strip_prefix('-').unwrap_or(s);
    let (mantissa, exp) = match body.find(['e', 'E']) {
        Some(i) => (body.get(..i).unwrap_or_default(), body.get(i + 1..)),
        None => (body, None),
    };
    let (int, frac) = match mantissa.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (mantissa, None),
    };
    let exp_ok = exp.is_none_or(|e| digits(e.strip_prefix(['+', '-']).unwrap_or(e)));
    digits(int) && frac.is_none_or(digits) && exp_ok
}

fn quote_key(k: &str) -> String {
    let plain = k.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && k.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.');
    if plain { k.into() } else { quoted(k) }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn uniform_objects_become_a_table() {
        let v = json!({"users": [
            {"id": 1, "name": "Alice", "role": "admin"},
            {"id": 2, "name": "Bob", "role": "user"},
        ]});
        assert_eq!(
            encode(&v),
            "users[2]{id,name,role}:\n  1,Alice,admin\n  2,Bob,user\n"
        );
    }

    #[test]
    fn nested_objects_indent_and_primitive_arrays_inline() {
        let v = json!({"user": {"id": 123, "name": "Ada", "tags": ["reading", "gaming"], "active": true, "prefs": []}});
        assert_eq!(
            encode(&v),
            "user:\n  id: 123\n  name: Ada\n  tags[2]: reading,gaming\n  active: true\n  prefs[0]:\n"
        );
    }

    #[test]
    fn strings_are_quoted_only_when_they_would_misread() {
        let v = json!({
            "a": "a,b", "b": "05", "c": "true", "d": "", "e": "path: x", "f": "-x",
            "g": "say \"hi\"", "h": "a\nb", "i": " pad", "j": "1e5", "k": "~/x/y.z", "l": "hello world",
        });
        assert_eq!(
            encode(&v),
            "a: \"a,b\"\nb: \"05\"\nc: \"true\"\nd: \"\"\ne: \"path: x\"\nf: \"-x\"\n\
             g: \"say \\\"hi\\\"\"\nh: \"a\\nb\"\ni: \" pad\"\nj: \"1e5\"\nk: ~/x/y.z\nl: hello world\n"
        );
        assert_eq!(
            encode(&json!({"my key": 1, "x-y": 2})),
            "\"my key\": 1\n\"x-y\": 2\n"
        );
    }

    #[test]
    fn mixed_arrays_are_lists_with_the_first_field_on_the_hyphen() {
        let v = json!({"items": [1, {"a": 1}, "text", [1, 2], {}]});
        assert_eq!(
            encode(&v),
            "items[5]:\n  - 1\n  - a: 1\n  - text\n  - [2]: 1,2\n  -\n"
        );
        let v = json!({"items": [{"id": 1, "meta": {"x": 1}, "tags": ["a"]}, {"id": 2}]});
        assert_eq!(
            encode(&v),
            "items[2]:\n  - id: 1\n    meta:\n      x: 1\n    tags[1]: a\n  - id: 2\n"
        );
    }

    #[test]
    fn numbers_roots_and_phrases() {
        assert_eq!(
            encode(&json!({"n": -0.0, "f": 1.5, "big": 1e21})),
            "n: 0\nf: 1.5\nbig: 1000000000000000000000\n"
        );
        assert_eq!(encode(&json!([1, 2])), "[2]: 1,2\n");
        assert_eq!(encode(&json!("x")), "x\n");
        assert_eq!(
            encode(&json!({"a": null, "help": ["drover agents"]})),
            "a: null\nhelp[1]:\n  - drover agents\n"
        );
    }
}
