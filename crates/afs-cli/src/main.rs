//! afs — a minimal CLI over an afs workspace (M0).
//!
//! Enough to exercise the engine from a shell:
//!
//! ```text
//! afs --workspace ./ws init
//! echo hello | afs --workspace ./ws write /notes/a.txt
//! afs --workspace ./ws ls /notes
//! afs --workspace ./ws read /notes/a.txt
//! ```

use afs_sdk::{MergeOutcome, Workspace, WriteCtx};
use anyhow::Result;
use clap::{Parser, Subcommand};
use std::io::{Read, Write};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "afs",
    version,
    about = "agent-and-human filesystem (M0 skeleton)"
)]
struct Cli {
    /// Workspace directory; holds `meta.db` and `cas/`.
    #[arg(long, default_value = ".afs")]
    workspace: PathBuf,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Initialize the workspace.
    Init,
    /// Create a directory and any missing parents.
    Mkdir { path: String },
    /// Write a file's contents from `--from <file>` or stdin.
    Write {
        path: String,
        /// Read data from this file instead of stdin.
        #[arg(long)]
        from: Option<PathBuf>,
        /// Attribute the write to this actor id (records blame + an edit-op).
        #[arg(long)]
        actor: Option<i64>,
    },
    /// Print a file's contents to stdout.
    Read { path: String },
    /// List a directory.
    Ls {
        #[arg(default_value = "/")]
        path: String,
    },
    /// Show inode metadata for a path.
    Stat { path: String },
    /// Remove a file or empty directory.
    Rm { path: String },
    /// Move/rename a path.
    Mv { from: String, to: String },
    /// Snapshot the working tree into a commit.
    Commit {
        #[arg(short, long)]
        message: String,
        #[arg(long, default_value = "afs")]
        author: String,
    },
    /// Show commit history (HEAD, first-parent).
    Log,
    /// Show working-tree changes relative to HEAD.
    Status,
    /// Create a branch at HEAD, or list branches when no name is given.
    Branch { name: Option<String> },
    /// Switch the working tree to a branch.
    Checkout { branch: String },
    /// Merge a branch into the current branch.
    Merge {
        branch: String,
        #[arg(long, default_value = "afs")]
        author: String,
        #[arg(short, long)]
        message: Option<String>,
    },
    /// List unresolved merge conflicts.
    Conflicts,
    /// Acquire an exclusive lock on a path.
    Lock {
        path: String,
        #[arg(long, default_value = "cli")]
        owner: String,
    },
    /// Release a lock on a path.
    Unlock {
        path: String,
        #[arg(long, default_value = "cli")]
        owner: String,
    },
    /// List held locks.
    Locks,
    /// Register an actor (human by default; `--agent` for an agent).
    Actor {
        name: String,
        #[arg(long)]
        agent: bool,
        #[arg(long, default_value = "unknown")]
        model: String,
        /// The human actor id that launched this agent.
        #[arg(long)]
        controller: Option<i64>,
    },
    /// Show per-line authorship (blame) for a file.
    Blame { path: String },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    std::fs::create_dir_all(&cli.workspace)?;
    let db = cli.workspace.join("meta.db");
    let cas = cli.workspace.join("cas");
    let ws = Workspace::open_local(&db, &cas).await?;

