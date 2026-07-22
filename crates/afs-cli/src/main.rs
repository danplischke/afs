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
            let data = match from {
                Some(p) => std::fs::read(p)?,
                None => {
                    let mut buf = Vec::new();
                    std::io::stdin().read_to_end(&mut buf)?;
                    buf
                }
            };
            // Convenience: ensure the parent directory exists before writing.
            if let Some(parent) = path.rsplit_once('/').map(|(p, _)| p) {
                if !parent.is_empty() {
                    ws.mkdir_p(parent).await?;
                }
            }
            ws.write(&path, &data).await?;
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
    }
    Ok(())
}
