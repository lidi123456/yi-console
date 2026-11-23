//! Read-only permission classifier for AI code review side chats.
//!
//! Enforces strict read-only execution across all 7 AI providers:
//! - Read-only tools and read-only shell commands (`git diff`, `git show`, `rg`, `cat`, ...) -> `Allow`
//! - File writes, edits, deletes, and mutating shell commands (`rm`, `sed -i`, `git commit`, `> file`, ...) -> `Deny`
//! - Ambiguous commands (`cargo test`, unknown external tools) -> `Ask` (falls back to in-place card)

use serde_json::Value;

pub const READONLY_DENY_MSG: &str = "当前为严格只读代码审阅模式，禁止修改文件或改变仓库状态。请改用只读工具（如 yi_review 的 review_scope / review_diff / review_read_file / review_grep）完成审阅。";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewGuardDecision {
    Allow,
    Deny(&'static str),
    Ask,
}

const SHELL_TOOLS: &[&str] = &[
    "bash",
    "shell",
    "run_command",
    "execute_bash",
    "execute_command",
    "terminal",
    "powershell",
    "cmd",
    "run_in_terminal",
];

const READ_TOOLS: &[&str] = &[
    "read",
    "read_file",
    "view_file",
    "view_file_outline",
    "view_code_item",
    "glob",
    "find_by_name",
    "grep",
    "grep_search",
    "ripgrep",
    "ls",
    "list_dir",
    "find",
    "search",
    "codebase_search",
    "read_lints",
    "fs_read",
    "todoread",
    "file_read",
];

const WRITE_TOOLS: &[&str] = &[
    "write",
    "edit",
    "multiedit",
    "multi_edit",
    "notebookedit",
    "notebook_edit",
    "update",
    "delete",
    "move",
    "write_file",
    "edit_file",
    "replace_file_content",
    "multi_replace_file_content",
    "write_to_file",
    "apply_patch",
    "fs_write",
    "create_file",
    "delete_file",
    "remove_file",
    "rename_file",
    "patch_file",
    "mkdir",
    "todowrite",
    "file_write",
];

const MUTATING_BINS: &[&str] = &[
    "rm",
    "rmdir",
    "mv",
    "cp",
    "mkdir",
    "touch",
    "chmod",
    "chown",
    "chgrp",
    "ln",
    "dd",
    "truncate",
    "install",
    "mktemp",
    "shred",
    "unlink",
    "tee",
    "patch",
    "ed",
    "ex",
    "vi",
    "vim",
    "nano",
];

const READ_BINS: &[&str] = &[
    "cat",
    "head",
    "tail",
    "wc",
    "nl",
    "rg",
    "grep",
    "egrep",
    "fgrep",
    "ls",
    "dir",
    "tree",
    "pwd",
    "stat",
    "file",
    "which",
    "where",
    "whereis",
    "type",
    "dirname",
    "basename",
    "realpath",
    "readlink",
    "diff",
    "cut",
    "uniq",
    "tr",
    "jq",
    "echo",
    "printf",
    "true",
    "false",
    "test",
    "[",
    "get-content",
    "select-string",
    "get-childitem",
];

const GIT_READ_SUBCMDS: &[&str] = &[
    "diff",
    "show",
    "status",
    "log",
    "rev-parse",
    "merge-base",
    "ls-files",
    "ls-tree",
    "cat-file",
    "blame",
    "grep",
    "shortlog",
    "describe",
    "rev-list",
    "name-rev",
    "for-each-ref",
    "show-ref",
    "count-objects",
];

const GIT_WRITE_SUBCMDS: &[&str] = &[
    "commit",
    "push",
    "pull",
    "fetch",
    "reset",
    "checkout",
    "switch",
    "restore",
    "clean",
    "add",
    "stage",
    "rm",
    "mv",
    "apply",
    "rebase",
    "merge",
    "cherry-pick",
    "stash",
    "am",
    "bisect",
    "init",
    "clone",
    "revert",
    "format-patch",
    "gc",
    "prune",
];

/// Classify a permission request inside a read-only code review side chat.
pub fn classify_review_action(
    tool_name: &str,
    kind: Option<&str>,
    title: &str,
    input: &Value,
) -> ReviewGuardDecision {
    let raw_name = unwrap_tool_name(tool_name, input);
    let low_name = raw_name.trim().to_ascii_lowercase();
    let kind_low = kind.unwrap_or("").trim().to_ascii_lowercase();

    // 1. Built-in MCP gateways
    if low_name.starts_with("mcp__yi_review__") || low_name.starts_with("mcp__yi_session__") {
        return ReviewGuardDecision::Allow;
    }
    if let Some(mem_tool) = low_name.strip_prefix("mcp__yi_memory__") {
        return match mem_tool {
            "memory_list" | "memory_read" | "memory_search" => ReviewGuardDecision::Allow,
            _ => ReviewGuardDecision::Deny(READONLY_DENY_MSG),
        };
    }
    if low_name.starts_with("mcp__yi_memory__")
        || title.ends_with(" (yi_memory)")
    {
        if title.starts_with("memory_save ") || title.starts_with("memory_delete ") {
            return ReviewGuardDecision::Deny(READONLY_DENY_MSG);
        }
        if title.starts_with("memory_list ")
            || title.starts_with("memory_read ")
            || title.starts_with("memory_search ")
        {
            return ReviewGuardDecision::Allow;
        }
    }
    if title.ends_with(" (yi_review)") || title.ends_with(" (yi_session)") {
        return ReviewGuardDecision::Allow;
    }

    // 2. Explicit mutating tools or ACP edit/delete/move kinds
    if matches!(kind_low.as_str(), "edit" | "delete" | "move") || WRITE_TOOLS.contains(&low_name.as_str()) {
        return ReviewGuardDecision::Deny(READONLY_DENY_MSG);
    }
    if low_name == "str_replace_editor" {
        return if input["command"].as_str() == Some("view") {
            ReviewGuardDecision::Allow
        } else {
            ReviewGuardDecision::Deny(READONLY_DENY_MSG)
        };
    }

    // 3. Shell / execute commands
    if SHELL_TOOLS.contains(&low_name.as_str()) || kind_low == "execute" {
        if let Some(cmd) = extract_shell_command(&low_name, title, input) {
            return classify_shell_command(&cmd);
        }
        return ReviewGuardDecision::Ask;
    }

    // 4. Explicit read-only tools or ACP read/search/think kinds
    if READ_TOOLS.contains(&low_name.as_str()) || matches!(kind_low.as_str(), "read" | "search" | "think") {
        return ReviewGuardDecision::Allow;
    }

    ReviewGuardDecision::Ask
}

fn unwrap_tool_name<'a>(tool_name: &'a str, input: &'a Value) -> &'a str {
    if tool_name == "DeferExecuteTool" {
        if let Some(inner) = input["toolName"].as_str().filter(|s| !s.is_empty()) {
            return inner;
        }
    }
    tool_name
}

fn extract_shell_command(low_name: &str, title: &str, input: &Value) -> Option<String> {
    for key in ["command", "CommandLine", "cmd"] {
        match &input[key] {
            Value::String(s) if !s.trim().is_empty() => return Some(s.trim().to_string()),
            Value::Array(a) if !a.is_empty() => {
                let parts: Vec<&str> = a.iter().filter_map(|v| v.as_str()).collect();
                if !parts.is_empty() {
                    return Some(parts.join(" "));
                }
            }
            _ => {}
        }
    }
    if SHELL_TOOLS.contains(&low_name) {
        let mut t = title.trim();
        for p in ["执行命令: ", "Bash: ", "Shell: ", "Run command: "] {
            if let Some(rest) = t.strip_prefix(p) {
                t = rest.trim();
                break;
            }
        }
        if !t.is_empty() && !t.eq_ignore_ascii_case(low_name) {
            return Some(t.to_string());
        }
    }
    None
}

fn strip_outer_shell(cmd: &str) -> String {
    let mut cur = crate::adapters::codex::strip_shell_wrapper(cmd.trim());
    for prefix in [
        "/bin/sh -lc ",
        "sh -lc ",
        "sh -c ",
        "zsh -lc ",
        "zsh -c ",
        "/bin/zsh -c ",
        "cmd /c ",
        "cmd.exe /c ",
        "powershell -Command ",
        "powershell -NoProfile -Command ",
    ] {
        if let Some(rest) = cur.strip_prefix(prefix) {
            let r = rest.trim();
            if r.len() >= 2
                && ((r.starts_with('\'') && r.ends_with('\''))
                    || (r.starts_with('"') && r.ends_with('"')))
            {
                cur = r[1..r.len() - 1].to_string();
            } else {
                cur = r.to_string();
            }
            break;
        }
    }
    cur
}

fn classify_shell_command(raw: &str) -> ReviewGuardDecision {
    let cmd = strip_outer_shell(raw);
    let trimmed = cmd.trim();
    if trimmed.is_empty() {
        return ReviewGuardDecision::Ask;
    }

    if has_disallowed_redirection(trimmed) {
        return ReviewGuardDecision::Deny(READONLY_DENY_MSG);
    }
    if has_subshell_or_proc_subst(trimmed) {
        return ReviewGuardDecision::Ask;
    }

    let Some(segments) = split_pipeline_segments(trimmed) else {
        return ReviewGuardDecision::Ask;
    };
    if segments.is_empty() {
        return ReviewGuardDecision::Ask;
    }

    let mut all_allow = true;
    for seg in segments {
        match classify_segment(&seg) {
            ReviewGuardDecision::Deny(msg) => return ReviewGuardDecision::Deny(msg),
            ReviewGuardDecision::Ask => all_allow = false,
            ReviewGuardDecision::Allow => {}
        }
    }
    if all_allow {
        ReviewGuardDecision::Allow
    } else {
        ReviewGuardDecision::Ask
    }
}

fn has_disallowed_redirection(cmd: &str) -> bool {
    let bytes = cmd.as_bytes();
    let mut in_single = false;
    let mut in_double = false;
    let mut escape = false;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if escape {
            escape = false;
            i += 1;
            continue;
        }
        if b == b'\\' && !in_single {
            escape = true;
            i += 1;
            continue;
        }
        if b == b'\'' && !in_double {
            in_single = !in_single;
            i += 1;
            continue;
        }
        if b == b'"' && !in_single {
            in_double = !in_double;
            i += 1;
            continue;
        }
        if !in_single && !in_double {
            if b == b'>' {
                let tail = &cmd[i..];
                let prev = i.checked_sub(1).and_then(|k| bytes.get(k)).copied();
                let is_stderr = prev == Some(b'2')
                    && (i == 1 || bytes[i - 2].is_ascii_whitespace());
                let is_stdout = prev == Some(b'1')
                    && (i == 1 || bytes[i - 2].is_ascii_whitespace());
                let harmless = tail.starts_with(">&2")
                    || tail.starts_with(">/dev/null")
                    || tail.starts_with("> /dev/null")
                    || ((is_stderr || is_stdout)
                        && (tail.starts_with(">&1") || tail.starts_with(">&2")));
                if !harmless {
                    return true;
                }
            }
        }
        i += 1;
    }
    false
}

