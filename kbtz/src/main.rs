mod cli;

use std::io::{IsTerminal, Read as _};
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::Parser;
use rusqlite::Connection;

use cli::{Cli, Command};
use kbtz::{db, ops, output, tui, watch};
use ops::StatusFilter;

fn default_db_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME environment variable not set")?;
    Ok(PathBuf::from(home).join(".kbtz").join("kbtz.db"))
}

fn resolve_db_path(cli_db: Option<String>) -> Result<String> {
    match cli_db {
        Some(p) => Ok(p),
        None => {
            let path = default_db_path()?;
            Ok(path
                .to_str()
                .context("default DB path is not valid UTF-8")?
                .to_string())
        }
    }
}

fn ensure_db_dir(db_path: &str) -> Result<()> {
    if let Some(parent) = std::path::Path::new(db_path).parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create directory {}", parent.display()))?;
        }
    }
    Ok(())
}

fn open_db(db_path: &str) -> Result<Connection> {
    let conn = db::open(db_path)?;
    db::init(&conn)?;
    Ok(conn)
}

/// Dispatch a single parsed command against an open database connection.
/// Used both for direct invocations and within `exec` batches.
fn dispatch(conn: &Connection, command: Command) -> Result<()> {
    match command {
        Command::Add {
            name,
            parent,
            desc,
            note,
            claim,
            paused,
            json,
        } => {
            ops::add_task(
                conn,
                &name,
                parent.as_deref(),
                &desc,
                note.as_deref(),
                claim.as_deref(),
                paused,
            )?;
            if json {
                let task = ops::get_task(conn, &name)?;
                let notes = ops::list_notes(conn, &name)?;
                let blockers = ops::get_blockers(conn, &name)?;
                let dependents = ops::get_dependents(conn, &name)?;
                let detail = output::TaskDetail {
                    task: &task,
                    notes: &notes,
                    blocked_by: &blockers,
                    blocks: &dependents,
                };
                println!("{}", serde_json::to_string_pretty(&detail)?);
            }
            eprintln!("Added task '{name}'");
            if paused {
                eprintln!("Task '{name}' created in paused state");
            } else if let Some(assignee) = &claim {
                eprintln!("Claimed '{name}' for '{assignee}'");
            }
        }

        Command::Claim { name, assignee } => {
            ops::claim_task(conn, &name, &assignee)?;
            eprintln!("Claimed '{name}' for '{assignee}'");
        }

        Command::ClaimNext {
            assignee,
            prefer,
            json,
        } => match ops::claim_next_task(conn, &assignee, prefer.as_deref())? {
            Some(name) => {
                let task = ops::get_task(conn, &name)?;
                let notes = ops::list_notes(conn, &name)?;
                let blockers = ops::get_blockers(conn, &name)?;
                let dependents = ops::get_dependents(conn, &name)?;
                if json {
                    let detail = output::TaskDetail {
                        task: &task,
                        notes: &notes,
                        blocked_by: &blockers,
                        blocks: &dependents,
                    };
                    println!("{}", serde_json::to_string_pretty(&detail)?);
                } else {
                    print!(
                        "{}",
                        output::format_task_detail(&task, &notes, &blockers, &dependents)
                    );
                }
                eprintln!("Claimed '{name}' for '{assignee}'");
            }
            None => {
                bail!("no tasks available");
            }
        },

        Command::Steal { name, assignee } => {
            let prev = ops::steal_task(conn, &name, &assignee)?;
            eprintln!("Stole '{name}' from '{prev}' to '{assignee}'");
        }

        Command::Release { name, assignee } => {
            ops::release_task(conn, &name, &assignee)?;
            eprintln!("Released '{name}'");
        }

        Command::ForceUnassign { name } => {
            ops::force_unassign_task(conn, &name)?;
            eprintln!("Force-unassigned '{name}'");
        }

        Command::Done { name } => {
            ops::mark_done(conn, &name)?;
            eprintln!("Marked '{name}' as done");
        }

        Command::Reopen { name } => {
            ops::reopen_task(conn, &name)?;
            eprintln!("Reopened '{name}'");
        }

        Command::Pause { name } => {
            ops::pause_task(conn, &name)?;
            eprintln!("Paused '{name}'");
        }

        Command::Unpause { name } => {
            ops::unpause_task(conn, &name)?;
            eprintln!("Unpaused '{name}'");
        }

        Command::Reparent { name, parent } => {
            ops::reparent_task(conn, &name, parent.as_deref())?;
            match parent.as_deref() {
                Some(p) => eprintln!("Moved '{name}' under '{p}'"),
                None => eprintln!("Moved '{name}' to root level"),
            }
        }

        Command::Describe { name, desc } => {
            ops::update_description(conn, &name, &desc)?;
            eprintln!("Updated description for '{name}'");
        }

        Command::Rm { name, recursive } => {
            ops::remove_task(conn, &name, recursive)?;
            eprintln!("Removed task '{name}'");
        }

        Command::Show { name, json } => {
            let task = ops::get_task(conn, &name)?;
            let notes = ops::list_notes(conn, &name)?;
            let blockers = ops::get_blockers(conn, &name)?;
            let dependents = ops::get_dependents(conn, &name)?;
            if json {
                let detail = output::TaskDetail {
                    task: &task,
                    notes: &notes,
                    blocked_by: &blockers,
                    blocks: &dependents,
                };
                println!("{}", serde_json::to_string_pretty(&detail)?);
            } else {
                print!(
                    "{}",
                    output::format_task_detail(&task, &notes, &blockers, &dependents)
                );
            }
        }

        Command::List {
            tree,
            status,
            all,
            root,
            children,
            assignee,
            blocked,
            unblocked,
            json,
        } => {
            let status = status.map(|s| StatusFilter::parse(&s)).transpose()?;
            let blocked_filter = match (blocked, unblocked) {
                (true, _) => Some(true),
                (_, true) => Some(false),
                _ => None,
            };
            let tasks = if let Some(ref parent) = children {
                ops::list_children(
                    conn,
                    parent,
                    status,
                    all,
                    assignee.as_deref(),
                    blocked_filter,
                )?
            } else {
                ops::list_tasks(
                    conn,
                    status,
                    all,
                    root.as_deref(),
                    assignee.as_deref(),
                    blocked_filter,
                )?
            };
            if json {
                let mut deps = ops::get_all_deps(conn)?;
                let items: Vec<output::TaskListItem> = tasks
                    .iter()
                    .map(|t| {
                        let (blocked_by, blocks) = deps.remove(&t.name).unwrap_or_default();
                        output::TaskListItem {
                            task: t,
                            blocked_by,
                            blocks,
                        }
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&items)?);
            } else if tree {
                print!("{}", output::format_task_tree(&tasks));
            } else {
                print!("{}", output::format_task_list(&tasks));
            }
        }

        Command::Note { name, content } => {
            let content = match content {
                Some(c) => c,
                None => bail!(
                    "note content must be provided explicitly (stdin is not available inside exec)"
                ),
            };
            ops::add_note(conn, &name, &content)?;
            eprintln!("Added note to '{name}'");
        }

        Command::Notes { name, json } => {
            let notes = ops::list_notes(conn, &name)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&notes)?);
            } else {
                print!("{}", output::format_notes(&notes));
            }
        }

        Command::Block { blocker, blocked } => {
            ops::add_block(conn, &blocker, &blocked)?;
            eprintln!("'{blocker}' now blocks '{blocked}'");
        }

        Command::Unblock { blocker, blocked } => {
            ops::remove_block(conn, &blocker, &blocked)?;
            eprintln!("'{blocker}' no longer blocks '{blocked}'");
        }

        Command::Search { query, json } => {
            let results = ops::search_tasks(conn, &query)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&results)?);
            } else {
                print!("{}", output::format_search_results(&results));
            }
        }

        Command::Watch { .. } => bail!("watch cannot be used inside exec"),
        Command::Wait => bail!("wait cannot be used inside exec"),
        Command::Exec => bail!("exec cannot be nested"),
    }

    Ok(())
}

