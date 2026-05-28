# ccfind

Global fzf-driven search across all your Claude Code sessions.

Find which clone, which subfolder, which branch you were in when you asked Claude about something — across every session on disk, in under 200ms.

## Why

Claude Code stores every session as JSONL under `~/.claude/projects/<encoded-path>/<session-uuid>.jsonl`. With multiple clones of the same repo and frequent work in subfolders, the built-in `claude --resume` picker doesn't cut it. `ccfind` indexes everything into SQLite + FTS5 and lets you fuzzy-search the lot through fzf.

## Install

Requires `cargo` and `fzf` on PATH.

```sh
cd ccfind
cargo install --path .
```

That drops `ccfind` into `~/.cargo/bin/`.

## Usage

```sh
ccfind                       # interactive: stream all sessions to fzf
ccfind migration             # search user messages for "migration"
ccfind --tools polars        # also search tool calls (Bash, Edit, ...)
ccfind --assistant "explain" # also search assistant text replies
ccfind --project /home/me/repo-a "redis"
ccfind --branch main --since 7d "TODO"
ccfind --cwd '*/backend' "schema"
ccfind --no-fzf foo | head   # pipe-friendly, scriptable
```

### Keybindings in the fzf picker

- `Enter` — resume the session: `cd <original cwd>` then `claude --resume <id>`.
- `Ctrl-O` — print the file-id (jsonl basename) and exit.
- `Ctrl-E` — open the raw JSONL in `$EDITOR`.

### Subcommands

```
ccfind reindex [--full]      # incremental; --full rebuilds from scratch
ccfind preview <file-id>     # render a session (used by fzf preview)
ccfind resume <file-id>      # chdir + exec claude --resume
ccfind edit <file-id>        # open JSONL in $EDITOR
ccfind stats                 # sessions / messages / db size
```

## How it works

- Indexes `~/.claude/projects/*/*.jsonl` into `~/.cache/ccfind/index.db`.
- Default search scope is your user messages + session metadata (cwd, branch, title). Use `--tools` / `--assistant` / `--scope all` to widen.
- Skips `tool_result` echoes and `isMeta:true` slash-command bodies (noise that would drown ranking).
- Tokenizer keeps `-`, `_`, `.`, `/` inside tokens, so `pr-review-toolkit`, `tool_use`, `src/main.rs` match as single tokens.
- Incremental refresh on every invocation: stats files, parses only what's changed, append-only path for active sessions. 2-second debounce so rapid-fire queries don't re-scan.

### Known behavior

After you resume a session via `Enter`, your interactive shell stays at its original cwd. Inside the resumed Claude session, `pwd` is the right clone/subfolder — which is what matters. Mutating the parent shell's cwd from a child process is a Unix impossibility.

## Status

v0.1.0 — early. Bug reports welcome.
