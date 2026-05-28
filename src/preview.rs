use anyhow::{anyhow, Result};
use chrono::{Local, TimeZone};
use nu_ansi_term::Color;
use std::io::{BufRead, BufReader};

use crate::index::{self, SessionRow};
use crate::session::{Action, Event};

fn print_header(row: &SessionRow) {
    let cwd = row.cwd.clone().unwrap_or_else(|| row.project_dir.clone());
    let branch = row.git_branch.clone().unwrap_or_default();
    let title = row.ai_title.clone().unwrap_or_default();
    let slug = row.slug.clone().unwrap_or_default();
    let label = |s: &str| Color::White.bold().paint(format!("{:<9}", s)).to_string();
    let dash = |s: &str| if s.is_empty() { "-".to_string() } else { s.to_string() };

    println!("{} {}", label("session:"), row.file_id);
    println!("{} {}", label("title:"), dash(&title));
    if !slug.is_empty() {
        println!("{} {}", label("slug:"), slug);
    }
    println!("{} {}", label("cwd:"), cwd);
    if cwd != row.project_dir {
        println!("{} {}", label("project:"), row.project_dir);
    }
    println!("{} {}", label("branch:"), dash(&branch));

    if let (Some(first), Some(last)) = (row.first_seen, row.last_seen) {
        let f = fmt_ts(first);
        let l = fmt_ts(last);
        let dur = humanize_duration(last - first);
        println!("{} {}  →  {}  ({})", label("range:"), f, l, dur);
    } else if let Some(last) = row.last_seen {
        println!("{} {}", label("last:"), fmt_ts(last));
    }

    println!("{} {}", label("messages:"), row.message_count);

    // File path + size.
    let size = std::fs::metadata(&row.jsonl_path).map(|m| m.len()).ok();
    let size_str = size.map(humanize_bytes).unwrap_or_else(|| "?".to_string());
    println!("{} {} ({})", label("file:"), row.jsonl_path, size_str);

    if let Some(lp) = &row.last_prompt {
        let one = lp.replace(['\n', '\r'], " ");
        let trimmed = one.split_whitespace().collect::<Vec<_>>().join(" ");
        let cut = if trimmed.chars().count() > 80 {
            let mut end = 80;
            while end > 0 && !trimmed.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}…", &trimmed[..end])
        } else {
            trimmed
        };
        if !cut.is_empty() {
            println!("{} {}", label("last cmd:"), cut);
        }
    }

    println!();
}

fn fmt_ts(ts: i64) -> String {
    Local
        .timestamp_opt(ts, 0)
        .single()
        .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| ts.to_string())
}

fn humanize_duration(secs: i64) -> String {
    if secs < 0 {
        return "?".to_string();
    }
    let m = secs / 60;
    let h = m / 60;
    let d = h / 24;
    if d > 0 {
        format!("{}d {}h", d, h % 24)
    } else if h > 0 {
        format!("{}h {}m", h, m % 60)
    } else if m > 0 {
        format!("{}m", m)
    } else {
        format!("{}s", secs)
    }
}

fn humanize_bytes(b: u64) -> String {
    const K: u64 = 1024;
    const M: u64 = K * 1024;
    const G: u64 = M * 1024;
    if b >= G {
        format!("{:.2} GB", b as f64 / G as f64)
    } else if b >= M {
        format!("{:.1} MB", b as f64 / M as f64)
    } else if b >= K {
        format!("{:.0} KB", b as f64 / K as f64)
    } else {
        format!("{} B", b)
    }
}

const MAX_CHUNK_EVENTS: usize = 50;
const CONTEXT_BEFORE: usize = 2;
const CONTEXT_AFTER: usize = 3;