fn parse_exec_tokens(tokens: &[String], display_line: &str) -> Result<Command> {
    let mut args = vec!["kbtz".to_string()];
    args.extend(tokens.iter().cloned());
    let cli =
        Cli::try_parse_from(&args).with_context(|| format!("failed to parse: {display_line}"))?;
    Ok(cli.command)
}

/// Tokenize a line using double-quote-only quoting rules.
///
/// Unlike POSIX shell quoting (used by the `shlex` crate), single quotes are
/// treated as ordinary characters.  This means apostrophes in text like
/// `Here's` do not start quoted strings.  Only double quotes delimit strings,
/// with `\"` and `\\` as escape sequences inside them.  Backslashes outside
/// double quotes are also literal.
fn tokenize_exec_line(line: &str) -> Result<Vec<String>> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut chars = line.chars().peekable();
    let mut in_quotes = false;
    let mut in_token = false;

    while let Some(c) = chars.next() {
        if in_quotes {
            match c {
                '"' => in_quotes = false,
                '\\' => match chars.peek() {
                    Some(&'"') | Some(&'\\') => {
                        current.push(chars.next().unwrap());
                    }
                    _ => current.push(c),
                },
                _ => current.push(c),
            }
        } else {
            match c {
                '"' => {
                    in_quotes = true;
                    in_token = true;
                }
                c if c.is_ascii_whitespace() => {
                    if in_token {
                        tokens.push(std::mem::take(&mut current));
                        in_token = false;
                    }
                }
                _ => {
                    current.push(c);
                    in_token = true;
                }
            }
        }
    }

    if in_quotes {
        bail!("unterminated double quote");
    }

    if in_token {
        tokens.push(current);
    }

    Ok(tokens)
}

