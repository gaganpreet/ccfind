use anyhow::{Context, Result};
use rayon::prelude::*;
use rusqlite::{params, Connection};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use walkdir::WalkDir;

use crate::paths;
use crate::session::{Action, Event};

pub const SCHEMA_VERSION: i32 = 1;

const REFRESH_DEBOUNCE_SECS: i64 = 2;

pub fn open() -> Result<Connection> {
    let path = paths::index_db_path()?;
    let conn = Connection::open(&path)
        .with_context(|| format!("opening sqlite db at {}", path.display()))?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    init_schema(&conn)?;
    Ok(conn)
}

fn init_schema(conn: &Connection) -> Result<()> {
    let existing: Option<String> = conn
        .query_row(
            "SELECT v FROM meta WHERE k='schema_version'",
            [],
            |r| r.get(0),
        )
        .ok();
    if existing.is_none() {
        conn.execute_batch(SCHEMA_SQL)?;
        conn.execute(
            "INSERT OR REPLACE INTO meta(k,v) VALUES('schema_version', ?1)",
            params![SCHEMA_VERSION.to_string()],
        )?;
    } else if let Some(v) = existing {
        let v: i32 = v.parse().unwrap_or(0);
        if v != SCHEMA_VERSION {
            anyhow::bail!(
                "index schema_version={} but binary expects {}. Delete {} and re-run.",
                v,
                SCHEMA_VERSION,
                paths::index_db_path()?.display()
            );
        }
    }
    Ok(())
}

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS sessions (
  file_id            TEXT PRIMARY KEY,
  jsonl_path         TEXT NOT NULL UNIQUE,
  project_dir        TEXT NOT NULL,
  encoded_dir        TEXT NOT NULL,
  cwd                TEXT,
  git_branch         TEXT,
  slug               TEXT,
  ai_title           TEXT,
  last_prompt        TEXT,
  first_seen         INTEGER,
  last_seen          INTEGER,
  jsonl_mtime        INTEGER NOT NULL,
  jsonl_size_indexed INTEGER NOT NULL DEFAULT 0,
  message_count      INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_sessions_last_seen ON sessions(last_seen DESC);
CREATE INDEX IF NOT EXISTS idx_sessions_project   ON sessions(project_dir);

CREATE TABLE IF NOT EXISTS messages (
  msg_id      INTEGER PRIMARY KEY,
  file_id     TEXT NOT NULL REFERENCES sessions(file_id) ON DELETE CASCADE,
  session_id  TEXT,
  ts          INTEGER NOT NULL,
  scope       TEXT NOT NULL,
  uuid        TEXT,
  jsonl_line  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_messages_file ON messages(file_id);
CREATE INDEX IF NOT EXISTS idx_messages_ts   ON messages(ts DESC);

CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
  body,
  tokenize="unicode61 remove_diacritics 2 tokenchars '-_./'"
);

CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v TEXT);

CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages BEGIN
  DELETE FROM messages_fts WHERE rowid = old.msg_id;
END;
"#;

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Read last_refresh from meta. Returns None if not set.
fn last_refresh(conn: &Connection) -> Option<i64> {
    conn.query_row(
        "SELECT v FROM meta WHERE k='last_refresh'",
        [],
        |r| r.get::<_, String>(0),
    )
    .ok()
    .and_then(|s| s.parse().ok())
}

fn set_last_refresh(conn: &Connection, ts: i64) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO meta(k,v) VALUES('last_refresh', ?1)",
        params![ts.to_string()],
    )?;
    Ok(())
}

/// Incremental refresh. Returns the number of files (re)parsed.
/// Honors a 2-second debounce: if we refreshed within the last 2s, skip.
pub fn refresh(conn: &mut Connection) -> Result<usize> {
    let now = unix_now();
    if let Some(last) = last_refresh(conn) {
        if now - last < REFRESH_DEBOUNCE_SECS {
            return Ok(0);
        }
    }
    let n = refresh_inner(conn, false)?;
    set_last_refresh(conn, unix_now())?;
    Ok(n)
}

/// Full reindex: drop tables and rebuild. Returns files parsed.
pub fn reindex_full(conn: &mut Connection) -> Result<usize> {
    eprintln!("Rebuilding index from scratch…");
    conn.execute_batch(
        r#"
        DROP TABLE IF EXISTS messages_fts;
        DROP TABLE IF EXISTS messages;
        DROP TABLE IF EXISTS sessions;
        "#,
    )?;
    conn.execute_batch(SCHEMA_SQL)?;
    conn.execute(
        "INSERT OR REPLACE INTO meta(k,v) VALUES('schema_version', ?1)",
        params![SCHEMA_VERSION.to_string()],
    )?;
    let n = refresh_inner(conn, true)?;
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").ok();
    set_last_refresh(conn, unix_now())?;
    Ok(n)
}