fn has_subshell_or_proc_subst(cmd: &str) -> bool {
    let bytes = cmd.as_bytes();
    let mut in_single = false;
    let mut in_double = false;
    let mut escape = false;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if escape {
            escape = false;
            i += 1;
            continue;
        }
        if b == b'\\' && !in_single {
            escape = true;
            i += 1;
            continue;
        }
        if b == b'\'' && !in_double {
            in_single = !in_single;
            i += 1;
            continue;
        }
        if b == b'"' && !in_single {
            in_double = !in_double;
            i += 1;
            continue;
        }
        if !in_single {
            if b == b'`' {
                return true;
            }
            if b == b'$' && bytes.get(i + 1) == Some(&b'(') {
                return true;
            }
            if !in_double && matches!(b, b'<' | b'>') && bytes.get(i + 1) == Some(&b'(') {
                return true;
            }
        }
        i += 1;
    }
    false
}

fn split_pipeline_segments(cmd: &str) -> Option<Vec<String>> {
    let bytes = cmd.as_bytes();
    let mut out = Vec::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut escape = false;
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if escape {
            escape = false;
            i += 1;
            continue;
        }
        if b == b'\\' && !in_single {
            escape = true;
            i += 1;
            continue;
        }
        if b == b'\'' && !in_double {
            in_single = !in_single;
            i += 1;
            continue;
        }
        if b == b'"' && !in_single {
            in_double = !in_double;
            i += 1;
            continue;
        }
        if !in_single && !in_double {
            if b == b'&' && bytes.get(i + 1) == Some(&b'&') {
                out.push(cmd[start..i].trim().to_string());
                i += 2;
                start = i;
                continue;
            }
            if b == b'|' && bytes.get(i + 1) == Some(&b'|') {
                out.push(cmd[start..i].trim().to_string());
                i += 2;
                start = i;
                continue;
            }
            if b == b'|' || b == b';' || b == b'\n' {
                out.push(cmd[start..i].trim().to_string());
                i += 1;
                start = i;
                continue;
            }
        }
        i += 1;
    }
    if in_single || in_double {
        return None;
    }
    out.push(cmd[start..].trim().to_string());
    out.retain(|s| !s.is_empty());
    Some(out)
}

