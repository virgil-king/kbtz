mod cli;
mod db;
mod model;
mod ops;
mod output;
mod tui;
mod validate;
mod watch;

use std::io::Read as _;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::Parser;
use rusqlite::Connection;

use cli::{Cli, Command};
use ops::StatusFilter;

fn default_db_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME environment variable not set")?;
    Ok(PathBuf::from(home).join(".tager").join("tager.db"))
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
        Command::Add {
            name,
            parent,
            desc,
            note,
            claim,
        } => {
            let conn = open_db(&db_path)?;
            ops::add_task(&conn, &name, parent.as_deref(), &desc, note.as_deref(), claim.as_deref())?;
            eprintln!("Added task '{name}'");
            if let Some(assignee) = &claim {
                eprintln!("Claimed '{name}' for '{assignee}'");
            }
        }

        Command::Claim { name, assignee } => {
            let conn = open_db(&db_path)?;
            ops::claim_task(&conn, &name, &assignee)?;
            eprintln!("Claimed '{name}' for '{assignee}'");
        }

        Command::ClaimNext { assignee, prefer } => {
            let conn = open_db(&db_path)?;
            match ops::claim_next_task(&conn, &assignee, prefer.as_deref())? {
                Some(name) => {
                    println!("{name}");
                    eprintln!("Claimed '{name}' for '{assignee}'");
                }
                None => {
                    eprintln!("No tasks available");
                    std::process::exit(1);
                }
            }
        }

        Command::Release { name, assignee } => {
            let conn = open_db(&db_path)?;
            ops::release_task(&conn, &name, &assignee)?;
            eprintln!("Released '{name}'");
        }

        Command::Done { name } => {
            let conn = open_db(&db_path)?;
            ops::mark_done(&conn, &name)?;
            eprintln!("Marked '{name}' as done");
        }

        Command::Reopen { name } => {
            let conn = open_db(&db_path)?;
            ops::reopen_task(&conn, &name)?;
            eprintln!("Reopened '{name}'");
        }

        Command::Reparent { name, parent } => {
            let conn = open_db(&db_path)?;
            ops::reparent_task(&conn, &name, parent.as_deref())?;
            match parent.as_deref() {
                Some(p) => eprintln!("Moved '{name}' under '{p}'"),
                None => eprintln!("Moved '{name}' to root level"),
            }
        }

        Command::Describe { name, desc } => {
            let conn = open_db(&db_path)?;
            ops::update_description(&conn, &name, &desc)?;
            eprintln!("Updated description for '{name}'");
        }

        Command::Rm { name, recursive } => {
            let conn = open_db(&db_path)?;
            ops::remove_task(&conn, &name, recursive)?;
            eprintln!("Removed task '{name}'");
        }

        Command::Show { name } => {
            let conn = open_db(&db_path)?;
            let task = ops::get_task(&conn, &name)?;
            let notes = ops::list_notes(&conn, &name)?;
            let blockers = ops::get_blockers(&conn, &name)?;
            let dependents = ops::get_dependents(&conn, &name)?;
            print!(
                "{}",
                output::format_task_detail(&task, &notes, &blockers, &dependents)
            );
        }

        Command::List {
            tree,
            status,
            all,
            root,
            json,
        } => {
            let conn = open_db(&db_path)?;
            let status = status.map(|s| StatusFilter::parse(&s)).transpose()?;
            let tasks = ops::list_tasks(&conn, status, all, root.as_deref())?;
            if json {
                println!("{}", serde_json::to_string_pretty(&tasks)?);
            } else if tree {
                print!("{}", output::format_task_tree(&tasks));
            } else {
                print!("{}", output::format_task_list(&tasks));
            }
        }

        Command::Note {
            name,
            content,
            stdin,
        } => {
            let conn = open_db(&db_path)?;
            let content = match content {
                Some(c) if !stdin => c,
                _ => {
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

        Command::Notes { name, json } => {
            let conn = open_db(&db_path)?;
            let notes = ops::list_notes(&conn, &name)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&notes)?);
            } else {
                print!("{}", output::format_notes(&notes));
            }
        }

        Command::Block { blocker, blocked } => {
            let conn = open_db(&db_path)?;
            ops::add_block(&conn, &blocker, &blocked)?;
            eprintln!("'{blocker}' now blocks '{blocked}'");
        }

        Command::Unblock { blocker, blocked } => {
            let conn = open_db(&db_path)?;
            ops::remove_block(&conn, &blocker, &blocked)?;
            eprintln!("'{blocker}' no longer blocks '{blocked}'");
        }

        Command::Tree {
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
    }

    Ok(())
}