struct FileInfo {
    path: PathBuf,
    encoded_dir: String,
    file_id: String,
    size: u64,
    mtime: i64,
}

#[derive(Debug)]
enum ParseMode {
    Full,
    AppendFrom(u64),
    Skip,
}

struct ParsedFile {
    file_id: String,
    jsonl_path: String,
    encoded_dir: String,
    project_dir: String,
    size: u64,
    mtime: i64,
    mode: ParseModeTag,
    // Aggregated session-level metadata (from latest event in this batch).
    cwd: Option<String>,
    git_branch: Option<String>,
    slug: Option<String>,
    ai_title: Option<String>,
    last_prompt: Option<String>,
    first_seen: i64,
    last_seen: i64,
    // FTS rows to insert.
    rows: Vec<MessageRow>,
}

#[derive(Debug, Clone, Copy)]
enum ParseModeTag {
    Full,
    Append,
}

struct MessageRow {
    session_id: Option<String>,
    ts: i64,
    scope: &'static str,
    uuid: Option<String>,
    jsonl_line: i64,
    body: String,
}

fn refresh_inner(conn: &mut Connection, force_all: bool) -> Result<usize> {
    let root = paths::projects_dir()?;
    if !root.exists() {
        return Ok(0);
    }

    // Walk depth 2: ~/.claude/projects/<encoded-dir>/<uuid>.jsonl
    let mut files: Vec<FileInfo> = Vec::new();
    for entry in WalkDir::new(&root).max_depth(2).into_iter().filter_map(|e| e.ok()) {
        let p = entry.path();
        if !entry.file_type().is_file() {
            continue;
        }
        if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(encoded_dir) = p
            .parent()
            .and_then(|d| d.file_name())
            .and_then(|s| s.to_str())
            .map(String::from)
        else { continue };
        let Some(file_id) = paths::file_id_from_path(p) else { continue };
        let Ok(md) = entry.metadata() else { continue };
        let size = md.len();
        let mtime = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        files.push(FileInfo {
            path: p.to_path_buf(),
            encoded_dir,
            file_id,
            size,
            mtime,
        });
    }

    // Load existing state: file_id -> (size_indexed, mtime).
    let mut known: HashMap<String, (u64, i64)> = HashMap::new();
    {
        let mut stmt = conn.prepare("SELECT file_id, jsonl_size_indexed, jsonl_mtime FROM sessions")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))
        })?;
        for r in rows.flatten() {
            known.insert(r.0, (r.1 as u64, r.2));
        }
    }

    // Decide mode per file.
    let work: Vec<(FileInfo, ParseMode)> = files
        .into_iter()
        .map(|f| {
            let mode = if force_all {
                ParseMode::Full
            } else if let Some(&(size_indexed, mtime_indexed)) = known.get(&f.file_id) {
                if f.size < size_indexed || f.mtime < mtime_indexed {
                    ParseMode::Full
                } else if f.size > size_indexed {
                    ParseMode::AppendFrom(size_indexed)
                } else if f.mtime > mtime_indexed {
                    // Same size but newer mtime: edge case (touch/rewrite-same-size). Be safe.
                    ParseMode::Full
                } else {
                    ParseMode::Skip
                }
            } else {
                ParseMode::Full
            };
            (f, mode)
        })
        .collect();

    // Parse in parallel; write sequentially.
    let parsed: Vec<ParsedFile> = work
        .par_iter()
        .filter_map(|(fi, mode)| match mode {
            ParseMode::Skip => None,
            ParseMode::Full => Some(parse_file(fi, 0, ParseModeTag::Full)),
            ParseMode::AppendFrom(off) => Some(parse_file(fi, *off, ParseModeTag::Append)),
        })
        .filter_map(|r| r.ok())
        .collect();

    if parsed.is_empty() {
        return Ok(0);
    }

    let n = parsed.len();
    write_parsed(conn, parsed)?;
    Ok(n)
}

