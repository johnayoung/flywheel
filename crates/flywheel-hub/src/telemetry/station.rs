//! Where a tool call went: the kind of work it was, read from its name and
//! summary, for drawing a turn by what it did (reading files, editing,
//! the shell, the network) rather than as a list of calls. A shell
//! command goes by its program (see [`super::shape`]): `grep` and `cat`
//! read files, `curl`, `gh`, and `git push` reach the network.

use super::shape;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Station {
    /// Reading and searching files.
    Files,
    /// Writing and editing files.
    Edits,
    /// Any other shell command.
    Shell,
    /// Wait loops (`sleep`, `until`, `while`).
    Wait,
    /// The web, MCP servers, and shell commands that reach the network.
    Network,
    Browser,
    /// Subagents.
    Agents,
    /// Skills and tool search.
    Skills,
    /// The agent's plan and progress tools.
    Plan,
    /// The agent's memory files.
    Memory,
    /// A question put to the operator.
    You,
    Other,
}

impl Station {
    pub fn name(self) -> &'static str {
        match self {
            Self::Files => "files",
            Self::Edits => "edits",
            Self::Shell => "shell",
            Self::Wait => "wait",
            Self::Network => "network",
            Self::Browser => "browser",
            Self::Agents => "agents",
            Self::Skills => "skills",
            Self::Plan => "plan",
            Self::Memory => "memory",
            Self::You => "you",
            Self::Other => "other",
        }
    }
}

const READS: &[&str] = &[
    "Read",
    "Grep",
    "Glob",
    "LS",
    "NotebookRead",
    "view_image",
    "read_file",
    "list_dir",
    "grep_files",
];
const SHELLS: &[&str] = &[
    "Bash",
    "shell",
    "local_shell",
    "exec_command",
    "shell_command",
    "PowerShell",
];
/// Shell programs that only read or move files.
const FILE_PROGRAMS: &[&str] = &[
    "cat", "head", "tail", "less", "grep", "rg", "find", "fd", "ls", "tree", "wc", "sed", "awk",
    "mv", "cp", "rm", "mkdir", "touch", "tee", "diff", "stat", "file", "du", "jq",
];
/// Shell programs that reach the network.
const NETWORK_PROGRAMS: &[&str] = &[
    "curl", "wget", "gh", "ssh", "scp", "rsync", "nc", "ping", "dig", "az", "gcloud", "aws",
];
/// `program verb` shapes that reach the network.
const NETWORK_VERBS: &[&str] = &[
    "git push",
    "git pull",
    "git fetch",
    "git clone",
    "npm install",
    "npm i",
    "pnpm install",
    "yarn install",
    "pip install",
    "pip3 install",
    "uv pip",
    "cargo install",
    "cargo publish",
    "docker pull",
    "docker push",
];

/// The station of a call of `tool` that acts on `summary`.
pub fn of(tool: &str, summary: Option<&str>) -> Station {
    let summary = summary.unwrap_or_default();
    if memory_path(summary) && (READS.contains(&tool) || is_edit(tool)) {
        return Station::Memory;
    }
    if READS.contains(&tool) {
        return Station::Files;
    }
    if is_edit(tool) {
        return Station::Edits;
    }
    if SHELLS.contains(&tool) {
        return shell(summary);
    }
    let lower = tool.to_ascii_lowercase();
    if lower.contains("browser") || lower.starts_with("mcp__playwright") {
        return Station::Browser;
    }
    match tool {
        "Agent" | "Task" | "spawn_agent" | "SendMessage" | "SubagentHandback" | "TaskStop"
        | "TaskOutput" | "Workflow" => Station::Agents,
        "Monitor" | "ScheduleWakeup" | "Sleep" => Station::Wait,
        "Skill" | "ToolSearch" | "SlashCommand" => Station::Skills,
        "TodoWrite" | "TaskCreate" | "TaskUpdate" | "TaskList" | "update_plan" => Station::Plan,
        "AskUserQuestion" => Station::You,
        "WebFetch" | "WebSearch" | "web_search" => Station::Network,
        _ if lower.contains("report_progress") => Station::Plan,
        _ if lower.contains("ask_operator") => Station::You,
        _ if lower.contains("memory") => Station::Memory,
        _ if tool.starts_with("mcp__") => Station::Network,
        _ => Station::Other,
    }
}

fn is_edit(tool: &str) -> bool {
    matches!(
        tool,
        "Write" | "Edit" | "MultiEdit" | "NotebookEdit" | "apply_patch"
    )
}

/// Claude Code's memory folder and the files it names memory.
fn memory_path(path: &str) -> bool {
    (path.contains("/.claude/") && path.contains("/memory/")) || path.ends_with("MEMORY.md")
}

fn shell(command: &str) -> Station {
    let found = shape::command(command);
    let found = found.strip_suffix(" &").unwrap_or(&found);
    if shape::is_poll(found) {
        return Station::Wait;
    }
    let program = found.split(' ').next().unwrap_or_default();
    if NETWORK_PROGRAMS.contains(&program)
        || NETWORK_VERBS
            .iter()
            .any(|v| found == *v || found.starts_with(&format!("{v} ")))
    {
        return Station::Network;
    }
    if FILE_PROGRAMS.contains(&program) {
        return Station::Files;
    }
    Station::Shell
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calls_go_to_the_station_of_what_they_do() {
        let cases = [
            ("Read", "/repo/src/main.rs", Station::Files),
            ("Grep", "fn main", Station::Files),
            ("Edit", "/repo/src/main.rs", Station::Edits),
            (
                "Write",
                "/home/u/.claude/projects/x/memory/a.md",
                Station::Memory,
            ),
            ("Bash", "cd /repo && cargo test -p x", Station::Shell),
            ("Bash", "grep -rn foo src | head", Station::Files),
            ("Bash", "gh run watch 12", Station::Network),
            ("Bash", "git push origin main", Station::Network),
            ("Bash", "git status", Station::Shell),
            ("Bash", "sleep 30 && gh run view 1", Station::Wait),
            ("exec_command", "curl -s https://x", Station::Network),
            ("apply_patch", "", Station::Edits),
            ("WebFetch", "https://x", Station::Network),
            ("mcp__playwright__browser_click", "", Station::Browser),
            ("mcp__context7__query-docs", "", Station::Network),
            ("Agent", "explore", Station::Agents),
            ("SendMessage", "", Station::Agents),
            ("Monitor", "", Station::Wait),
            ("Skill", "fw-plan", Station::Skills),
            ("TodoWrite", "", Station::Plan),
            ("mcp__flywheel__report_progress", "", Station::Plan),
            ("AskUserQuestion", "", Station::You),
            ("Mystery", "", Station::Other),
        ];
        for (tool, summary, want) in cases {
            assert_eq!(of(tool, Some(summary)), want, "{tool} {summary}");
        }
    }
}
