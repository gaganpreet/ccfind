use anyhow::Result;
use chrono::{Local, TimeZone};
use rusqlite::{params_from_iter, Connection};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeFlag {
    User,
    Assistant,
    Tools,
    All,
}

impl ScopeFlag {
    pub fn to_scopes(self) -> &'static [&'static str] {
        match self {
            ScopeFlag::User => &["user"],
            ScopeFlag::Assistant => &["user", "assistant"],
            ScopeFlag::Tools => &["user", "tool"],
            ScopeFlag::All => &["user", "assistant", "tool"],
        }
    }
}

#[derive(Debug, Default)]
pub struct SearchOpts {
    pub query: Option<String>,
    pub scope: Option<ScopeFlag>,
    pub project: Option<String>,
    pub branch: Option<String>,
    pub cwd_glob: Option<String>,
    pub since: Option<i64>,
    pub limit: usize,
}

#[derive(Debug)]
#[allow(dead_code)]
pub struct Hit {
    pub file_id: String,
    pub cwd: String,
    pub git_branch: String,
    pub project_dir: String,
    pub ts: i64,
    pub scope: String,
    pub snippet: String,
}

/// FTS5 MATCH query sanitization. Splits on whitespace, double-quotes each token
/// to neutralize FTS5 operators. With the `tokenchars '-_./'` tokenizer, dashes
/// and slashes survive inside tokens, so `pr-review-toolkit` works.
pub fn sanitize_query(q: &str) -> String {
    let mut out = String::new();
    for tok in q.split_whitespace() {
        // Strip any embedded double quotes to keep the wrap valid.
        let cleaned: String = tok.chars().filter(|&c| c != '"').collect();
        if cleaned.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push('"');
        out.push_str(&cleaned);
        out.push('"');
    }
    out
}

