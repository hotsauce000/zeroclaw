//! Channel infrastructure: session backends, debouncing, stall watchdog, and
//! the plugin webhook ingress.
//! These are cross-cutting utilities used by multiple channel implementations.

pub mod acp_session_store;
pub mod debounce;
pub mod net_guard;
pub mod plugin_webhook;
pub mod session_backend;
pub mod session_queue;
pub mod session_sqlite;
pub mod session_store;
pub mod stall_watchdog;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::session_backend::SessionBackend;

pub fn effective_gateway_bind_socket_addr(host: &str, port: u16) -> SocketAddr {
    parse_gateway_bind_socket_addr(host, port)
        .unwrap_or_else(|_| fallback_gateway_bind_socket_addr(port))
}

pub fn parse_gateway_bind_socket_addr(
    host: &str,
    port: u16,
) -> Result<SocketAddr, std::net::AddrParseError> {
    format!("{host}:{port}").parse()
}

pub fn fallback_gateway_bind_socket_addr(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

/// Fallback for `gateway.idempotency_max_keys = 0`, shared by the gateway's
/// replay store and the plugin webhook ingress.
pub const IDEMPOTENCY_MAX_KEYS_DEFAULT: usize = 10_000;

/// A configured key bound where zero selects `fallback`; never below one.
#[must_use]
pub fn normalize_max_keys(configured: usize, fallback: usize) -> usize {
    if configured == 0 {
        fallback.max(1)
    } else {
        configured
    }
}

/// `gateway.idempotency_ttl_secs`, never below one second.
#[must_use]
pub fn effective_idempotency_ttl(ttl_secs: u64) -> Duration {
    Duration::from_secs(ttl_secs.max(1))
}

/// Idempotency keys already acted on, each kept for a TTL and bounded in
/// number. A full set evicts its oldest key to admit a new one.
#[derive(Debug)]
pub struct CommittedKeys {
    ttl: Duration,
    max_keys: usize,
    seen: HashMap<String, Instant>,
}

impl CommittedKeys {
    /// `max_keys` below one is raised to one.
    #[must_use]
    pub fn new(ttl: Duration, max_keys: usize) -> Self {
        Self {
            ttl,
            max_keys: max_keys.max(1),
            seen: HashMap::new(),
        }
    }

    /// Whether `key` was inserted less than the TTL before `now`. Keys that
    /// have reached the TTL are forgotten first.
    pub fn contains(&mut self, key: &str, now: Instant) -> bool {
        self.forget_expired(now);
        self.seen.contains_key(key)
    }

    /// Record `key` as seen at `now`, after forgetting expired keys and, if
    /// the set is still full, the oldest one.
    pub fn insert(&mut self, key: String, now: Instant) {
        self.forget_expired(now);
        if self.seen.len() >= self.max_keys {
            let oldest = self
                .seen
                .iter()
                .min_by_key(|(_, seen_at)| *seen_at)
                .map(|(oldest, _)| oldest.clone());
            if let Some(oldest) = oldest {
                self.seen.remove(&oldest);
            }
        }
        self.seen.insert(key, now);
    }

    /// Keys held, counting expired ones no call has forgotten yet.
    #[must_use]
    pub fn len(&self) -> usize {
        self.seen.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }

    fn forget_expired(&mut self, now: Instant) {
        self.seen
            .retain(|_, seen_at| now.duration_since(*seen_at) < self.ttl);
    }
}

pub fn make_session_backend(
    workspace_dir: &Path,
    backend: &str,
) -> std::io::Result<Arc<dyn SessionBackend>> {
    match backend {
        "jsonl" => {
            let store = session_store::SessionStore::new(workspace_dir)?;
            Ok(Arc::new(store))
        }
        "sqlite" => Ok(Arc::new(open_sqlite_with_jsonl_import(workspace_dir)?)),
        other => {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({"other": other})),
                "Unknown session_backend ''; falling back to sqlite. \
                 Valid values: 'sqlite' (default), 'jsonl'."
            );
            Ok(Arc::new(open_sqlite_with_jsonl_import(workspace_dir)?))
        }
    }
}

