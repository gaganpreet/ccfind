use anyhow::{anyhow, Result};
use std::path::PathBuf;

pub fn projects_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no $HOME"))?;
    Ok(home.join(".claude").join("projects"))
}

pub fn cache_dir() -> Result<PathBuf> {
    let cache = dirs::cache_dir().ok_or_else(|| anyhow!("no cache dir"))?;
    let dir = cache.join("cc-search");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub fn index_db_path() -> Result<PathBuf> {
    Ok(cache_dir()?.join("index.db"))
}

/// Decode `-home-user-projects-foo` -> `/home/user/projects/foo`.
/// Lossy for real dashes in path components; treat as a hint only — the canonical
/// cwd comes from the JSONL events themselves.
pub fn decode_project_dir(encoded: &str) -> String {
    let trimmed = encoded.strip_prefix('-').unwrap_or(encoded);
    let mut out = String::with_capacity(trimmed.len() + 1);
    out.push('/');
    for ch in trimmed.chars() {
        if ch == '-' {
            out.push('/');
        } else {
            out.push(ch);
        }
    }
    out
}

/// Extract the uuid (basename without `.jsonl`) from a path.
pub fn file_id_from_path(p: &std::path::Path) -> Option<String> {
    p.file_stem()?.to_str().map(|s| s.to_string())
}
