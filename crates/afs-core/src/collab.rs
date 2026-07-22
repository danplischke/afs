//! Live collaboration: a change feed + presence for a shared human+agent
//! workspace (`docs/DESIGN.md` §7 / roadmap M8).
//!
//! When several actors — humans and agents — share one workspace, each needs to
//! see what the others are doing *as it happens*: who touched which file, who
//! committed, who is currently active and where. This module records an
//! append-only **event feed** (a monotonic `seq` cursor other writers tail) and
//! **presence** (per-session heartbeat with a current path). On Postgres, every
//! appended event also fires `LISTEN/NOTIFY` so consumers can be pushed to
//! instead of polling; SQLite consumers poll the feed by cursor.
//!
//! Events are emitted at the workspace API boundary (see `afs-sdk`), so internal
//! engine operations — materializing a checkout, importing history — don't flood
//! the feed; only user/agent-initiated actions do.

use crate::attribution::ActorKind;
use crate::content::ContentStore;
use crate::engine::Fs;
use crate::error::Result;
use crate::metadata::MetadataStore;
use crate::util::now_secs;

/// The channel Postgres backends `NOTIFY` on when an event is appended.
pub const EVENT_CHANNEL: &str = "afs_events";

/// A change to record in the feed.
#[derive(Clone, Debug)]
pub struct EventInit {
    pub actor_id: Option<i64>,
    pub session_id: Option<i64>,
    /// A short verb: `write`, `mkdir`, `remove`, `rename`, `symlink`, `commit`,
    /// `lock`, `unlock`.
    pub kind: String,
    pub path: String,
    /// Optional extra context (rename target, commit message, lock owner, …).
    pub detail: Option<String>,
}

/// A recorded feed event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    pub seq: i64,
    pub actor_id: Option<i64>,
    pub session_id: Option<i64>,
    pub kind: String,
    pub path: String,
    pub detail: Option<String>,
    pub ts: i64,
}

/// A currently-active session: who it is and where they are.
#[derive(Clone, Debug)]
pub struct Presence {
    pub session_id: i64,
    pub actor_id: i64,
    pub display_name: String,
    pub kind: ActorKind,
    pub path: Option<String>,
    pub last_seen: i64,
}

/// Default presence window: sessions seen within this many seconds are "active".
pub const PRESENCE_WINDOW_SECS: i64 = 60;

impl<M: MetadataStore, C: ContentStore> Fs<M, C> {
    /// Append an event to the change feed, returning its `seq` cursor.
    pub async fn record_event(&self, ev: EventInit) -> Result<i64> {
        self.meta.append_event(ev, now_secs()).await
    }

    /// Events strictly after `after_seq`, oldest first (cursor-based tailing).
    pub async fn events_since(&self, after_seq: i64, limit: i64) -> Result<Vec<Event>> {
        self.meta.events_since(after_seq, limit).await
    }

    /// Heartbeat a session's presence, optionally noting the path it is on.
    pub async fn touch_presence(
        &self,
        session_id: i64,
        actor_id: i64,
        path: Option<&str>,
    ) -> Result<()> {
        self.meta
            .touch_presence(session_id, actor_id, path, now_secs())
            .await
    }

    /// Sessions active within `window_secs`, most recently seen first.
    pub async fn presence(&self, window_secs: i64) -> Result<Vec<Presence>> {
        self.meta.active_presence(now_secs() - window_secs).await
    }
}
