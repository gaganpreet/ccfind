use anyhow::Result;
use chrono::{Local, TimeZone};

use crate::index;

pub fn run(conn: &rusqlite::Connection) -> Result<()> {
    let s = index::stats(conn)?;
    let last = s
        .last_refresh
        .and_then(|ts| Local.timestamp_opt(ts, 0).single())
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| "never".to_string());

    let mb = s.db_bytes as f64 / (1024.0 * 1024.0);
    println!("schema_version : {}", s.schema_version);
    println!("projects       : {}", s.projects);
    println!("sessions       : {}", s.sessions);
    println!("messages       : {}", s.messages);
    println!("index size     : {:.1} MB", mb);
    println!("last refresh   : {}", last);
    Ok(())
}
