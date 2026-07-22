//! Attribution & provenance (`docs/DESIGN.md` §4d): who edited which lines.
//!
//! Every attributed write ([`Fs::write_as`]) records an append-only [`EditOp`]
//! (the durable ground truth, linked to an actor/session/tool-call) and updates a
//! line-level authorship map for the file. [`Fs::blame`] then reports, per line
//! range, whether a **human** or **agent** wrote it — so a shared human+agent
//! workspace can always tell who did what.
//!
//! M6 scope: live working-tree blame (survives commits; reset by checkout/merge).
//! Persisting blame per blob-version in the object graph — so it survives
//! merge/rebase — is a noted follow-up.

use crate::content::ContentStore;
use crate::engine::Fs;
use crate::error::{AfsError, Result};
use crate::metadata::MetadataStore;
use crate::types::{FileKind, Ino};
use crate::util::now_secs;
use similar::{DiffOp, TextDiff};

/// Whether an actor is a person, an autonomous agent, or the system.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActorKind {
    Human,
    Agent,
    System,
}

impl ActorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ActorKind::Human => "human",
            ActorKind::Agent => "agent",
            ActorKind::System => "system",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "human" => Some(ActorKind::Human),
            "agent" => Some(ActorKind::Agent),
            "system" => Some(ActorKind::System),
            _ => None,
        }
    }
}

/// Fields to register a new actor.
#[derive(Clone, Debug, Default)]
pub struct ActorInit {
    pub kind: Option<ActorKind>,
    pub display_name: String,
    pub auth_subject: Option<String>,
    pub agent_model: Option<String>,
    pub agent_vendor: Option<String>,
    /// The human/actor that launched this agent (provenance chain).
    pub controller_actor_id: Option<i64>,
}

impl ActorInit {
    pub fn human(display_name: impl Into<String>, auth_subject: Option<String>) -> Self {
        Self {
            kind: Some(ActorKind::Human),
            display_name: display_name.into(),
            auth_subject,
            ..Default::default()
        }
    }
    pub fn agent(
        display_name: impl Into<String>,
        model: impl Into<String>,
        controller: Option<i64>,
    ) -> Self {
        Self {
            kind: Some(ActorKind::Agent),
            display_name: display_name.into(),
            agent_model: Some(model.into()),
            controller_actor_id: controller,
            ..Default::default()
        }
    }
}

/// A registered actor.
#[derive(Clone, Debug)]
pub struct Actor {
    pub id: i64,
    pub kind: ActorKind,
    pub display_name: String,
    pub auth_subject: Option<String>,
    pub agent_model: Option<String>,
    pub agent_vendor: Option<String>,
    pub controller_actor_id: Option<i64>,
    pub created_at: i64,
}

/// A recorded tool-call audit entry (agentfs-style), optionally linked from edits.
#[derive(Clone, Debug, Default)]
pub struct ToolCallInit {
    pub session_id: Option<i64>,
    pub actor_id: Option<i64>,
    pub name: String,
    pub parameters: Option<String>,
    pub result: Option<String>,
    pub error: Option<String>,
    pub started_at: i64,
    pub completed_at: i64,
    pub duration_ms: i64,
}

/// Fields for an append-only edit-op log entry.
#[derive(Clone, Debug)]
pub struct EditOpInit {
    pub session_id: Option<i64>,
    pub actor_id: i64,
    pub tool_call_id: Option<i64>,
    pub ino: Ino,
    pub path: String,
    pub op: String,
    pub byte_start: i64,
    pub byte_len: i64,
    pub pre_hash: Option<String>,
    pub post_hash: Option<String>,
    pub ts: i64,
}

/// A stored edit-op log entry.
#[derive(Clone, Debug)]
pub struct EditOp {
    pub id: i64,
    pub session_id: Option<i64>,
    pub actor_id: i64,
    pub tool_call_id: Option<i64>,
    pub ino: Ino,
    pub path: String,
    pub op: String,
    pub byte_start: i64,
    pub byte_len: i64,
    pub pre_hash: Option<String>,
    pub post_hash: Option<String>,
    pub ts: i64,
}

/// The actor context for an attributed write.
#[derive(Clone, Copy, Debug)]
pub struct WriteCtx {
    pub actor: i64,
    pub session: Option<i64>,
    pub tool_call: Option<i64>,
}

impl WriteCtx {
    pub fn actor(actor: i64) -> Self {
        Self {
            actor,
            session: None,
            tool_call: None,
        }
    }
    pub fn session(actor: i64, session: i64) -> Self {
        Self {
            actor,
            session: Some(session),
            tool_call: None,
        }
    }
    fn sid(&self) -> i64 {
        self.session.unwrap_or(0)
    }
}

/// One coalesced authorship run over consecutive lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BlameRun {
    actor: i64,
    session: i64,
    lines: u32,
}

/// A file's line-authorship map, stored as `actor,session,lines;...`.
#[derive(Clone, Debug, Default)]
struct BlameMap {
    runs: Vec<BlameRun>,
}

