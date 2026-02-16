mod cli;

use std::io::Read as _;
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
            json,
        } => {
            let status = status.map(|s| StatusFilter::parse(&s)).transpose()?;
            let tasks = if let Some(ref parent) = children {
                ops::list_children(conn, parent, status, all)?
            } else {
                ops::list_tasks(conn, status, all, root.as_deref())?
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

/// Pre-process exec input to resolve heredoc syntax.
///
/// A token of the form `<<DELIMITER` causes subsequent lines to be accumulated
/// until a line matching `DELIMITER` (after trimming) is found. The accumulated
/// text replaces the `<<DELIMITER` token. Only one heredoc per command line is
/// supported.
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

        let mut tokens = shlex::split(line)
            .with_context(|| format!("line {lineno}: invalid shell quoting: {line}"))?;

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
                if lines[i].trim() == delimiter {
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
            let content = match content {
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
}
