use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "kbtz", about = "Task tracker for AI agents", version)]
pub struct Cli {
    /// Path to the SQLite database [default: ~/.kbtz/kbtz.db]
    #[arg(long, env = "KBTZ_DB", global = true)]
    pub db: Option<String>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Add a task
    Add {
        /// Task name (alphanumeric, hyphens, underscores; immutable after creation)
        name: String,
        /// Parent task name
        #[arg(short, long)]
        parent: Option<String>,
        /// Task description
        desc: String,
        /// Initial note
        #[arg(short, long)]
        note: Option<String>,
        /// Create already claimed by this assignee
        #[arg(short, long)]
        claim: Option<String>,
        /// Create in paused state (mutually exclusive with --claim)
        #[arg(long)]
        paused: bool,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Claim a task (set assignee)
    Claim {
        /// Task name
        name: String,
        /// Assignee ID (agent session ID)
        assignee: String,
    },

    /// Claim the best available task
    #[command(name = "claim-next")]
    ClaimNext {
        /// Assignee ID (agent session ID)
        assignee: String,
        /// Soft preference text for ranking (matched against name, description, and notes)
        #[arg(long)]
        prefer: Option<String>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Atomically transfer task ownership
    Steal {
        /// Task name
        name: String,
        /// New assignee ID
        assignee: String,
    },

    /// Release a task (clear assignee if it matches)
    Release {
        /// Task name
        name: String,
        /// Assignee ID to verify ownership
        assignee: String,
    },

    /// Forcibly clear a task's assignee (regardless of who holds it)
    #[command(name = "force-unassign")]
    ForceUnassign {
        /// Task name
        name: String,
    },

    /// Mark a task as done
    Done {
        /// Task name
        name: String,
    },

    /// Reopen a completed task
    Reopen {
        /// Task name
        name: String,
    },

    /// Pause a task (remove from active work and default listing)
    Pause {
        /// Task name
        name: String,
    },

    /// Unpause a paused task (return to open)
    Unpause {
        /// Task name
        name: String,
    },

    /// Change a task's parent
    Reparent {
        /// Task name
        name: String,
        /// New parent task name (omit to make root-level)
        #[arg(short, long)]
        parent: Option<String>,
    },

    /// Update a task's description
    Describe {
        /// Task name
        name: String,
        /// New description
        desc: String,
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
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// List tasks
    List {
        /// Display as tree
        #[arg(long)]
        tree: bool,
        /// Filter by status (open, active, paused, done)
        #[arg(long)]
        status: Option<String>,
        /// Show all tasks including done and paused
        #[arg(long)]
        all: bool,
        /// Root task for subtree
        #[arg(long)]
        root: Option<String>,
        /// Show only direct children of the given task (depth 1)
        #[arg(long, conflicts_with = "root")]
        children: Option<String>,
        /// Filter by assignee
        #[arg(long)]
        assignee: Option<String>,
        /// Show only blocked tasks
        #[arg(long, conflicts_with = "unblocked")]
        blocked: bool,
        /// Show only unblocked tasks
        #[arg(long)]
        unblocked: bool,
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
    #[command(name = "watch")]
    Watch {
        /// Root task for subtree
        #[arg(long)]
        root: Option<String>,
        /// Poll interval in milliseconds
        #[arg(long, default_value = "1000")]
        poll_interval: u64,
    },

    /// Wait for database changes (blocks until a change occurs)
    Wait,

    /// Full-text search across tasks and notes
    Search {
        /// Search query
        query: String,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Execute commands from stdin atomically (all-or-nothing transaction)
    ///
    /// Reads commands from stdin, one per line, and runs them in a single
    /// database transaction. If any command fails, all changes are rolled back.
    ///
    /// Lines are parsed with shell quoting rules (single/double quotes, backslash
    /// escapes). Blank lines and lines starting with # are ignored. Do not prefix
    /// commands with "kbtz":
    ///
    ///     add my-task "A new task"
    ///     block my-task other-task
    ///     note my-task "A note"
    ///
    /// Heredoc syntax is supported for multiline arguments (one per command):
    ///
    ///     note my-task <<EOF
    ///     Line one
    ///     Line two
    ///     EOF
    ///
    /// The "note" command normally reads from stdin when content is omitted, but
    /// this does not work inside exec. Always pass note content as an argument or
    /// use heredoc syntax.
    ///
    /// The exec, watch, and wait commands cannot be used inside exec.
    #[command(verbatim_doc_comment)]
    Exec,
}