fn tokenize(seg: &str) -> Option<Vec<String>> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut in_token = false;
    let mut in_single = false;
    let mut in_double = false;
    let mut escape = false;
    for ch in seg.chars() {
        if escape {
            cur.push(ch);
            escape = false;
            in_token = true;
            continue;
        }
        if ch == '\\' && !in_single {
            escape = true;
            in_token = true;
            continue;
        }
        if ch == '\'' && !in_double {
            in_single = !in_single;
            in_token = true;
            continue;
        }
        if ch == '"' && !in_single {
            in_double = !in_double;
            in_token = true;
            continue;
        }
        if !in_single && !in_double && ch.is_whitespace() {
            if in_token {
                tokens.push(std::mem::take(&mut cur));
                in_token = false;
            }
            continue;
        }
        cur.push(ch);
        in_token = true;
    }
    if in_single || in_double || escape {
        return None;
    }
    if in_token {
        tokens.push(cur);
    }
    Some(tokens)
}

fn strip_env_prefix(tokens: &[String]) -> &[String] {
    let mut idx = 0;
    while idx < tokens.len() {
        let t = &tokens[idx];
        if t.contains('=') && !t.starts_with('-') && t.split_once('=').is_some_and(|(k, _)| !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')) {
            idx += 1;
            continue;
        }
        if t == "env" {
            idx += 1;
            continue;
        }
        break;
    }
    &tokens[idx..]
}

