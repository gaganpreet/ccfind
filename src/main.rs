use anyhow::Result;
use clap::{Parser, Subcommand};

mod index;
mod paths;
mod picker;
mod preview;
mod resume;
mod search;
mod session;
mod stats;

use crate::search::{ScopeFlag, SearchOpts};

#[derive(Parser, Debug)]
#[command(name = "ccfind", version, about = "Global fzf-driven search across all Claude Code sessions")]
struct Cli {
    /// Search terms (optional). If omitted, opens fzf streaming all sessions.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    terms: Vec<String>,

    /// Search scope: user (default), assistant, tools, all
    #[arg(long)]
    scope: Option<String>,

    /// Include assistant text replies (shortcut for --scope=assistant)
    #[arg(long)]
    assistant: bool,

    /// Include tool calls (shortcut for --scope=tools)
    #[arg(long)]
    tools: bool,

    /// Filter by project path (substring match)
    #[arg(long)]
    project: Option<String>,

    /// Filter by git branch (exact match)
    #[arg(long)]
    branch: Option<String>,

    /// Filter by cwd (glob pattern)
    #[arg(long)]
    cwd: Option<String>,

    /// Only messages newer than: YYYY-MM-DD, 7d, 24h, 30m
    #[arg(long)]
    since: Option<String>,

    /// Max results (default 500)
    #[arg(long, default_value_t = 500)]
    limit: usize,

    /// Print to stdout instead of opening fzf
    #[arg(long)]
    no_fzf: bool,

    #[command(subcommand)]
    sub: Option<Sub>,
}

#[derive(Subcommand, Debug)]
enum Sub {
    /// Rebuild or incrementally refresh the index
    Reindex {
        /// Drop everything and rebuild from scratch
        #[arg(long)]
        full: bool,
    },
    /// Render a session for the fzf preview pane
    Preview {
        file_id: String,
        #[arg(long)]
        r#match: Option<String>,
        #[arg(long)]
        lines: Option<usize>,
    },
    /// chdir into the session's cwd and exec `claude --resume <file_id>`
    Resume { file_id: String },
    /// Open the JSONL in $EDITOR
    Edit { file_id: String },
    /// Print index statistics
    Stats,
}

fn main() {
    if let Err(e) = real_main() {
        eprintln!("ccfind: {:#}", e);
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    let cli = Cli::parse();

    // First-run auto-reindex: if the DB doesn't exist yet, force a full reindex on
    // the first user-facing command (anything other than `reindex`).
    let needs_first_run = !paths::index_db_path()?.exists();

    let mut conn = index::open()?;

    match cli.sub {
        Some(Sub::Reindex { full }) => {
            let n = if full {
                index::reindex_full(&mut conn)?
            } else {
                index::refresh(&mut conn)?
            };
            eprintln!("indexed {} file(s)", n);
            return Ok(());
        }
        Some(Sub::Preview { file_id, r#match, lines: _ }) => {
            // Don't refresh for preview — it's called from fzf and needs to be fast.
            return preview::run(&conn, &file_id, r#match.as_deref());
        }
        Some(Sub::Resume { file_id }) => {
            return resume::run(&conn, &file_id);
        }
        Some(Sub::Edit { file_id }) => {
            return resume::edit(&conn, &file_id);
        }
        Some(Sub::Stats) => {
            // Run a refresh first so stats reflect current state.
            let _ = index::refresh(&mut conn);
            return stats::run(&conn);
        }
        None => {}
    }

    // Default path: interactive search.
    if needs_first_run {
        eprintln!("Building index for first use (this can take 30–90s)…");
        index::reindex_full(&mut conn)?;
    } else {
        let _ = index::refresh(&mut conn);
    }

    let scope = pick_scope(&cli);
    let query = if cli.terms.is_empty() {
        None
    } else {
        Some(cli.terms.join(" "))
    };
    let since = cli.since.as_deref().and_then(search::parse_since);
    let opts = SearchOpts {
        query: query.clone(),
        scope: Some(scope),
        project: cli.project,
        branch: cli.branch,
        cwd_glob: cli.cwd,
        since,
        limit: cli.limit,
    };

    let hits = search::search(&conn, &opts)?;
    if hits.is_empty() {
        eprintln!("no results");
        return Ok(());
    }
    if cli.no_fzf {
        picker::print_lines(&hits);
    } else {
        picker::run_fzf(&hits, query.as_deref())?;
    }
    Ok(())
}

fn pick_scope(cli: &Cli) -> ScopeFlag {
    if let Some(s) = &cli.scope {
        return match s.as_str() {
            "user" => ScopeFlag::User,
            "assistant" => ScopeFlag::Assistant,
            "tools" | "tool" => ScopeFlag::Tools,
            "all" => ScopeFlag::All,
            other => {
                eprintln!("ccfind: unknown scope '{}', defaulting to user", other);
                ScopeFlag::User
            }
        };
    }
    match (cli.assistant, cli.tools) {
        (true, true) => ScopeFlag::All,
        (true, false) => ScopeFlag::Assistant,
        (false, true) => ScopeFlag::Tools,
        (false, false) => ScopeFlag::User,
    }
}
