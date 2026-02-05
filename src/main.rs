mod cli;
mod db;
mod model;
mod ops;
mod output;
mod tui;
mod validate;

use std::io::Read as _;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::Parser;

use cli::{Cli, Command};
use model::Status;

fn default_db_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME environment variable not set")?;
    Ok(PathBuf::from(home).join(".tager").join("tager.db"))
}

fn resolve_db_path(cli_db: Option<String>) -> Result<String> {
    match cli_db {
        Some(p) => Ok(p),
        None => {
            let path = default_db_path()?;
            Ok(path.to_str().context("default DB path is not valid UTF-8")?.to_string())
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
        Command::Init => {
            let conn = db::open(&db_path)?;
            db::init(&conn)?;
            eprintln!("Database initialized at {db_path}");
        }

        Command::Add {
            name,
            parent,
            desc,
            status,
        } => {
            let conn = db::open(&db_path)?;
            let status = Status::parse(&status)?;
            ops::add_task(&conn, &name, parent.as_deref(), &desc, status)?;
            eprintln!("Added task '{name}'");
        }

        Command::Edit {
            name,
            desc,
            status,
            parent,
            rename,
        } => {
            let conn = db::open(&db_path)?;
            let status = status.map(|s| Status::parse(&s)).transpose()?;
            // If --parent is given, wrap it in Some; if not given, parent is None (no change)
            let parent_opt = parent.map(|p| {
                if p.is_empty() {
                    None // --parent "" means clear parent
                } else {
                    Some(p)
                }
            });
            let parent_ref = parent_opt
                .as_ref()
                .map(|opt| opt.as_deref());
            ops::edit_task(
                &conn,
                &name,
                desc.as_deref(),
                status,
                parent_ref,
                rename.as_deref(),
            )?;
            if let Some(ref new_name) = rename {
                eprintln!("Renamed task '{name}' to '{new_name}'");
            } else {
                eprintln!("Updated task '{name}'");
            }
        }

        Command::Rm { name, recursive } => {
            let conn = db::open(&db_path)?;
            ops::remove_task(&conn, &name, recursive)?;
            eprintln!("Removed task '{name}'");
        }

        Command::Show { name } => {
            let conn = db::open(&db_path)?;
            let task = ops::get_task(&conn, &name)?;
            let notes = ops::list_notes(&conn, &name)?;
            let blockers = ops::get_blockers(&conn, &name)?;
            let dependents = ops::get_dependents(&conn, &name)?;
            print!("{}", output::format_task_detail(&task, &notes, &blockers, &dependents));
        }

        Command::List {
            tree,
            status,
            all,
            root,
            json,
        } => {
            let conn = db::open(&db_path)?;
            let status = status.map(|s| Status::parse(&s)).transpose()?;
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
            let conn = db::open(&db_path)?;
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
            let conn = db::open(&db_path)?;
            let notes = ops::list_notes(&conn, &name)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&notes)?);
            } else {
                print!("{}", output::format_notes(&notes));
            }
        }

        Command::Block { blocker, blocked } => {
            let conn = db::open(&db_path)?;
            ops::add_block(&conn, &blocker, &blocked)?;
            eprintln!("'{blocker}' now blocks '{blocked}'");
        }

        Command::Unblock { blocker, blocked } => {
            let conn = db::open(&db_path)?;
            ops::remove_block(&conn, &blocker, &blocked)?;
            eprintln!("'{blocker}' no longer blocks '{blocked}'");
        }

        Command::Tree {
            root,
            poll_interval,
        } => {
            let conn = db::open(&db_path)?;
            tui::run(&db_path, &conn, root.as_deref(), poll_interval)?;
        }
    }

    Ok(())
}