fn classify_segment(seg: &str) -> ReviewGuardDecision {
    let Some(raw_tokens) = tokenize(seg) else {
        return ReviewGuardDecision::Ask;
    };
    let tokens = strip_env_prefix(&raw_tokens);
    let Some(first) = tokens.first() else {
        return ReviewGuardDecision::Ask;
    };
    let bin = first
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(first)
        .trim_end_matches(".exe")
        .to_ascii_lowercase();
    let rest = &tokens[1..];

    if MUTATING_BINS.contains(&bin.as_str()) {
        return ReviewGuardDecision::Deny(READONLY_DENY_MSG);
    }

    match bin.as_str() {
        "git" => classify_git(rest),
        "sed" => {
            if rest.iter().any(|a| a == "-i" || a.starts_with("-i") || a == "--in-place" || a.starts_with("--in-place=")) {
                ReviewGuardDecision::Deny(READONLY_DENY_MSG)
            } else if rest.iter().any(|a| a == "-f" || a == "-e") && rest.iter().any(|a| a.contains("w ")) {
                ReviewGuardDecision::Ask
            } else {
                ReviewGuardDecision::Allow
            }
        }
        "perl" => {
            if rest.iter().any(|a| a == "-i" || a.starts_with("-i")) {
                ReviewGuardDecision::Deny(READONLY_DENY_MSG)
            } else {
                ReviewGuardDecision::Ask
            }
        }
        "sort" => {
            if rest.iter().any(|a| a == "-o" || a.starts_with("-o") || a == "--output" || a.starts_with("--output=")) {
                ReviewGuardDecision::Deny(READONLY_DENY_MSG)
            } else {
                ReviewGuardDecision::Allow
            }
        }
        "find" => {
            if rest.iter().any(|a| matches!(a.as_str(), "-delete" | "-exec" | "-execdir" | "-ok" | "-okdir")) {
                ReviewGuardDecision::Deny(READONLY_DENY_MSG)
            } else {
                ReviewGuardDecision::Allow
            }
        }
        "awk" | "gawk" => {
            let joined = rest.join(" ");
            if joined.contains("system(") || joined.contains('>') {
                ReviewGuardDecision::Deny(READONLY_DENY_MSG)
            } else {
                ReviewGuardDecision::Allow
            }
        }
        "npm" | "pnpm" | "yarn" => {
            if rest.first().is_some_and(|s| matches!(s.as_str(), "install" | "i" | "add" | "remove" | "uninstall" | "update" | "ci" | "publish")) {
                ReviewGuardDecision::Deny(READONLY_DENY_MSG)
            } else {
                ReviewGuardDecision::Ask
            }
        }
        "pip" | "pip3" => {
            if rest.first().is_some_and(|s| matches!(s.as_str(), "install" | "uninstall")) {
                ReviewGuardDecision::Deny(READONLY_DENY_MSG)
            } else {
                ReviewGuardDecision::Ask
            }
        }
        "cargo" => {
            match rest.first().map(String::as_str) {
                Some("add" | "remove" | "fix" | "install" | "uninstall" | "publish") => {
                    ReviewGuardDecision::Deny(READONLY_DENY_MSG)
                }
                Some("fmt") if !rest.iter().any(|a| a == "--check") => {
                    ReviewGuardDecision::Deny(READONLY_DENY_MSG)
                }
                _ => ReviewGuardDecision::Ask,
            }
        }
        b if READ_BINS.contains(&b) => ReviewGuardDecision::Allow,
        _ => ReviewGuardDecision::Ask,
    }
}