fn parse_file(fi: &FileInfo, start_offset: u64, mode: ParseModeTag) -> Result<ParsedFile> {
    let f = std::fs::File::open(&fi.path)?;
    let mut reader = BufReader::new(f);
    if start_offset > 0 {
        reader.seek(SeekFrom::Start(start_offset))?;
    }
    // Count which line we're on. For Append mode we don't know the starting line
    // number cheaply (would need a pre-scan); we use 0 as a sentinel — preview will
    // re-walk the file by uuid anyway.
    let mut line_no: i64 = if matches!(mode, ParseModeTag::Append) { -1 } else { 0 };
    let mut pf = ParsedFile {
        file_id: fi.file_id.clone(),
        jsonl_path: fi.path.to_string_lossy().into_owned(),
        encoded_dir: fi.encoded_dir.clone(),
        project_dir: paths::decode_project_dir(&fi.encoded_dir),
        size: fi.size,
        mtime: fi.mtime,
        mode,
        cwd: None,
        git_branch: None,
        slug: None,
        ai_title: None,
        last_prompt: None,
        first_seen: 0,
        last_seen: 0,
        rows: Vec::new(),
    };
    let mut buf = String::new();
    loop {
        buf.clear();
        let n = reader.read_line(&mut buf)?;
        if n == 0 {
            break;
        }
        if line_no >= 0 {
            line_no += 1;
        }
        let trimmed = buf.trim_end();
        if trimmed.is_empty() {
            continue;
        }
        let ev: Event = match serde_json::from_str(trimmed) {
            Ok(e) => e,
            Err(_) => continue, // skip malformed lines silently
        };

        // Capture session-level metadata (overwrite — last write wins).
        if let Some(c) = &ev.cwd {
            if !c.is_empty() {
                pf.cwd = Some(c.clone());
            }
        }
        if let Some(b) = &ev.git_branch {
            if !b.is_empty() {
                pf.git_branch = Some(b.clone());
            }
        }
        if let Some(s) = &ev.slug {
            if !s.is_empty() {
                pf.slug = Some(s.clone());
            }
        }
        let ts = ev
            .timestamp
            .as_deref()
            .map(crate::session::parse_ts)
            .unwrap_or(0);
        if ts > 0 {
            if pf.first_seen == 0 || ts < pf.first_seen {
                pf.first_seen = ts;
            }
            if ts > pf.last_seen {
                pf.last_seen = ts;
            }
        }

        let action = ev.classify();
        match action {
            Action::Skip => {}
            Action::SetTitle(t) => {
                pf.ai_title = Some(t);
            }
            Action::SetLastPrompt(p) => {
                pf.last_prompt = Some(p);
            }
            Action::Index { scope, body } => {
                pf.rows.push(MessageRow {
                    session_id: ev.session_id.clone(),
                    ts,
                    scope: scope.as_str(),
                    uuid: ev.uuid.clone(),
                    jsonl_line: if line_no >= 0 { line_no } else { 0 },
                    body,
                });
            }
            Action::IndexMany(rows) => {
                for (scope, body) in rows {
                    pf.rows.push(MessageRow {
                        session_id: ev.session_id.clone(),
                        ts,
                        scope: scope.as_str(),
                        uuid: ev.uuid.clone(),
                        jsonl_line: if line_no >= 0 { line_no } else { 0 },
                        body,
                    });
                }
            }
        }
    }
    Ok(pf)
}

