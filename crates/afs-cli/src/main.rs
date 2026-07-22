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

use afs_sdk::Workspace;
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
        Cmd::Write { path, from } => {
            // Convenience: ensure the parent directory exists before writing.
            if let Some(parent) = path
                .rsplit_once('/')
                .map(|(p, _)| p)
                .filter(|p| !p.is_empty())
            {
                ws.mkdir_p(parent).await?;
            }
            match from {
                // Stream from a file so large files never need full residency.
                Some(p) => {
                    let file = std::fs::File::open(p)?;
                    ws.write_reader(&path, file).await?;
                }
                None => {
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
    }
    Ok(())
}