impl BlameMap {
    fn from_per_line(authors: &[(i64, i64)]) -> Self {
        let mut runs: Vec<BlameRun> = Vec::new();
        for &(actor, session) in authors {
            match runs.last_mut() {
                Some(r) if r.actor == actor && r.session == session => r.lines += 1,
                _ => runs.push(BlameRun {
                    actor,
                    session,
                    lines: 1,
                }),
            }
        }
        BlameMap { runs }
    }

    fn per_line(&self) -> Vec<(i64, i64)> {
        let mut out = Vec::new();
        for r in &self.runs {
            for _ in 0..r.lines {
                out.push((r.actor, r.session));
            }
        }
        out
    }

    fn encode(&self) -> String {
        self.runs
            .iter()
            .map(|r| format!("{},{},{}", r.actor, r.session, r.lines))
            .collect::<Vec<_>>()
            .join(";")
    }

    fn decode(s: &str) -> BlameMap {
        let runs = s
            .split(';')
            .filter(|p| !p.is_empty())
            .filter_map(|p| {
                let mut it = p.split(',');
                Some(BlameRun {
                    actor: it.next()?.parse().ok()?,
                    session: it.next()?.parse().ok()?,
                    lines: it.next()?.parse().ok()?,
                })
            })
            .collect();
        BlameMap { runs }
    }
}

/// One blame result: an inclusive 1-based line range and its author.
#[derive(Clone, Debug)]
pub struct BlameRange {
    pub line_start: u32,
    pub line_end: u32,
    pub actor: Actor,
    pub session: Option<i64>,
}

fn is_text(bytes: &[u8]) -> bool {
    std::str::from_utf8(bytes).is_ok()
}

impl<M: MetadataStore, C: ContentStore> Fs<M, C> {
    // --- registry ---------------------------------------------------------

    pub async fn create_actor(&self, init: ActorInit) -> Result<i64> {
        self.meta.create_actor(init).await
    }

    pub async fn get_actor(&self, id: i64) -> Result<Option<Actor>> {
        self.meta.get_actor(id).await
    }

    /// Register a new agent actor whose controller is `controller`.
    pub async fn create_agent(
        &self,
        name: &str,
        model: &str,
        controller: Option<i64>,
    ) -> Result<i64> {
        self.create_actor(ActorInit::agent(name, model, controller))
            .await
    }

    /// Register a new human actor.
    pub async fn create_human(&self, name: &str, auth_subject: Option<&str>) -> Result<i64> {
        self.create_actor(ActorInit::human(name, auth_subject.map(|s| s.to_string())))
            .await
    }

    pub async fn create_session(&self, actor_id: i64, client: Option<&str>) -> Result<i64> {
        self.meta.create_session(actor_id, client, now_secs()).await
    }

    pub async fn record_tool_call(&self, tc: ToolCallInit) -> Result<i64> {
        self.meta.record_tool_call(tc).await
    }

    // --- attributed write -------------------------------------------------

    /// Write `data` to `path`, attributing the change to `ctx`'s actor and
    /// updating per-line authorship. Creates the file if needed.
    pub async fn write_as(&self, ctx: WriteCtx, path: &str, data: &[u8]) -> Result<()> {
        let (parent, name) = self.resolve_parent(path).await?;
        self.ensure_dir(parent).await?;
        let ino = self.ensure_file(parent, name, path).await?;

        let inode = self
            .meta
            .get_inode(ino)
            .await?
            .ok_or_else(|| AfsError::NotFound(path.to_string()))?;
        let pre_hash = inode.content;
        let old_bytes = match pre_hash {
            Some(h) => self.read_body(&h).await?,
            None => Vec::new(),
        };

        // Update line authorship.
        let blame = if is_text(&old_bytes) && is_text(data) {
            let old_authors = match self.meta.get_line_blame(ino).await? {
                Some(s) => BlameMap::decode(&s).per_line(),
                None => Vec::new(),
            };
            let new_authors = diff_authors(&old_bytes, data, &old_authors, (ctx.actor, ctx.sid()));
            BlameMap::from_per_line(&new_authors)
        } else {
            // Binary: file-level attribution (single unit).
            BlameMap::from_per_line(&[(ctx.actor, ctx.sid())])
        };
        self.meta.set_line_blame(ino, &blame.encode()).await?;

        // Store content.
        let (mhash, size) = self.store_body(data).await?;
        self.meta.set_content(ino, mhash, size).await?;

        // Durable op-log entry.
        self.meta
            .append_edit_op(EditOpInit {
                session_id: ctx.session,
                actor_id: ctx.actor,
                tool_call_id: ctx.tool_call,
                ino,
                path: path.to_string(),
                op: "write".to_string(),
                byte_start: 0,
                byte_len: data.len() as i64,
                pre_hash: pre_hash.map(|h| h.to_hex()),
                post_hash: mhash.map(|h| h.to_hex()),
                ts: now_secs(),
            })
            .await?;
        Ok(())
    }

    // --- queries ----------------------------------------------------------

