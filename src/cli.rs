use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "tager", about = "Task tracker for AI agents")]
pub struct Cli {
    /// Path to the SQLite database [default: ~/.tager/tager.db]
    #[arg(long, env = "TAGER_DB", global = true)]
    pub db: Option<String>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Create database and tables (idempotent)
    Init,

    /// Add a task
    Add {
        /// Task name (alphanumeric, hyphens, underscores)
        name: String,
        /// Parent task name
        #[arg(short, long)]
        parent: Option<String>,
        /// Task description
        #[arg(short, long, default_value = "")]
        desc: String,
        /// Task status (active, idle, done)
        #[arg(short, long, default_value = "active")]
        status: String,
    },

    /// Edit a task
    Edit {
        /// Task name to edit
        name: String,
        /// New description
        #[arg(short, long)]
        desc: Option<String>,
        /// New status (active, idle, done)
        #[arg(short, long)]
        status: Option<String>,
        /// New parent task name
        #[arg(short, long)]
        parent: Option<String>,
        /// Rename the task
        #[arg(short = 'r', long)]
        rename: Option<String>,
    },

    /// Remove a task
    Rm {
        /// Task name to remove
        name: String,
        /// Remove children recursively
        #[arg(long)]
        recursive: bool,
    },

    /// Show task details
    Show {
        /// Task name
        name: String,
    },

    /// List tasks
    List {
        /// Display as tree
        #[arg(long)]
        tree: bool,
        /// Filter by status
        #[arg(long)]
        status: Option<String>,
        /// Show all tasks including done
        #[arg(long)]
        all: bool,
        /// Root task for subtree
        #[arg(long)]
        root: Option<String>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Add a note to a task
    Note {
        /// Task name
        name: String,
        /// Note content (omit to read from stdin)
        content: Option<String>,
        /// Read content from stdin
        #[arg(long)]
        stdin: bool,
    },

    /// List notes for a task
    Notes {
        /// Task name
        name: String,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Mark a task as blocking another
    Block {
        /// The blocking task
        blocker: String,
        /// The blocked task
        blocked: String,
    },

    /// Remove a blocking relationship
    Unblock {
        /// The blocking task
        blocker: String,
        /// The blocked task
        blocked: String,
    },

    /// Launch interactive TUI
    Tree {
        /// Root task for subtree
        #[arg(long)]
        root: Option<String>,
        /// Poll interval in milliseconds
        #[arg(long, default_value = "1000")]
        poll_interval: u64,
    },
}
