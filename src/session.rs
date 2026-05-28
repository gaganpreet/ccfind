#![allow(dead_code)]
use serde::Deserialize;

const MAX_BODY_BYTES: usize = 64 * 1024;

#[derive(Debug, Deserialize)]
pub struct Event {
    #[serde(rename = "type")]
    pub event_type: Option<String>,
    #[serde(default)]
    pub uuid: Option<String>,
    #[serde(rename = "sessionId", default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub timestamp: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(rename = "gitBranch", default)]
    pub git_branch: Option<String>,
    #[serde(default)]
    pub slug: Option<String>,
    #[serde(default)]
    pub message: Option<Message>,
    #[serde(rename = "isMeta", default)]
    pub is_meta: bool,
    #[serde(rename = "isSidechain", default)]
    pub is_sidechain: bool,
    // Some non-message event types put their text in different keys:
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub content: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct Message {
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub content: Option<Content>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Content {
    Text(String),
    Blocks(Vec<Block>),
}

#[derive(Debug, Deserialize)]
pub struct Block {
    #[serde(rename = "type", default)]
    pub block_type: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub thinking: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub input: Option<serde_json::Value>,
    // tool_result blocks:
    #[serde(rename = "tool_use_id", default)]
    pub tool_use_id: Option<String>,
    #[serde(default)]
    pub content: Option<serde_json::Value>,
}

/// What we want to do with this event.
#[derive(Debug, Clone)]
pub enum Action {
    /// Insert one FTS row with `scope` and `body`.
    Index { scope: Scope, body: String },
    /// Insert multiple FTS rows (e.g., assistant text + N tool_use blocks).
    IndexMany(Vec<(Scope, String)>),
    /// Update session-level metadata (no FTS row).
    SetTitle(String),
    SetLastPrompt(String),
    /// Skip this event entirely.
    Skip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    User,
    Assistant,
    Tool,
}

impl Scope {
    pub fn as_str(&self) -> &'static str {
        match self {
            Scope::User => "user",
            Scope::Assistant => "assistant",
            Scope::Tool => "tool",
        }
    }
}

impl Event {
    /// Map this event into an Action per the dispatch table in the plan.
    pub fn classify(&self) -> Action {
        let t = self.event_type.as_deref().unwrap_or("");
        match t {
            "user" => self.classify_user(),
            "assistant" => self.classify_assistant(),
            "summary" => self.summary.as_ref().map(|s| Action::SetTitle(s.clone())).unwrap_or(Action::Skip),
            "custom-title" | "ai-title" => {
                // Both fields seen in practice; pick whichever is populated.
                let v = self.title.as_ref().or(self.text.as_ref()).or(self.summary.as_ref());
                v.map(|s| Action::SetTitle(s.clone())).unwrap_or(Action::Skip)
            }
            "last-prompt" => self
                .prompt
                .as_ref()
                .or(self.text.as_ref())
                .map(|s| Action::SetLastPrompt(s.clone()))
                .unwrap_or(Action::Skip),
            "system" => {
                // System messages can carry useful context for assistant scope.
                if let Some(s) = self.text.as_ref().or(self.summary.as_ref()) {
                    if !s.trim().is_empty() {
                        return Action::Index { scope: Scope::Assistant, body: truncate(s, MAX_BODY_BYTES) };
                    }
                }
                Action::Skip
            }
            // Everything else: noise.
            _ => Action::Skip,
        }
    }