/// Build the SQL and parameters for a search. Returns hits ordered by rank+recency.
pub fn search(conn: &Connection, opts: &SearchOpts) -> Result<Vec<Hit>> {
    let scopes = opts.scope.unwrap_or(ScopeFlag::User).to_scopes();
    let scope_placeholders = (0..scopes.len())
        .map(|_| "?".to_string())
        .collect::<Vec<_>>()
        .join(",");

    let mut sql = String::new();
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    if let Some(q) = &opts.query {
        let sanitized = sanitize_query(q);
        if sanitized.is_empty() {
            return Ok(Vec::new());
        }
        sql.push_str(&format!(
            r#"SELECT s.file_id, s.cwd, s.git_branch, s.project_dir, m.ts, m.scope,
                      snippet(messages_fts, 0, '', '', '…', 12) AS snip
                 FROM messages_fts
                 JOIN messages m  ON m.msg_id  = messages_fts.rowid
                 JOIN sessions s  ON s.file_id = m.file_id
                WHERE messages_fts MATCH ?
                  AND m.scope IN ({})"#,
            scope_placeholders
        ));
        params.push(Box::new(sanitized));
        for sc in scopes {
            params.push(Box::new(sc.to_string()));
        }
    } else {
        // No-args mode: one row per session.
        sql.push_str(
            r#"SELECT s.file_id, s.cwd, s.git_branch, s.project_dir,
                      COALESCE(s.last_seen, 0) AS ts,
                      'session' AS scope,
                      COALESCE(s.ai_title, s.slug, s.last_prompt, '') AS snip
                 FROM sessions s
                WHERE 1=1"#,
        );
    }

    if let Some(p) = &opts.project {
        sql.push_str(" AND s.project_dir LIKE ?");
        params.push(Box::new(format!("%{}%", p)));
    }
    if let Some(b) = &opts.branch {
        sql.push_str(" AND s.git_branch = ?");
        params.push(Box::new(b.clone()));
    }
    if let Some(g) = &opts.cwd_glob {
        sql.push_str(" AND s.cwd GLOB ?");
        params.push(Box::new(g.clone()));
    }
    if let Some(since) = opts.since {
        if opts.query.is_some() {
            sql.push_str(" AND m.ts >= ?");
        } else {
            sql.push_str(" AND COALESCE(s.last_seen, 0) >= ?");
        }
        params.push(Box::new(since));
    }

    let limit = if opts.limit == 0 { 500 } else { opts.limit };
    if opts.query.is_some() {
        // Ranking: bm25 (lower = better) + age penalty (newer = lower).
        // Linear 0.02/day handles the long tail; step bonuses pull "still working
        // on this" sessions to the top regardless of bm25 differences.
        sql.push_str(&format!(
            r#" ORDER BY bm25(messages_fts)
                       + ((strftime('%s','now') - m.ts) / 86400.0) * 0.02
                       + CASE
                           WHEN (strftime('%s','now') - m.ts) <  3600  THEN -3.0
                           WHEN (strftime('%s','now') - m.ts) < 86400  THEN -1.5
                           WHEN (strftime('%s','now') - m.ts) < 604800 THEN -0.5
                           ELSE 0
                         END
                  ASC LIMIT {}"#,
            limit * 8
        ));
    } else {
        sql.push_str(&format!(
            " ORDER BY COALESCE(s.last_seen, 0) DESC LIMIT {}",
            limit
        ));
    }

    let mut stmt = conn.prepare(&sql)?;
    let rows: Vec<Hit> = stmt
        .query_map(params_from_iter(params.iter().map(|b| b.as_ref())), |r| {
            Ok(Hit {
                file_id: r.get(0)?,
                cwd: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                git_branch: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                project_dir: r.get(3)?,
                ts: r.get(4)?,
                scope: r.get(5)?,
                snippet: r.get::<_, Option<String>>(6)?.unwrap_or_default(),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    // Dedupe by file_id: keep the first occurrence (best-ranked) per file.
    // No-args mode is already one-per-session, so this is a no-op there.
    let mut seen = std::collections::HashSet::new();
    let mut deduped: Vec<Hit> = Vec::with_capacity(rows.len().min(limit));
    for h in rows {
        if seen.insert(h.file_id.clone()) {
            deduped.push(h);
            if deduped.len() >= limit {
                break;
            }
        }
    }
    Ok(deduped)
}

/// Format a hit as a tab-separated fzf line.
/// Columns: ts, short_cwd, branch, snippet, file_id
pub fn fzf_line(hit: &Hit) -> String {
    let ts_str = if hit.ts > 0 {
        Local
            .timestamp_opt(hit.ts, 0)
            .single()
            .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_default()
    } else {
        String::new()
    };
    let short_cwd = shorten_cwd(&hit.cwd, &hit.project_dir);
    let snippet = one_line(&hit.snippet);
    let branch = if hit.git_branch.is_empty() {
        "-".to_string()
    } else {
        hit.git_branch.clone()
    };
    format!("{}\t{}\t{}\t{}\t{}", ts_str, short_cwd, branch, snippet, hit.file_id)
}

fn shorten_cwd(cwd: &str, project_dir: &str) -> String {
    let pick = if !cwd.is_empty() { cwd } else { project_dir };
    if pick.is_empty() {
        return "?".to_string();
    }
    // Take last two path segments.
    let parts: Vec<&str> = pick.split('/').filter(|s| !s.is_empty()).collect();
    let n = parts.len();
    if n >= 2 {
        format!("{}/{}", parts[n - 2], parts[n - 1])
    } else if n == 1 {
        parts[0].to_string()
    } else {
        pick.to_string()
    }
}

fn one_line(s: &str) -> String {
    let single = s.replace(['\n', '\r', '\t'], " ");
    let collapsed = single
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if collapsed.chars().count() > 120 {
        let mut end = 120;
        while end > 0 && !collapsed.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &collapsed[..end])
    } else {
        collapsed
    }
}

/// Parse a `--since` value: "2026-01-15", "7d", "24h", "30m".
pub fn parse_since(s: &str) -> Option<i64> {
    let now = chrono::Utc::now().timestamp();
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        let dt = d.and_hms_opt(0, 0, 0)?.and_utc();
        return Some(dt.timestamp());
    }
    let (num, unit) = s.split_at(s.len().saturating_sub(1));
    let n: i64 = num.parse().ok()?;
    let secs = match unit {
        "d" => n * 86400,
        "h" => n * 3600,
        "m" => n * 60,
        _ => return None,
    };
    Some(now - secs)
}