pub fn run(conn: &rusqlite::Connection, file_id: &str, match_query: Option<&str>) -> Result<()> {
    let row = index::session_row(conn, file_id)?
        .ok_or_else(|| anyhow!("session not found: {} (try `cc-search reindex`)", file_id))?;

    print_header(&row);

    let f = std::fs::File::open(&row.jsonl_path)
        .map_err(|e| anyhow!("opening {}: {}", row.jsonl_path, e))?;
    let reader = BufReader::new(f);

    // Pull out renderable events (skip-noise filter same as indexer for consistency).
    struct Item {
        ts: i64,
        role: &'static str,
        body: String,
    }
    let mut items: Vec<Item> = Vec::new();
    for line in reader.lines() {
        let Ok(line) = line else { continue };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let ev: Event = match serde_json::from_str(trimmed) {
            Ok(e) => e,
            Err(_) => continue,
        };
        let ts = ev
            .timestamp
            .as_deref()
            .map(crate::session::parse_ts)
            .unwrap_or(0);
        match ev.classify() {
            Action::Index { scope, body } => items.push(Item {
                ts,
                role: scope.as_str(),
                body,
            }),
            Action::IndexMany(rows) => {
                for (scope, body) in rows {
                    items.push(Item {
                        ts,
                        role: scope.as_str(),
                        body,
                    });
                }
            }
            _ => {}
        }
    }

    // Decide which indices to render.
    let tokens: Vec<String> = match match_query {
        Some(q) => q.split_whitespace().map(|s| s.to_lowercase()).collect(),
        None => Vec::new(),
    };

    let render_indices: Vec<usize> = if tokens.is_empty() {
        // First 20 events.
        (0..items.len().min(20)).collect()
    } else {
        let mut chosen: Vec<usize> = Vec::new();
        for (i, it) in items.iter().enumerate() {
            let lc = it.body.to_lowercase();
            if tokens.iter().any(|t| lc.contains(t)) {
                let lo = i.saturating_sub(CONTEXT_BEFORE);
                let hi = (i + CONTEXT_AFTER + 1).min(items.len());
                for k in lo..hi {
                    if !chosen.contains(&k) {
                        chosen.push(k);
                    }
                }
                if chosen.len() >= MAX_CHUNK_EVENTS {
                    break;
                }
            }
        }
        chosen.sort();
        chosen.dedup();
        if chosen.is_empty() {
            // Match query but no hits in this file (rare but possible if FTS5 matched a
            // tokenization-sensitive variant). Fall back to first 20.
            (0..items.len().min(20)).collect()
        } else {
            chosen
        }
    };

    let mut last_idx: Option<usize> = None;
    for &i in &render_indices {
        if let Some(prev) = last_idx {
            if i > prev + 1 {
                println!("{}", Color::DarkGray.paint("───"));
            }
        }
        last_idx = Some(i);
        let it = &items[i];
        let ts_str = if it.ts > 0 {
            Local
                .timestamp_opt(it.ts, 0)
                .single()
                .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_default()
        } else {
            "?".to_string()
        };
        let role_colored = match it.role {
            "user" => Color::Cyan.paint(format!("user      ")),
            "assistant" => Color::Green.paint(format!("assistant ")),
            "tool" => Color::Magenta.paint(format!("tool      ")),
            other => Color::White.paint(format!("{:<10}", other)),
        };
        println!(
            "{} {} {}",
            Color::DarkGray.paint(format!("[{}]", ts_str)),
            role_colored,
            ""
        );
        // Body: word-wrap-light by line; highlight tokens.
        for body_line in it.body.lines() {
            if tokens.is_empty() {
                println!("  {}", body_line);
            } else {
                println!("  {}", highlight(body_line, &tokens));
            }
        }
        println!();
    }
    Ok(())
}

fn highlight(line: &str, tokens: &[String]) -> String {
    if tokens.is_empty() {
        return line.to_string();
    }
    let lc = line.to_lowercase();
    // Find all match ranges across tokens.
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for tok in tokens {
        if tok.is_empty() {
            continue;
        }
        let mut start = 0;
        while let Some(pos) = lc[start..].find(tok) {
            let abs = start + pos;
            ranges.push((abs, abs + tok.len()));
            start = abs + tok.len();
        }
    }
    if ranges.is_empty() {
        return line.to_string();
    }
    ranges.sort();
    // Merge overlapping.
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for r in ranges {
        if let Some(last) = merged.last_mut() {
            if r.0 <= last.1 {
                last.1 = last.1.max(r.1);
                continue;
            }
        }
        merged.push(r);
    }
    let mut out = String::new();
    let mut cursor = 0;
    for (a, b) in merged {
        // Snap to char boundaries.
        let mut aa = a;
        while aa > 0 && !line.is_char_boundary(aa) {
            aa -= 1;
        }
        let mut bb = b;
        while bb < line.len() && !line.is_char_boundary(bb) {
            bb += 1;
        }
        if aa < cursor {
            continue;
        }
        out.push_str(&line[cursor..aa]);
        out.push_str(&Color::Yellow.bold().paint(&line[aa..bb]).to_string());
        cursor = bb;
    }
    out.push_str(&line[cursor..]);
    out
}
