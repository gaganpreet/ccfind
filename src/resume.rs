use anyhow::{anyhow, Result};
use std::os::unix::process::CommandExt;
use std::process::Command;

use crate::index;

pub fn run(conn: &rusqlite::Connection, file_id: &str) -> Result<()> {
    let row = index::session_row(conn, file_id)?
        .ok_or_else(|| anyhow!("session not found: {} (try `cc-search reindex`)", file_id))?;

    let candidates: Vec<String> = std::iter::once(row.cwd.clone())
        .chain(std::iter::once(Some(row.project_dir.clone())))
        .chain(std::iter::once(std::env::var("HOME").ok()))
        .filter_map(|x| x)
        .collect();

    let mut chosen: Option<String> = None;
    for c in &candidates {
        if std::path::Path::new(c).is_dir() {
            chosen = Some(c.clone());
            break;
        } else {
            eprintln!("cc-search: cwd unavailable: {}", c);
        }
    }
    let target = chosen.ok_or_else(|| anyhow!("no usable cwd found"))?;
    std::env::set_current_dir(&target)?;

    let err = Command::new("claude").args(["--resume", file_id]).exec();
    Err(anyhow!("failed to exec claude: {}", err))
}

pub fn edit(conn: &rusqlite::Connection, file_id: &str) -> Result<()> {
    let row = index::session_row(conn, file_id)?
        .ok_or_else(|| anyhow!("session not found: {}", file_id))?;
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
    let status = Command::new(&editor).arg(&row.jsonl_path).status()?;
    if !status.success() {
        anyhow::bail!("{} exited non-zero", editor);
    }
    Ok(())
}