fn classify_git(args: &[String]) -> ReviewGuardDecision {
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if matches!(a, "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace") {
            i += 2;
            continue;
        }
        if a.starts_with("--git-dir=")
            || a.starts_with("--work-tree=")
            || a.starts_with("-C")
            || a.starts_with("-c")
            || matches!(a, "--no-pager" | "-p" | "--paginate" | "--bare" | "--no-replace-objects" | "--literal-pathspecs")
        {
            i += 1;
            continue;
        }
        if a.starts_with('-') {
            i += 1;
            continue;
        }
        break;
    }
    let Some(sub) = args.get(i).map(String::as_str) else {
        return ReviewGuardDecision::Ask;
    };
    let rest = &args[i + 1..];

    if GIT_WRITE_SUBCMDS.contains(&sub) {
        return ReviewGuardDecision::Deny(READONLY_DENY_MSG);
    }
    if GIT_READ_SUBCMDS.contains(&sub) {
        if rest.iter().any(|a| a.starts_with("--output=")) {
            return ReviewGuardDecision::Deny(READONLY_DENY_MSG);
        }
        return ReviewGuardDecision::Allow;
    }
    match sub {
        "branch" => {
            if rest.iter().any(|a| matches!(a.as_str(), "-d" | "-D" | "-m" | "-M" | "-c" | "-C" | "-f" | "--delete" | "--move" | "--copy")) {
                ReviewGuardDecision::Deny(READONLY_DENY_MSG)
            } else if rest.iter().all(|a| matches!(a.as_str(), "-a" | "--all" | "-r" | "--remotes" | "-v" | "-vv" | "--verbose" | "--list" | "--show-current" | "--merged" | "--no-merged" | "--contains")) {
                ReviewGuardDecision::Allow
            } else {
                ReviewGuardDecision::Ask
            }
        }
        "tag" => {
            if rest.iter().any(|a| matches!(a.as_str(), "-d" | "--delete" | "-f" | "--force" | "-a" | "-s")) {
                ReviewGuardDecision::Deny(READONLY_DENY_MSG)
            } else if rest.is_empty() || rest.iter().any(|a| matches!(a.as_str(), "-l" | "--list")) {
                ReviewGuardDecision::Allow
            } else {
                ReviewGuardDecision::Ask
            }
        }
        _ => ReviewGuardDecision::Ask,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn allows_readonly_tools_and_git_commands() {
        assert_eq!(classify_review_action("Read", None, "Read file", &json!({})), ReviewGuardDecision::Allow);
        assert_eq!(classify_review_action("mcp__yi_review__review_diff", None, "", &json!({})), ReviewGuardDecision::Allow);
        assert_eq!(classify_review_action("mcp__yi_memory__memory_read", None, "", &json!({})), ReviewGuardDecision::Allow);
        assert_eq!(
            classify_review_action("Bash", None, "", &json!({"command": "git -C /repo diff main...HEAD -- src/lib.rs | head -n 80"})),
            ReviewGuardDecision::Allow
        );
        assert_eq!(
            classify_review_action("shell", Some("execute"), "", &json!({"command": "/bin/bash -lc 'GIT_OPTIONAL_LOCKS=0 git status --short && rg foo src/'"})),
            ReviewGuardDecision::Allow
        );
        assert_eq!(
            classify_review_action("Bash", None, "", &json!({"command": "git show abc123 2>/dev/null"})),
            ReviewGuardDecision::Allow
        );
    }

    #[test]
    fn denies_write_tools_and_mutating_commands() {
        assert!(matches!(classify_review_action("Edit", None, "Edit file", &json!({})), ReviewGuardDecision::Deny(_)));
        assert!(matches!(classify_review_action("tool", Some("edit"), "Edit", &json!({})), ReviewGuardDecision::Deny(_)));
        assert!(matches!(classify_review_action("mcp__yi_memory__memory_save", None, "", &json!({})), ReviewGuardDecision::Deny(_)));
        assert!(matches!(
            classify_review_action("Bash", None, "", &json!({"command": "sed -i 's/a/b/' foo.rs"})),
            ReviewGuardDecision::Deny(_)
        ));
        assert!(matches!(
            classify_review_action("Bash", None, "", &json!({"command": "echo hi > out.txt"})),
            ReviewGuardDecision::Deny(_)
        ));
        assert!(matches!(
            classify_review_action("Bash", None, "", &json!({"command": "git diff && git commit -m x"})),
            ReviewGuardDecision::Deny(_)
        ));
        assert!(matches!(
            classify_review_action("Bash", None, "", &json!({"command": "rm -rf tmp"})),
            ReviewGuardDecision::Deny(_)
        ));
    }

    #[test]
    fn asks_on_ambiguous_commands() {
        assert_eq!(
            classify_review_action("Bash", None, "", &json!({"command": "cargo test --lib"})),
            ReviewGuardDecision::Ask
        );
        assert_eq!(
            classify_review_action("unknown_mcp_tool", None, "Something", &json!({})),
            ReviewGuardDecision::Ask
        );
    }
}