fn open_sqlite_with_jsonl_import(
    workspace_dir: &Path,
) -> std::io::Result<session_sqlite::SqliteSessionBackend> {
    let backend = session_sqlite::SqliteSessionBackend::new(workspace_dir)
        .map_err(|e| std::io::Error::other(format!("{e:#}")))?;
    match backend
        .migrate_from_jsonl(workspace_dir)
        .map_err(|e| std::io::Error::other(format!("{e:#}")))?
    {
        0 => {}
        n => ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
            &format!(
                "session_backend=sqlite: completed {n} legacy JSONL session migration \
             handoff(s) from {}/sessions to *.jsonl.migrated.",
                workspace_dir.display()
            )
        ),
    }
    Ok(backend)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use zeroclaw_api::model_provider::ChatMessage;

    fn user_msg(content: &str) -> ChatMessage {
        ChatMessage::user(content)
    }

    #[test]
    fn normalize_max_keys_uses_fallback_for_zero() {
        assert_eq!(normalize_max_keys(0, 10_000), 10_000);
        assert_eq!(normalize_max_keys(0, 0), 1);
    }

    #[test]
    fn normalize_max_keys_preserves_nonzero_values() {
        assert_eq!(normalize_max_keys(2_048, 10_000), 2_048);
        assert_eq!(normalize_max_keys(1, 10_000), 1);
    }

    #[test]
    fn effective_idempotency_limits_apply_the_documented_floors() {
        assert_eq!(effective_idempotency_ttl(0), Duration::from_secs(1));
        assert_eq!(effective_idempotency_ttl(7), Duration::from_secs(7));
        assert_eq!(
            normalize_max_keys(0, IDEMPOTENCY_MAX_KEYS_DEFAULT),
            IDEMPOTENCY_MAX_KEYS_DEFAULT
        );
    }

    #[test]
    fn committed_keys_contain_exactly_the_inserted_keys() {
        let now = Instant::now();
        let mut keys = CommittedKeys::new(Duration::from_secs(60), 8);
        assert!(keys.is_empty());
        assert!(!keys.contains("a", now));

        keys.insert("a".to_owned(), now);
        assert!(keys.contains("a", now));
        assert!(!keys.contains("b", now));
        assert_eq!(keys.len(), 1);
    }

    #[test]
    fn committed_keys_forget_a_key_once_its_ttl_elapses() {
        let ttl = Duration::from_secs(60);
        let start = Instant::now();
        let mut keys = CommittedKeys::new(ttl, 8);
        keys.insert("a".to_owned(), start);

        assert!(keys.contains("a", start + ttl - Duration::from_millis(1)));
        assert!(!keys.contains("a", start + ttl));
        assert!(keys.is_empty());
    }

    #[test]
    fn committed_keys_at_capacity_evict_the_oldest_key() {
        let start = Instant::now();
        let later = |millis| start + Duration::from_millis(millis);
        let mut keys = CommittedKeys::new(Duration::from_secs(60), 2);
        keys.insert("k2".to_owned(), later(2));
        keys.insert("k1".to_owned(), later(1));
        keys.insert("k3".to_owned(), later(3));

        assert_eq!(keys.len(), 2);
        assert!(!keys.contains("k1", later(3)));
        assert!(keys.contains("k2", later(3)));
        assert!(keys.contains("k3", later(3)));
    }

    #[test]
    fn committed_keys_forget_expired_keys_before_evicting_a_live_one() {
        let ttl = Duration::from_secs(60);
        let start = Instant::now();
        let mut keys = CommittedKeys::new(ttl, 2);
        keys.insert("expired".to_owned(), start);
        keys.insert("live".to_owned(), start + Duration::from_secs(30));
        keys.insert("new".to_owned(), start + ttl);

        assert_eq!(keys.len(), 2);
        assert!(keys.contains("live", start + ttl));
        assert!(keys.contains("new", start + ttl));
    }

    #[test]
    fn committed_keys_hold_at_least_one_key() {
        let now = Instant::now();
        let mut keys = CommittedKeys::new(Duration::from_secs(60), 0);
        keys.insert("a".to_owned(), now);
        assert!(keys.contains("a", now));

        keys.insert("b".to_owned(), now);
        assert_eq!(keys.len(), 1);
        assert!(keys.contains("b", now));
    }

    #[test]
    fn make_session_backend_jsonl_round_trips_through_session_store() {
        let tmp = TempDir::new().unwrap();
        let backend = make_session_backend(tmp.path(), "jsonl").unwrap();
        backend.append("k1", &user_msg("hello-jsonl")).unwrap();
        let loaded = backend.load("k1");
        assert_eq!(loaded.len(), 1);
        // The JSONL backend writes one file per session key.
        let jsonl = tmp.path().join("sessions").join("k1.jsonl");
        assert!(jsonl.exists(), "jsonl file must be written under sessions/");
    }

    #[test]
    fn make_session_backend_sqlite_round_trips_through_sqlite_db() {
        let tmp = TempDir::new().unwrap();
        let backend = make_session_backend(tmp.path(), "sqlite").unwrap();
        backend.append("k1", &user_msg("hello-sqlite")).unwrap();
        let loaded = backend.load("k1");
        assert_eq!(loaded.len(), 1);
        let db = tmp.path().join("sessions").join("sessions.db");
        assert!(db.exists(), "sqlite db must be written under sessions/");
        // The JSONL companion file must NOT have been created.
        assert!(!tmp.path().join("sessions").join("k1.jsonl").exists());
    }

    #[test]
    fn make_session_backend_can_reload_from_empty_sqlite_to_jsonl() {
        let tmp = TempDir::new().unwrap();
        let sqlite = make_session_backend(tmp.path(), "sqlite").unwrap();
        drop(sqlite);

        let jsonl = make_session_backend(tmp.path(), "jsonl").unwrap();
        jsonl
            .append("reload_user", &user_msg("hello after reload"))
            .unwrap();
        assert_eq!(jsonl.load("reload_user").len(), 1);
    }

    #[test]
    fn make_session_backend_unknown_value_falls_back_to_sqlite() {
        let tmp = TempDir::new().unwrap();
        let backend = make_session_backend(tmp.path(), "totally-not-a-backend").unwrap();
        backend.append("k1", &user_msg("hello-fallback")).unwrap();
        let db = tmp.path().join("sessions").join("sessions.db");
        assert!(
            db.exists(),
            "unknown value must fall back to sqlite, not error"
        );
    }

    #[test]
    fn make_session_backend_sqlite_imports_legacy_jsonl_on_first_open() {
        // Seed JSONL session files, then open SQLite — the .jsonl files must
        // be migrated and the imported sessions must be visible via the new
        // backend. The .jsonl files get renamed to .jsonl.migrated so the
        // operator can roll back.
        let tmp = TempDir::new().unwrap();
        {
            let jsonl = make_session_backend(tmp.path(), "jsonl").unwrap();
            jsonl.append("legacy", &user_msg("from-jsonl")).unwrap();
        }
        let sqlite = make_session_backend(tmp.path(), "sqlite").unwrap();
        let loaded = sqlite.load("legacy");
        assert_eq!(
            loaded.len(),
            1,
            "legacy JSONL session must hydrate via SQLite"
        );
        // .jsonl renamed to .jsonl.migrated; original gone.
        let jsonl_orig = tmp.path().join("sessions").join("legacy.jsonl");
        let jsonl_migrated = tmp.path().join("sessions").join("legacy.jsonl.migrated");
        assert!(!jsonl_orig.exists(), "original .jsonl should be renamed");
        assert!(
            jsonl_migrated.exists(),
            ".jsonl.migrated rollback file should remain"
        );
    }

    #[test]
    fn make_session_backend_sqlite_fails_closed_on_import_collision() {
        let tmp = TempDir::new().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        std::fs::write(
            sessions_dir.join("legacy.jsonl"),
            "{\"role\":\"user\",\"content\":\"from-jsonl\"}\n",
        )
        .unwrap();
        std::fs::write(
            sessions_dir.join("legacy.jsonl.migrated"),
            "existing archive",
        )
        .unwrap();

        let err = make_session_backend(tmp.path(), "sqlite")
            .err()
            .expect("migration collision must prevent SQLite startup");
        assert!(err.to_string().contains("Refusing to replace"));
    }

    #[test]
    fn make_session_backend_preserves_initialization_error_chain() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("sessions/sessions.db")).unwrap();

        let err = make_session_backend(tmp.path(), "sqlite")
            .err()
            .expect("a directory cannot be opened as the SQLite database");
        let message = err.to_string();
        assert!(message.contains("Failed to open session DB"));
        assert!(message.contains("unable to open database file"));
    }
}
