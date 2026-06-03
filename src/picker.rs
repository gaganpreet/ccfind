use anyhow::{anyhow, Result};
use std::io::Write;
use std::process::{Command, Stdio};

use crate::search::Hit;

/// Run fzf with the given hits as input. Returns the exit status.
/// `query` is the literal user query string (used for preview --match).
pub fn run_fzf(hits: &[Hit], query: Option<&str>) -> Result<()> {
    let exe = std::env::current_exe()?;
    let exe = exe
        .to_str()
        .ok_or_else(|| anyhow!("non-utf8 exe path"))?;

    let preview_cmd = match query {
        Some(q) if !q.is_empty() => format!(
            "{} preview {{5}} --match {}",
            shlex::try_quote(exe).unwrap(),
            shlex::try_quote(q).unwrap(),
        ),
        _ => format!("{} preview {{5}}", shlex::try_quote(exe).unwrap()),
    };
    let resume_cmd = format!("{} resume {{5}}", shlex::try_quote(exe).unwrap());
    let edit_cmd = format!("{} edit {{5}}", shlex::try_quote(exe).unwrap());

    let mut child = Command::new("fzf")
        .args([
            "--ansi",
            "--delimiter", "\t",
            "--with-nth", "1..4",
            "--nth", "1..4",
            "--preview", &preview_cmd,
            "--preview-window", "right:60%:wrap",
            "--height", "90%",
            "--reverse",
            "--bind", &format!("enter:become({})", resume_cmd),
            "--bind", "ctrl-o:execute-silent(echo {5})+abort",
            "--bind", &format!("ctrl-e:execute({})", edit_cmd),
            "--header",
            "enter=resume  ctrl-o=print id  ctrl-e=edit jsonl",
        ])
        .stdin(Stdio::piped())
        .spawn()?;

    {
        let stdin = child.stdin.as_mut().ok_or_else(|| anyhow!("fzf stdin"))?;
        for h in hits {
            writeln!(stdin, "{}", crate::search::fzf_line(h))?;
        }
    }
    let status = child.wait()?;
    // fzf exits 130 on Ctrl-C / no selection — not an error for us.
    if let Some(code) = status.code() {
        if code != 0 && code != 130 {
            anyhow::bail!("fzf exited with code {}", code);
        }
    }
    Ok(())
}

/// Print hits as tab-separated lines to stdout (no fzf).
pub fn print_lines(hits: &[Hit]) {
    let mut out = std::io::stdout().lock();
    for h in hits {
        let _ = writeln!(out, "{}", crate::search::fzf_line(h));
    }
}

/// Interactive content-search mode. Streams `initial_hits` (recent sessions) to
/// fzf as the starting view, then on every keystroke calls back into
/// `ccfind --no-fzf -- <query>` to do a real FTS5 search and reload the list.
///
/// Uses `--disabled` so fzf itself doesn't filter on top of FTS5 results — the
/// search engine is the ranker, fzf is only the picker.
pub fn run_fzf_interactive(initial_hits: &[Hit]) -> Result<()> {
    let exe = std::env::current_exe()?;
    let exe = exe.to_str().ok_or_else(|| anyhow!("non-utf8 exe path"))?;
    let exe_q = shlex::try_quote(exe).unwrap();

    // {q} is shell-escaped by fzf. `--` ensures clap treats it as terms,
    // not as flags, even if the user types something like `-foo`.
    let reload_cmd = format!("{} --no-fzf -- {{q}}", exe_q);
    let preview_cmd = format!("{} preview {{5}} --match {{q}}", exe_q);
    let resume_cmd = format!("{} resume {{5}}", exe_q);
    let edit_cmd = format!("{} edit {{5}}", exe_q);

    let mut child = Command::new("fzf")
        .args([
            "--ansi",
            "--disabled",
            "--delimiter", "\t",
            "--with-nth", "1..4",
            "--query", "",
            "--bind", &format!("change:reload:{}", reload_cmd),
            "--preview", &preview_cmd,
            "--preview-window", "right:60%:wrap",
            "--height", "90%",
            "--reverse",
            "--bind", &format!("enter:become({})", resume_cmd),
            "--bind", "ctrl-o:execute-silent(echo {5})+abort",
            "--bind", &format!("ctrl-e:execute({})", edit_cmd),
            "--header",
            "type to search content & metadata  |  enter=resume  ctrl-o=print id  ctrl-e=edit jsonl",
        ])
        .stdin(Stdio::piped())
        .spawn()?;

    {
        let stdin = child.stdin.as_mut().ok_or_else(|| anyhow!("fzf stdin"))?;
        for h in initial_hits {
            writeln!(stdin, "{}", crate::search::fzf_line(h))?;
        }
    }
    let status = child.wait()?;
    if let Some(code) = status.code() {
        if code != 0 && code != 130 {
            anyhow::bail!("fzf exited with code {}", code);
        }
    }
    Ok(())
}