    match cli.cmd {
        Cmd::Init => {
            println!("initialized afs workspace at {}", cli.workspace.display());
        }
        Cmd::Mkdir { path } => {
            ws.mkdir_p(&path).await?;
        }
        Cmd::Write { path, from, actor } => {
            // Convenience: ensure the parent directory exists before writing.
            if let Some(parent) = path
                .rsplit_once('/')
                .map(|(p, _)| p)
                .filter(|p| !p.is_empty())
            {
                ws.mkdir_p(parent).await?;
            }
            match (from, actor) {
                // Unattributed streaming from a file (large files stay off-heap).
                (Some(p), None) => {
                    let file = std::fs::File::open(p)?;
                    ws.write_reader(&path, file).await?;
                }
                // Attributed write: read into memory, record blame + an edit-op.
                (from, Some(actor)) => {
                    let data = match from {
                        Some(p) => std::fs::read(p)?,
                        None => {
                            let mut buf = Vec::new();
                            std::io::stdin().read_to_end(&mut buf)?;
                            buf
                        }
                    };
                    let session = ws.create_session(actor, Some("cli")).await?;
                    ws.write_as(WriteCtx::session(actor, session), &path, &data)
                        .await?;
                }
                (None, None) => {
                    let mut buf = Vec::new();
                    std::io::stdin().read_to_end(&mut buf)?;
                    ws.write(&path, &buf).await?;
                }
            }
        }
        Cmd::Read { path } => {
            let bytes = ws.read(&path).await?;
            std::io::stdout().write_all(&bytes)?;
        }
        Cmd::Ls { path } => {
            for e in ws.ls(&path).await? {
                println!("{}\t{}", e.kind.as_str(), e.name);
            }
        }
        Cmd::Stat { path } => {
            let i = ws.stat(&path).await?;
            println!(
                "ino={} kind={} mode={:o} nlink={} size={}",
                i.ino,
                i.kind.as_str(),
                i.mode,
                i.nlink,
                i.size
            );
        }
        Cmd::Rm { path } => {
            ws.remove(&path).await?;
        }
        Cmd::Mv { from, to } => {
            ws.rename(&from, &to).await?;
        }
        Cmd::Commit { message, author } => {
            let hash = ws.commit(&author, &message).await?;
            let branch = ws.current_branch().await?.unwrap_or_else(|| "?".into());
            println!("[{branch} {}] {message}", &hash.to_hex()[..12]);
        }
        Cmd::Log => {
            for ci in ws.log().await? {
                println!(
                    "{} {}  {}",
                    &ci.hash.to_hex()[..12],
                    ci.commit.author,
                    ci.commit.message
                );
            }
        }
        Cmd::Status => {
            let changes = ws.status().await?;
            if changes.is_empty() {
                println!("clean (working tree matches HEAD)");
            }
            for d in changes {
                println!("{} {}", d.status.sigil(), d.path);
            }
        }
        Cmd::Branch { name } => match name {
            Some(name) => {
                ws.create_branch(&name).await?;
                println!("created branch {name}");
            }
            None => {
                let current = ws.current_branch().await?;
                for (name, hash) in ws.list_branches().await? {
                    let marker = if current.as_deref() == Some(&name) {
                        "* "
                    } else {
                        "  "
                    };
                    println!("{marker}{name}\t{}", &hash.to_hex()[..12]);
                }
            }
        },
        Cmd::Checkout { branch } => {
            ws.checkout(&branch).await?;
            println!("switched to branch {branch}");
        }
        Cmd::Merge {
            branch,
            author,
            message,
        } => {
            let msg = message.unwrap_or_else(|| format!("merge {branch}"));
            match ws.merge_branch(&branch, &author, &msg).await? {
                MergeOutcome::AlreadyUpToDate => println!("already up to date"),
                MergeOutcome::FastForward(h) => {
                    println!("fast-forward to {}", &h.to_hex()[..12])
                }
                MergeOutcome::Merged(h) => println!("merged as {}", &h.to_hex()[..12]),
                MergeOutcome::Conflicts(cs) => {
                    println!(
                        "merge stopped with {} conflict(s); resolve then commit:",
                        cs.len()
                    );
                    for c in cs {
                        println!("  {} {}", c.kind, c.path);
                    }
                }
            }
        }
        Cmd::Conflicts => {
            for (path, kind) in ws.conflicts().await? {
                println!("{kind}\t{path}");
            }
        }
        Cmd::Lock { path, owner } => {
            if ws.lock(&path, &owner).await? {
                println!("locked {path}");
            } else {
                println!("already locked: {path}");
            }
        }
        Cmd::Unlock { path, owner } => {
            if ws.unlock(&path, &owner).await? {
                println!("unlocked {path}");
            } else {
                println!("not your lock: {path}");
            }
        }
        Cmd::Locks => {
            for (path, owner, _at) in ws.locks().await? {
                println!("{owner}\t{path}");
            }
        }
        Cmd::Actor {
            name,
            agent,
            model,
            controller,
        } => {
            let id = if agent {
                ws.create_agent(&name, &model, controller).await?
            } else {
                ws.create_human(&name, None).await?
            };
            println!("{id}");
        }
        Cmd::Blame { path } => {
            for r in ws.blame(&path).await? {
                let who = format!("{}:{}", r.actor.kind.as_str(), r.actor.display_name);
                if r.line_start == r.line_end {
                    println!("{:>4}       {who}", r.line_start);
                } else {
                    println!("{:>4}-{:<4}  {who}", r.line_start, r.line_end);
                }
            }
        }
    }
    Ok(())
}