/// Check whether double quotes are balanced in a string, respecting escape sequences.
///
/// Returns `true` when every opening `"` has a matching closing `"`.
fn has_balanced_quotes(s: &str) -> bool {
    let mut in_quotes = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quotes {
            match c {
                '"' => in_quotes = false,
                '\\' => {
                    if matches!(chars.peek(), Some(&'"') | Some(&'\\')) {
                        chars.next();
                    }
                }
                _ => {}
            }
        } else if c == '"' {
            in_quotes = true;
        }
    }
    !in_quotes
}

/// Pre-process exec input to resolve heredoc syntax and multiline quoted strings.
///
/// A token of the form `<<DELIMITER` causes subsequent lines to be accumulated
/// until a line matching `DELIMITER` (after trimming) is found. The accumulated
/// text replaces the `<<DELIMITER` token. Only one heredoc per command line is
/// supported.
///
/// Double-quoted strings may span multiple lines. When a line has unbalanced
/// quotes, subsequent lines are joined (with embedded newlines) until the
/// quotes are balanced.
fn resolve_heredocs(input: &str) -> Result<Vec<(usize, String, Vec<String>)>> {
    let lines: Vec<&str> = input.lines().collect();
    let mut result = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i].trim();
        let lineno = i + 1;
        i += 1;

        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // Accumulate continuation lines when double quotes are unbalanced.
        let mut accumulated = line.to_string();
        while !has_balanced_quotes(&accumulated) {
            if i >= lines.len() {
                bail!("line {lineno}: unterminated double quote");
            }
            accumulated.push('\n');
            accumulated.push_str(lines[i]);
            i += 1;
        }

        let mut tokens = tokenize_exec_line(&accumulated)
            .with_context(|| format!("line {lineno}: invalid quoting: {line}"))?;

        // Find heredoc markers
        let heredoc_positions: Vec<usize> = tokens
            .iter()
            .enumerate()
            .filter(|(_, t)| t.starts_with("<<") && t.len() > 2)
            .map(|(i, _)| i)
            .collect();
        if heredoc_positions.len() > 1 {
            bail!("line {lineno}: only one heredoc per command is supported");
        }
        let heredoc_pos = heredoc_positions.into_iter().next();

        if let Some(pos) = heredoc_pos {
            let delimiter = tokens[pos][2..].to_string();
            let mut body_lines = Vec::new();
            let mut found = false;

            while i < lines.len() {
                if lines[i] == delimiter {
                    found = true;
                    i += 1;
                    break;
                }
                body_lines.push(lines[i]);
                i += 1;
            }

            if !found {
                bail!("line {lineno}: unterminated heredoc (expected closing '{delimiter}')");
            }

            tokens[pos] = body_lines.join("\n");
        }

        result.push((lineno, line.to_string(), tokens));
    }

    Ok(result)
}

