//! Agent markdown → Discord: no tables, rules or `####`, and a fence cut across messages is reopened.

const NARROW: usize = 60;

#[must_use]
pub fn discord(md: &str) -> String {
    let mut out = Vec::new();
    let mut table = Vec::new();
    let mut fenced = false;
    for line in md.lines() {
        let t = line.trim();
        if t.starts_with("```") {
            fenced = !fenced;
        }
        if !fenced && t.starts_with('|') {
            table.push(t);
            continue;
        }
        if !table.is_empty() {
            out.push(render_table(&table));
            table.clear();
        }
        if fenced || t.starts_with("```") {
            out.push(line.to_string());
        } else if is_rule(t) {
            out.push(String::new());
        } else if let Some(h) = t.strip_prefix("####") {
            out.push(format!("**{}**", h.trim_start_matches('#').trim()));
        } else {
            out.push(line.to_string());
        }
    }
    if !table.is_empty() {
        out.push(render_table(&table));
    }
    out.join("\n")
}

fn is_rule(t: &str) -> bool {
    let s: Vec<char> = t.chars().filter(|c| *c != ' ').collect();
    s.len() >= 3 && ['-', '*', '_'].iter().any(|c| s.iter().all(|x| x == c))
}

fn cells(row: &str) -> Vec<String> {
    let row = row.trim().trim_start_matches('|').trim_end_matches('|');
    row.split('|').map(|c| c.trim().replace("**", "").replace('`', "")).collect()
}

fn is_separator(row: &[String]) -> bool {
    row.iter().all(|c| !c.is_empty() && c.chars().all(|x| matches!(x, '-' | ':' | ' ')))
}

fn render_table(lines: &[&str]) -> String {
    let rows: Vec<Vec<String>> = lines.iter().map(|l| cells(l)).filter(|r| !is_separator(r)).collect();
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> = (0..cols)
        .map(|i| rows.iter().filter_map(|r| r.get(i)).map(|c| c.chars().count()).max().unwrap_or(0))
        .collect();
    if widths.iter().sum::<usize>() + 3 * cols.saturating_sub(1) <= NARROW {
        let line = |r: &Vec<String>| {
            let padded = (0..cols).map(|i| {
                let c = r.get(i).map_or("", String::as_str);
                format!("{c:<w$}", w = widths.get(i).copied().unwrap_or(0))
            });
            padded.collect::<Vec<_>>().join(" │ ").trim_end().to_string()
        };
        let mut out = vec!["```".to_string()];
        let mut it = rows.iter();
        if let Some(h) = it.next() {
            out.push(line(h));
            out.push(widths.iter().map(|w| "─".repeat(*w)).collect::<Vec<_>>().join("─┼─"));
        }
        out.extend(it.map(line));
        out.push("```".into());
        return out.join("\n");
    }
    let Some((header, body)) = rows.split_first() else { return String::new() };
    body.iter()
        .map(|r| {
            let mut it = r.iter().zip(header).filter(|(c, _)| !c.is_empty());
            let first = it.next().map_or(String::new(), |(c, _)| format!("**{c}**"));
            let rest: Vec<String> = it.map(|(c, h)| format!("{h}: {c}")).collect();
            if rest.is_empty() { format!("- {first}") } else { format!("- {first} — {}", rest.join(" · ")) }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[must_use]
pub fn chunks(text: &str, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut fence: Option<String> = None;
    for line in text.lines().flat_map(|l| split_long(l, max / 2)) {
        let reserve = if fence.is_some() { 4 } else { 0 };
        if !cur.is_empty() && cur.chars().count() + line.chars().count() + 1 + reserve > max {
            if fence.is_some() {
                cur.push_str("\n```");
            }
            out.push(std::mem::take(&mut cur));
            if let Some(f) = &fence {
                cur.push_str(f);
            }
        }
        if !cur.is_empty() {
            cur.push('\n');
        }
        cur.push_str(&line);
        if line.trim_start().starts_with("```") {
            fence = if fence.is_some() { None } else { Some(line.trim().to_string()) };
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

fn split_long(line: &str, n: usize) -> Vec<String> {
    let chars: Vec<char> = line.chars().collect();
    if chars.len() <= n {
        return vec![line.to_string()];
    }
    chars.chunks(n).map(|c| c.iter().collect()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn narrow_table_becomes_aligned_block() {
        let md = "x\n| a | bb |\n|---|:-:|\n| `1` | **2** |\ny";
        assert_eq!(discord(md), "x\n```\na │ bb\n──┼───\n1 │ 2\n```\ny");
    }

    #[test]
    fn wide_table_becomes_list() {
        let long = "z".repeat(70);
        let md = format!("| name | note |\n|---|---|\n| a | {long} |");
        assert_eq!(discord(&md), format!("- **a** — note: {long}"));
    }

    #[test]
    fn table_inside_fence_is_kept() {
        let md = "```\n| a |\n```";
        assert_eq!(discord(md), md);
    }

    #[test]
    fn cut_fence_is_reopened() {
        let text = format!("```rust\n{}\n```", ["line"; 10].join("\n"));
        let parts = chunks(&text, 30);
        assert!(parts.len() > 1);
        for p in &parts {
            assert!(p.chars().count() <= 30, "{p:?}");
            assert_eq!(p.matches("```").count() % 2, 0, "{p:?}");
        }
        assert!(parts.get(1).is_some_and(|p| p.starts_with("```rust\n")));
    }
}