fn write_parsed(conn: &mut Connection, parsed: Vec<ParsedFile>) -> Result<()> {
    let tx = conn.transaction()?;
    {
        let mut up_session = tx.prepare(
            r#"
            INSERT INTO sessions
              (file_id, jsonl_path, project_dir, encoded_dir, cwd, git_branch, slug,
               ai_title, last_prompt, first_seen, last_seen, jsonl_mtime,
               jsonl_size_indexed, message_count)
            VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)
            ON CONFLICT(file_id) DO UPDATE SET
              jsonl_path = excluded.jsonl_path,
              project_dir = excluded.project_dir,
              encoded_dir = excluded.encoded_dir,
              cwd = COALESCE(excluded.cwd, sessions.cwd),
              git_branch = COALESCE(excluded.git_branch, sessions.git_branch),
              slug = COALESCE(excluded.slug, sessions.slug),
              ai_title = COALESCE(excluded.ai_title, sessions.ai_title),
              last_prompt = COALESCE(excluded.last_prompt, sessions.last_prompt),
              first_seen = MIN(COALESCE(sessions.first_seen, excluded.first_seen),
                               COALESCE(excluded.first_seen, sessions.first_seen)),
              last_seen = MAX(COALESCE(sessions.last_seen, 0), COALESCE(excluded.last_seen, 0)),
              jsonl_mtime = excluded.jsonl_mtime,
              jsonl_size_indexed = excluded.jsonl_size_indexed,
              message_count = sessions.message_count + excluded.message_count
            "#,
        )?;
        let mut delete_session_msgs =
            tx.prepare("DELETE FROM messages WHERE file_id = ?1")?;
        let mut ins_msg = tx.prepare(
            r#"INSERT INTO messages
                 (file_id, session_id, ts, scope, uuid, jsonl_line)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6)"#,
        )?;
        let mut ins_fts = tx.prepare(
            "INSERT INTO messages_fts(rowid, body) VALUES (?1, ?2)",
        )?;

        for pf in parsed {
            // For Full mode: wipe any existing messages for this file first.
            if matches!(pf.mode, ParseModeTag::Full) {
                delete_session_msgs.execute(params![&pf.file_id])?;
            }
            let row_count = pf.rows.len() as i64;
            // For Full mode, we replace the message_count rather than incrementing.
            // The upsert above adds to existing — so for Full, zero out first.
            if matches!(pf.mode, ParseModeTag::Full) {
                tx.execute(
                    "UPDATE sessions SET message_count = 0 WHERE file_id = ?1",
                    params![&pf.file_id],
                )
                .ok();
            }
            up_session.execute(params![
                pf.file_id,
                pf.jsonl_path,
                pf.project_dir,
                pf.encoded_dir,
                pf.cwd,
                pf.git_branch,
                pf.slug,
                pf.ai_title,
                pf.last_prompt,
                if pf.first_seen == 0 { None } else { Some(pf.first_seen) },
                if pf.last_seen == 0 { None } else { Some(pf.last_seen) },
                pf.mtime,
                pf.size as i64,
                row_count,
            ])?;
            for row in pf.rows {
                ins_msg.execute(params![
                    pf.file_id,
                    row.session_id,
                    row.ts,
                    row.scope,
                    row.uuid,
                    row.jsonl_line,
                ])?;
                let rowid = tx.last_insert_rowid();
                ins_fts.execute(params![rowid, row.body])?;
            }
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn session_row(
    conn: &Connection,
    file_id: &str,
) -> Result<Option<SessionRow>> {
    let mut stmt = conn.prepare(
        "SELECT file_id, jsonl_path, project_dir, cwd, git_branch, slug, ai_title,
                last_prompt, first_seen, last_seen, message_count
           FROM sessions WHERE file_id = ?1",
    )?;
    let row = stmt
        .query_row(params![file_id], |r| {
            Ok(SessionRow {
                file_id: r.get(0)?,
                jsonl_path: r.get(1)?,
                project_dir: r.get(2)?,
                cwd: r.get(3)?,
                git_branch: r.get(4)?,
                slug: r.get(5)?,
                ai_title: r.get(6)?,
                last_prompt: r.get(7)?,
                first_seen: r.get(8)?,
                last_seen: r.get(9)?,
                message_count: r.get(10)?,
            })
        })
        .ok();
    Ok(row)
}

#[allow(dead_code)]
pub struct SessionRow {
    pub file_id: String,
    pub jsonl_path: String,
    pub project_dir: String,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    pub slug: Option<String>,
    pub ai_title: Option<String>,
    pub last_prompt: Option<String>,
    pub first_seen: Option<i64>,
    pub last_seen: Option<i64>,
    pub message_count: i64,
}

pub struct IndexStats {
    pub sessions: i64,
    pub projects: i64,
    pub messages: i64,
    pub db_bytes: u64,
    pub last_refresh: Option<i64>,
    pub schema_version: i32,
}

pub fn stats(conn: &Connection) -> Result<IndexStats> {
    let sessions: i64 = conn.query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))?;
    let projects: i64 = conn.query_row(
        "SELECT COUNT(DISTINCT project_dir) FROM sessions",
        [],
        |r| r.get(0),
    )?;
    let messages: i64 = conn.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))?;
    let db_bytes = std::fs::metadata(paths::index_db_path()?)
        .map(|m| m.len())
        .unwrap_or(0);
    let last_refresh = last_refresh(conn);
    let schema_version: i32 = conn
        .query_row("SELECT v FROM meta WHERE k='schema_version'", [], |r| {
            r.get::<_, String>(0)
        })
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    Ok(IndexStats {
        sessions,
        projects,
        messages,
        db_bytes,
        last_refresh,
        schema_version,
    })
}