fn run_exec(conn: &Connection, input: &str) -> Result<()> {
    let resolved = resolve_heredocs(input)?;

    // Parse all commands first, before starting the transaction
    let mut commands = Vec::new();
    for (lineno, line, tokens) in &resolved {
        let command = parse_exec_tokens(tokens, line).with_context(|| format!("line {lineno}"))?;
        // Reject commands that don't belong in a batch
        match &command {
            Command::Exec => bail!("line {lineno}: exec cannot be nested"),
            Command::Watch { .. } => bail!("line {lineno}: watch cannot be used inside exec"),
            Command::Wait => bail!("line {lineno}: wait cannot be used inside exec"),
            _ => {}
        }
        commands.push((*lineno, line.clone(), command));
    }

    if commands.is_empty() {
        return Ok(());
    }

    conn.execute_batch("BEGIN IMMEDIATE")?;

    let result = (|| -> Result<()> {
        for (lineno, line, command) in commands {
            dispatch(conn, command).with_context(|| format!("line {lineno}: {line}"))?;
        }
        Ok(())
    })();

    match result {
        Ok(()) => {
            conn.execute_batch("COMMIT")?;
            Ok(())
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

/// Check whether note content is available without blocking.
///
/// Returns `Ok(Some(content))` when the content argument was provided,
/// `Ok(None)` when stdin should be read (non-terminal), or an error when
/// no content was provided and stdin is a terminal (which would hang).
fn check_note_content(content: Option<String>, stdin_is_terminal: bool) -> Result<Option<String>> {
    match content {
        Some(c) => Ok(Some(c)),
        None if stdin_is_terminal => {
            bail!("no note content provided (pass content as argument or pipe via stdin)")
        }
        None => Ok(None),
    }
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let db_path = resolve_db_path(cli.db)?;
    ensure_db_dir(&db_path)?;

    match cli.command {
        Command::Exec => {
            let conn = open_db(&db_path)?;
            let mut input = String::new();
            std::io::stdin().read_to_string(&mut input)?;
            run_exec(&conn, &input)?;
        }

        Command::Note { name, content } => {
            let conn = open_db(&db_path)?;
            let content = match check_note_content(content, std::io::stdin().is_terminal())? {
                Some(c) => c,
                None => {
                    let mut buf = String::new();
                    std::io::stdin().read_to_string(&mut buf)?;
                    if buf.is_empty() {
                        bail!("no content provided");
                    }
                    buf
                }
            };
            ops::add_note(&conn, &name, &content)?;
            eprintln!("Added note to '{name}'");
        }

        Command::Watch {
            root,
            poll_interval,
        } => {
            let conn = open_db(&db_path)?;
            tui::run(&db_path, &conn, root.as_deref(), poll_interval)?;
        }

        Command::Wait => {
            // Ensure DB exists before watching
            let _conn = open_db(&db_path)?;
            let (_watcher, rx) = watch::watch_db(&db_path)?;
            // Block until a change event
            watch::wait_for_change(&rx, std::time::Duration::MAX);
        }

        other => {
            let conn = open_db(&db_path)?;
            dispatch(&conn, other)?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_conn() -> Connection {
        db::open_memory().unwrap()
    }

    #[test]
    fn exec_batch_commits_all() {
        let conn = test_conn();
        let input = r#"
add task-a "First task"
add task-b "Second task" -p task-a
block task-a task-b
note task-a "a note on task-a"
"#;
        run_exec(&conn, input).unwrap();

        // All operations should be visible
        let a = ops::get_task(&conn, "task-a").unwrap();
        assert_eq!(a.description, "First task");
        let b = ops::get_task(&conn, "task-b").unwrap();
        assert_eq!(b.parent.as_deref(), Some("task-a"));
        let blockers = ops::get_blockers(&conn, "task-b").unwrap();
        assert_eq!(blockers, vec!["task-a"]);
        let notes = ops::list_notes(&conn, "task-a").unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].content, "a note on task-a");
    }

    #[test]
    fn exec_batch_rolls_back_on_failure() {
        let conn = test_conn();
        // Second line refers to nonexistent parent — should fail and roll back the first add
        let input = r#"
add task-ok "This one is fine"
add task-bad "This will fail" -p nonexistent
"#;
        let result = run_exec(&conn, input);
        assert!(result.is_err());

        // task-ok should NOT exist because the batch was rolled back
        assert!(ops::get_task(&conn, "task-ok").is_err());
    }

    #[test]
    fn exec_skips_blanks_and_comments() {
        let conn = test_conn();
        let input = r#"
# This is a comment
add task-x "Created"

# Another comment

done task-x
"#;
        run_exec(&conn, input).unwrap();
        let task = ops::get_task(&conn, "task-x").unwrap();
        assert_eq!(task.status, "done");
    }

    #[test]
    fn exec_empty_input_is_noop() {
        let conn = test_conn();
        run_exec(&conn, "").unwrap();
        run_exec(&conn, "  \n  \n# only comments\n").unwrap();
    }

    #[test]
    fn exec_rejects_nested_exec() {
        let conn = test_conn();
        let input = "exec\n";
        let result = run_exec(&conn, input);
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("nested"),
            "expected nested exec error"
        );
    }

    #[test]
    fn exec_rejects_watch() {
        let conn = test_conn();
        let input = "watch\n";
        let result = run_exec(&conn, input);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("watch"));
    }

    #[test]
    fn exec_rejects_wait() {
        let conn = test_conn();
        let input = "wait\n";
        let result = run_exec(&conn, input);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("wait"));
    }

    #[test]
    fn exec_parse_error_reports_line() {
        let conn = test_conn();
        let input = "add task-a \"good\"\nnot-a-command\n";
        let result = run_exec(&conn, input);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("line 2"),
            "expected line number in error: {msg}"
        );
    }

    #[test]
    fn exec_quoted_args() {
        let conn = test_conn();
        let input = "add my-task \"a description with spaces\"\n";
        run_exec(&conn, input).unwrap();
        let task = ops::get_task(&conn, "my-task").unwrap();
        assert_eq!(task.description, "a description with spaces");
    }

    #[test]
    fn exec_claim_next_inside_transaction() {
        let conn = test_conn();
        let input = r#"
add task-1 "First"
add task-2 "Second"
claim-next agent-1
"#;
        run_exec(&conn, input).unwrap();
        // One of the tasks should be claimed
        let t1 = ops::get_task(&conn, "task-1").unwrap();
        let t2 = ops::get_task(&conn, "task-2").unwrap();
        let claimed_count = [&t1, &t2].iter().filter(|t| t.status == "active").count();
        assert_eq!(claimed_count, 1);
    }

    #[test]
    fn exec_runtime_error_reports_line() {
        let conn = test_conn();
        // done on a nonexistent task should fail at runtime
        let input = "add real-task \"exists\"\ndone nonexistent\n";
        let result = run_exec(&conn, input);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("line 2"),
            "expected line number in error: {msg}"
        );
        // First task should be rolled back
        assert!(ops::get_task(&conn, "real-task").is_err());
    }

    #[test]
    fn exec_heredoc_note() {
        let conn = test_conn();
        let input = "\
add my-task \"A task\"
note my-task <<END
Line one
Line two
END
";
        run_exec(&conn, input).unwrap();
        let notes = ops::list_notes(&conn, "my-task").unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].content, "Line one\nLine two");
    }

    #[test]
    fn exec_heredoc_description() {
        let conn = test_conn();
        let input = "\
add my-task <<EOF
A multiline
description here
EOF
";
        run_exec(&conn, input).unwrap();
        let task = ops::get_task(&conn, "my-task").unwrap();
        assert_eq!(task.description, "A multiline\ndescription here");
    }

    #[test]
    fn exec_heredoc_empty_body() {
        let conn = test_conn();
        let input = "\
add my-task <<EOF
EOF
";
        run_exec(&conn, input).unwrap();
        let task = ops::get_task(&conn, "my-task").unwrap();
        assert_eq!(task.description, "");
    }

    #[test]
    fn exec_heredoc_unterminated() {
        let conn = test_conn();
        let input = "\
add my-task <<EOF
this never ends
";
        let result = run_exec(&conn, input);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("unterminated heredoc"),
            "expected unterminated heredoc error: {msg}"
        );
    }

    #[test]
    fn exec_heredoc_with_other_commands() {
        let conn = test_conn();
        let input = "\
add task-a \"First task\"
add task-b \"Second task\"
note task-a <<MARKER
This is a
multiline note
for task-a
MARKER
block task-a task-b
";
        run_exec(&conn, input).unwrap();
        let notes = ops::list_notes(&conn, "task-a").unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].content, "This is a\nmultiline note\nfor task-a");
        let blockers = ops::get_blockers(&conn, "task-b").unwrap();
        assert_eq!(blockers, vec!["task-a"]);
    }

    #[test]
    fn exec_heredoc_preserves_indentation() {
        let conn = test_conn();
        let input = "\
add my-task \"A task\"
note my-task <<END
  indented line
    more indented
END
";
        run_exec(&conn, input).unwrap();
        let notes = ops::list_notes(&conn, "my-task").unwrap();
        assert_eq!(notes[0].content, "  indented line\n    more indented");
    }

    #[test]
    fn exec_heredoc_line_number_points_to_command() {
        let conn = test_conn();
        // The heredoc command is on line 1, but the error should reference line 1
        let input = "\
note nonexistent <<END
some content
END
";
        let result = run_exec(&conn, input);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("line 1"), "expected line 1 in error: {msg}");
    }

    #[test]
    fn exec_heredoc_with_flag_arg() {
        let conn = test_conn();
        let input = "\
add my-task \"A task\" -n <<NOTE
Initial note
with multiple lines
NOTE
";
        run_exec(&conn, input).unwrap();
        let notes = ops::list_notes(&conn, "my-task").unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].content, "Initial note\nwith multiple lines");
    }

    #[test]
    fn add_json_outputs_task_detail() {
        let conn = test_conn();
        dispatch(
            &conn,
            Command::Add {
                name: "json-task".into(),
                parent: None,
                desc: "A test task".into(),
                note: Some("initial note".into()),
                claim: None,
                paused: false,
                json: true,
            },
        )
        .unwrap();
        // Verify the task was created correctly (JSON output goes to stdout
        // which we can't capture in-process, but we verify the underlying
        // data is correct)
        let task = ops::get_task(&conn, "json-task").unwrap();
        assert_eq!(task.description, "A test task");
        assert_eq!(task.status, "open");
        let notes = ops::list_notes(&conn, "json-task").unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].content, "initial note");
    }

    #[test]
    fn exec_add_with_json_flag() {
        let conn = test_conn();
        let input = "add my-json-task \"A task\" --json\n";
        run_exec(&conn, input).unwrap();
        let task = ops::get_task(&conn, "my-json-task").unwrap();
        assert_eq!(task.description, "A task");
    }

    #[test]
    fn exec_heredoc_rejects_multiple_per_line() {
        let conn = test_conn();
        let input = "\
add my-task <<DESC -n <<NOTE
description
DESC
";
        let result = run_exec(&conn, input);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("only one heredoc"),
            "expected multiple heredoc error: {msg}"
        );
    }

    #[test]
    fn note_without_content_errors_when_stdin_is_terminal() {
        let result = check_note_content(None, true);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("no note content"),
            "expected error about missing note content, got: {msg}"
        );
    }

    #[test]
    fn note_with_content_arg_ignores_terminal_check() {
        let result = check_note_content(Some("hello".to_string()), true);
        assert_eq!(result.unwrap(), Some("hello".to_string()));
    }

    #[test]
    fn note_without_content_allows_stdin_pipe() {
        // Returns None to signal "read from stdin"
        let result = check_note_content(None, false);
        assert_eq!(result.unwrap(), None);
    }

    // --- has_balanced_quotes unit tests ---

    #[test]
    fn balanced_quotes_simple() {
        assert!(has_balanced_quotes(r#"hello "world""#));
    }

    #[test]
    fn balanced_quotes_no_quotes() {
        assert!(has_balanced_quotes("no quotes here"));
    }

    #[test]
    fn unbalanced_quotes_open() {
        assert!(!has_balanced_quotes(r#"note task "oops"#));
    }

    #[test]
    fn balanced_quotes_escaped_quote() {
        assert!(has_balanced_quotes(r#""say \"hello\"""#));
    }

    #[test]
    fn unbalanced_quotes_escaped_closing() {
        // The \" escapes the quote, so the string is still open
        assert!(!has_balanced_quotes(r#""ends with \""#));
    }

    #[test]
    fn balanced_quotes_escaped_backslash_before_quote() {
        // \\\\" means: escaped backslash + closing quote
        assert!(has_balanced_quotes(r#""trailing backslash\\""#));
    }

    // --- tokenize_exec_line unit tests ---

    #[test]
    fn tokenize_simple_words() {
        let tokens = tokenize_exec_line("add my-task description").unwrap();
        assert_eq!(tokens, vec!["add", "my-task", "description"]);
    }

    #[test]
    fn tokenize_double_quoted_string() {
        let tokens = tokenize_exec_line(r#"note task "hello world""#).unwrap();
        assert_eq!(tokens, vec!["note", "task", "hello world"]);
    }

    #[test]
    fn tokenize_apostrophe_unquoted() {
        let tokens = tokenize_exec_line("note task it's-fine").unwrap();
        assert_eq!(tokens, vec!["note", "task", "it's-fine"]);
    }

    #[test]
    fn tokenize_escaped_quotes_inside_quotes() {
        let tokens = tokenize_exec_line(r#"note task "say \"hello\"""#).unwrap();
        assert_eq!(tokens, vec!["note", "task", r#"say "hello""#]);
    }

    #[test]
    fn tokenize_escaped_backslash_inside_quotes() {
        let tokens = tokenize_exec_line(r#"note task "path\\to""#).unwrap();
        assert_eq!(tokens, vec!["note", "task", r"path\to"]);
    }

    #[test]
    fn tokenize_backslash_outside_quotes_is_literal() {
        let tokens = tokenize_exec_line(r"note task path\to\file").unwrap();
        assert_eq!(tokens, vec!["note", "task", r"path\to\file"]);
    }

    #[test]
    fn tokenize_empty_quoted_string() {
        let tokens = tokenize_exec_line(r#"add task """#).unwrap();
        assert_eq!(tokens, vec!["add", "task", ""]);
    }

    #[test]
    fn tokenize_adjacent_quoted_and_unquoted() {
        let tokens = tokenize_exec_line(r#""foo"bar"#).unwrap();
        assert_eq!(tokens, vec!["foobar"]);
    }

    #[test]
    fn tokenize_unterminated_quote_errors() {
        assert!(tokenize_exec_line(r#"note task "oops"#).is_err());
    }

    #[test]
    fn tokenize_empty_input() {
        assert!(tokenize_exec_line("").unwrap().is_empty());
    }

    #[test]
    fn tokenize_only_whitespace() {
        assert!(tokenize_exec_line("   \t  ").unwrap().is_empty());
    }

    #[test]
    fn tokenize_heredoc_marker_preserved() {
        let tokens = tokenize_exec_line("note task <<EOF").unwrap();
        assert_eq!(tokens, vec!["note", "task", "<<EOF"]);
    }

    // --- exec integration tests for quoting ---

    #[test]
    fn exec_heredoc_delimiter_requires_exact_line_match() {
        let conn = test_conn();
        // Indented "END" in the body should NOT close the heredoc
        let input = "\
add my-task \"A task\"
note my-task <<END
before
  END
after
END
";
        run_exec(&conn, input).unwrap();
        let notes = ops::list_notes(&conn, "my-task").unwrap();
        assert_eq!(notes[0].content, "before\n  END\nafter");
    }

    #[test]
    fn exec_note_with_apostrophe() {
        let conn = test_conn();
        let input = "\
add my-task \"A task\"
note my-task \"Here's the issue\"
";
        run_exec(&conn, input).unwrap();
        let notes = ops::list_notes(&conn, "my-task").unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].content, "Here's the issue");
    }

    #[test]
    fn exec_note_with_unquoted_apostrophe() {
        // Apostrophe in unquoted context should not start a single-quoted string
        let conn = test_conn();
        let input = "\
add my-task \"A task\"
note my-task Here's-a-problem
";
        run_exec(&conn, input).unwrap();
        let notes = ops::list_notes(&conn, "my-task").unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].content, "Here's-a-problem");
    }

    #[test]
    fn exec_add_note_flag_with_apostrophe() {
        let conn = test_conn();
        let input = "\
add my-task \"A task\" -n \"It's working\"
";
        run_exec(&conn, input).unwrap();
        let notes = ops::list_notes(&conn, "my-task").unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].content, "It's working");
    }

    #[test]
    fn exec_note_with_nested_double_quotes() {
        // Double quotes inside double-quoted string should be escapable
        let conn = test_conn();
        let input = "\
add my-task \"A task\"
note my-task \"Used the \\\"foo\\\" method\"
";
        run_exec(&conn, input).unwrap();
        let notes = ops::list_notes(&conn, "my-task").unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].content, "Used the \"foo\" method");
    }

    #[test]
    fn exec_note_with_single_quotes_in_double_quotes() {
        // Single quotes inside double-quoted strings should be literal
        let conn = test_conn();
        let input = "\
add my-task \"A task\"
note my-task \"It's got 'single quotes' inside\"
";
        run_exec(&conn, input).unwrap();
        let notes = ops::list_notes(&conn, "my-task").unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].content, "It's got 'single quotes' inside");
    }

    // --- multiline quoted string tests ---

    #[test]
    fn exec_note_with_newline_in_double_quotes() {
        let conn = test_conn();
        let input = "add my-task \"A task\"\nnote my-task \"line one\nline two\"\n";
        run_exec(&conn, input).unwrap();
        let notes = ops::list_notes(&conn, "my-task").unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].content, "line one\nline two");
    }

    #[test]
    fn exec_add_with_multiline_note_flag() {
        let conn = test_conn();
        let input = "add my-task \"A task\" -n \"first\nsecond\nthird\"\n";
        run_exec(&conn, input).unwrap();
        let notes = ops::list_notes(&conn, "my-task").unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].content, "first\nsecond\nthird");
    }

    #[test]
    fn exec_multiline_quote_with_heredoc_on_next_command() {
        // A multiline quoted string on one command shouldn't interfere with
        // a heredoc on a subsequent command.
        let conn = test_conn();
        let input = "\
add my-task \"A task\" -n \"line one\nline two\"
note my-task <<EOF
heredoc body
EOF
";
        run_exec(&conn, input).unwrap();
        let notes = ops::list_notes(&conn, "my-task").unwrap();
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0].content, "line one\nline two");
        assert_eq!(notes[1].content, "heredoc body");
    }

    #[test]
    fn exec_unterminated_multiline_quote_errors() {
        let conn = test_conn();
        // Quote opened but never closed across all remaining lines
        let input = "add my-task \"A task\"\nnote my-task \"oops\n";
        assert!(run_exec(&conn, input).is_err());
    }
}