    /// Per-line-range authorship for `path`, distinguishing human vs agent.
    pub async fn blame(&self, path: &str) -> Result<Vec<BlameRange>> {
        let ino = self.resolve(path).await?;
        let map = match self.meta.get_line_blame(ino).await? {
            Some(s) => BlameMap::decode(&s),
            None => return Ok(Vec::new()),
        };
        let mut out = Vec::new();
        let mut line: u32 = 1;
        for r in &map.runs {
            let actor = self
                .meta
                .get_actor(r.actor)
                .await?
                .ok_or_else(|| AfsError::NotFound(format!("actor {}", r.actor)))?;
            out.push(BlameRange {
                line_start: line,
                line_end: line + r.lines - 1,
                actor,
                session: (r.session != 0).then_some(r.session),
            });
            line += r.lines;
        }
        Ok(out)
    }

    /// The edit-op log for `actor` (optionally narrowed to one `session`).
    pub async fn edit_ops(&self, actor_id: i64, session_id: Option<i64>) -> Result<Vec<EditOp>> {
        self.meta.list_edit_ops(actor_id, session_id).await
    }

    /// Revert every line an actor wrote in a session, across all files they
    /// touched. Returns the number of files changed. The removed lines are
    /// dropped; remaining lines keep their authorship.
    pub async fn revert_session(&self, actor_id: i64, session_id: i64) -> Result<usize> {
        // Distinct files this actor touched in this session (from the op-log).
        let ops = self.meta.list_edit_ops(actor_id, Some(session_id)).await?;
        let mut paths: Vec<(Ino, String)> = Vec::new();
        for op in ops {
            if !paths.iter().any(|(i, _)| *i == op.ino) {
                paths.push((op.ino, op.path));
            }
        }

        let mut changed = 0;
        for (ino, path) in paths {
            let Some(map_s) = self.meta.get_line_blame(ino).await? else {
                continue;
            };
            let authors = BlameMap::decode(&map_s).per_line();
            let Ok(current) =
                std::str::from_utf8(&self.read_current(ino).await?).map(str::to_owned)
            else {
                continue; // binary: skip line-revert
            };
            let lines: Vec<&str> = split_lines(&current);
            if lines.len() != authors.len() {
                continue; // out of sync; skip conservatively
            }

            let mut kept_body = String::new();
            let mut kept_authors: Vec<(i64, i64)> = Vec::new();
            let mut removed = false;
            for (line, &(a, s)) in lines.iter().zip(authors.iter()) {
                if a == actor_id && s == session_id {
                    removed = true; // drop this line
                } else {
                    kept_body.push_str(line);
                    kept_authors.push((a, s));
                }
            }
            if !removed {
                continue;
            }

            let (mhash, size) = self.store_body(kept_body.as_bytes()).await?;
            self.meta.set_content(ino, mhash, size).await?;
            self.meta
                .set_line_blame(ino, &BlameMap::from_per_line(&kept_authors).encode())
                .await?;
            self.meta
                .append_edit_op(EditOpInit {
                    session_id: None,
                    actor_id,
                    tool_call_id: None,
                    ino,
                    path,
                    op: "revert".to_string(),
                    byte_start: 0,
                    byte_len: size as i64,
                    pre_hash: None,
                    post_hash: mhash.map(|h| h.to_hex()),
                    ts: now_secs(),
                })
                .await?;
            changed += 1;
        }
        Ok(changed)
    }

    async fn read_current(&self, ino: Ino) -> Result<Vec<u8>> {
        let inode = self
            .meta
            .get_inode(ino)
            .await?
            .ok_or_else(|| AfsError::NotFound(format!("ino {ino}")))?;
        if inode.kind != FileKind::File {
            return Ok(Vec::new());
        }
        match inode.content {
            Some(h) => self.read_body(&h).await,
            None => Ok(Vec::new()),
        }
    }
}

/// Split text into lines the way `TextDiff::from_lines` tokenizes (keeping
/// trailing newlines), so line counts line up with the diff indices.
fn split_lines(s: &str) -> Vec<&str> {
    s.split_inclusive('\n').collect()
}

/// Compute per-new-line authorship: unchanged lines keep their prior author;
/// inserted/replaced lines are attributed to `new_author`.
fn diff_authors(
    old: &[u8],
    new: &[u8],
    old_authors: &[(i64, i64)],
    new_author: (i64, i64),
) -> Vec<(i64, i64)> {
    let old_s = std::str::from_utf8(old).unwrap_or("");
    let new_s = std::str::from_utf8(new).unwrap_or("");
    let diff = TextDiff::from_lines(old_s, new_s);
    let mut out: Vec<(i64, i64)> = Vec::new();
    for op in diff.ops() {
        match *op {
            DiffOp::Equal { old_index, len, .. } => {
                for k in 0..len {
                    out.push(
                        old_authors
                            .get(old_index + k)
                            .copied()
                            .unwrap_or(new_author),
                    );
                }
            }
            DiffOp::Insert { new_len, .. } | DiffOp::Replace { new_len, .. } => {
                for _ in 0..new_len {
                    out.push(new_author);
                }
            }
            DiffOp::Delete { .. } => {}
        }
    }
    out
}