    fn classify_user(&self) -> Action {
        // Skip slash-command body expansions.
        if self.is_meta {
            return Action::Skip;
        }
        let Some(msg) = &self.message else { return Action::Skip };
        let Some(content) = &msg.content else { return Action::Skip };
        match content {
            Content::Text(s) => {
                if s.trim().is_empty() {
                    Action::Skip
                } else {
                    Action::Index { scope: Scope::User, body: truncate(s, MAX_BODY_BYTES) }
                }
            }
            Content::Blocks(blocks) => {
                // If ANY block is tool_result, the whole event is tool-output echo. Skip.
                if blocks.iter().any(|b| b.block_type.as_deref() == Some("tool_result")) {
                    return Action::Skip;
                }
                let joined = blocks
                    .iter()
                    .filter_map(|b| match b.block_type.as_deref() {
                        Some("text") => b.text.as_deref(),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if joined.trim().is_empty() {
                    Action::Skip
                } else {
                    Action::Index { scope: Scope::User, body: truncate(&joined, MAX_BODY_BYTES) }
                }
            }
        }
    }

    fn classify_assistant(&self) -> Action {
        let Some(msg) = &self.message else { return Action::Skip };
        let Some(content) = &msg.content else { return Action::Skip };
        let blocks = match content {
            Content::Text(s) => {
                // Rare but observed: assistant with string content.
                if s.trim().is_empty() {
                    return Action::Skip;
                }
                return Action::Index {
                    scope: Scope::Assistant,
                    body: truncate(s, MAX_BODY_BYTES),
                };
            }
            Content::Blocks(b) => b,
        };

        let mut rows: Vec<(Scope, String)> = Vec::new();
        let mut text_parts: Vec<&str> = Vec::new();
        for b in blocks {
            match b.block_type.as_deref() {
                Some("text") => {
                    if let Some(t) = b.text.as_deref() {
                        if !t.trim().is_empty() {
                            text_parts.push(t);
                        }
                    }
                }
                Some("thinking") => { /* skip */ }
                Some("tool_use") => {
                    let body = format_tool_use(b);
                    if !body.is_empty() {
                        rows.push((Scope::Tool, truncate(&body, MAX_BODY_BYTES)));
                    }
                }
                _ => {}
            }
        }
        if !text_parts.is_empty() {
            let joined = text_parts.join("\n");
            rows.insert(0, (Scope::Assistant, truncate(&joined, MAX_BODY_BYTES)));
        }
        match rows.len() {
            0 => Action::Skip,
            1 => {
                let (scope, body) = rows.into_iter().next().unwrap();
                Action::Index { scope, body }
            }
            _ => Action::IndexMany(rows),
        }
    }
}

fn format_tool_use(b: &Block) -> String {
    let name = b.name.as_deref().unwrap_or("Tool");
    let input = match &b.input {
        Some(v) => v,
        None => return format!("{}:", name),
    };
    let key = match name {
        "Bash" => input.get("command").and_then(|v| v.as_str()).map(String::from),
        "Read" | "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => {
            input.get("file_path").and_then(|v| v.as_str()).map(String::from)
        }
        "Glob" => input.get("pattern").and_then(|v| v.as_str()).map(String::from),
        "Grep" => input.get("pattern").and_then(|v| v.as_str()).map(String::from),
        "Task" | "Agent" => {
            let desc = input.get("description").and_then(|v| v.as_str()).unwrap_or("");
            let sub = input.get("subagent_type").and_then(|v| v.as_str()).unwrap_or("");
            Some(format!("{} ({})", desc, sub))
        }
        "WebFetch" | "WebSearch" => input.get("url").and_then(|v| v.as_str()).map(String::from)
            .or_else(|| input.get("query").and_then(|v| v.as_str()).map(String::from)),
        _ => None,
    };
    let key = key.unwrap_or_else(|| {
        let s = serde_json::to_string(input).unwrap_or_default();
        truncate(&s, 512)
    });
    // For Edit/Write, also include a snippet of new_string for searchability.
    let extra = match name {
        "Edit" | "Write" => input.get("new_string").or_else(|| input.get("content"))
            .and_then(|v| v.as_str())
            .map(|s| format!(" {}", truncate(s, 1024)))
            .unwrap_or_default(),
        _ => String::new(),
    };
    format!("{}: {}{}", name, key, extra)
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        // Find a UTF-8 boundary at or before `max`.
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

/// Parse the ISO8601 timestamp to a unix epoch seconds. Returns 0 on failure.
pub fn parse_ts(s: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.timestamp())
        .unwrap_or(0)
}
