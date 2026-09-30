//! The recovery contract every agent rename surface shares.
//!
//! Renaming an agent commits the config change first; the state kept under
//! the old alias follows afterwards: the default per-alias workspace, memory
//! attribution, cron jobs and their run history, ACP sessions and their saved
//! working directories, and session attribution. The CLI, the gateway, and the
//! daemon RPC all run the same sequence:
//!
//! 1. [`resolve`] validates both aliases and decides whether the rename is
//!    fresh or resumes an unfinished one.
//! 2. [`arm`] records the rename in the agent lifecycle recovery journal and
//!    holds the journal lock while the surface commits the config.
//! 3. The surface commits, then calls [`acknowledge_commit`], or [`abandon`]
//!    when the commit failed.
//! 4. [`converge`] moves every follower, re-checks all of them, and clears the
//!    record only once nothing is left under the old alias.
//!
//! The record keeps the recovery durable across the window after the commit
//! and across processes. While it is open the old alias cannot be reused, and
//! re-running the same rename resumes it.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use zeroclaw_api::attribution::{Attributable as _, MemoryKind, Role};
use zeroclaw_api::memory_traits::Memory;
use zeroclaw_config::agent_recovery_journal::{
    self, AgentRecoveryJournal, JournalError, JournalGuard, RecoveryOperation, RecoveryPhase,
    RecoveryRecord,
};
use zeroclaw_config::schema::Config;
use zeroclaw_infra::acp_session_store::AcpSessionStore;
use zeroclaw_infra::session_backend::SessionBackend;
use zeroclaw_infra::session_sqlite::SqliteSessionBackend;
use zeroclaw_memory::MemoryBackendKind;

use crate::lifecycle_path::{PathPresence, inspect_lifecycle_path};

/// How long [`arm`], and a rename discovered without a record, wait for the
/// journal lock. Callers hold their config write lock while arming, so a
/// contended journal fails fast rather than stalling every config write.
const ARM_LOCK_WAIT: Duration = Duration::from_millis(250);
/// How long [`converge`] waits for the journal lock to clear a record.
const CLEAR_LOCK_WAIT: Duration = Duration::from_secs(5);
/// The store the journal's own read failures are reported under.
const JOURNAL_STORE: &str = "agent lifecycle recovery journal";

const KEY_ARMED: &str = "agents.rename_recovery.armed";
const KEY_RESUMED: &str = "agents.rename_recovery.resumed";
const KEY_CONVERGED: &str = "agents.rename_recovery.converged";
const KEY_INCOMPLETE: &str = "agents.rename_recovery.incomplete";
const KEY_REFUSED: &str = "agents.rename_recovery.refused";
const KEY_UNREADABLE: &str = "agents.rename_recovery.unreadable";
const KEY_RECORD_FAILED: &str = "agents.rename_recovery.record_failed";

/// Store handles a rename surface already holds. `None` means the surface
/// holds no handle for that store, not that the store is unconfigured: the
/// store is then resolved from the live config and what exists on disk.
#[derive(Clone, Copy)]
pub struct SurfaceStores<'a> {
    pub memory: Option<&'a Arc<dyn Memory>>,
    pub session_backend: Option<&'a Arc<dyn SessionBackend>>,
    pub acp: Option<&'a Arc<AcpSessionStore>>,
}

impl SurfaceStores<'_> {
    /// No handles: every store is resolved from the config and the disk.
    #[must_use]
    pub fn none() -> SurfaceStores<'static> {
        SurfaceStores {
            memory: None,
            session_backend: None,
            acp: None,
        }
    }
}

/// State that follows an agent's alias through a rename.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FollowerKind {
    /// The default per-alias workspace directory.
    Workspace,
    /// Memory attribution.
    Memory,
    /// Cron jobs and their run-history ownership.
    Cron,
    /// ACP sessions and their saved working directories.
    Acp,
    /// Session attribution.
    Sessions,
}

impl FollowerKind {
    /// Every follower, in the order a rename moves them.
    const ALL: [Self; 5] = [
        Self::Workspace,
        Self::Memory,
        Self::Cron,
        Self::Acp,
        Self::Sessions,
    ];

    fn as_str(self) -> &'static str {
        match self {
            Self::Workspace => "workspace",
            Self::Memory => "memory",
            Self::Cron => "cron",
            Self::Acp => "acp",
            Self::Sessions => "sessions",
        }
    }
}

impl fmt::Display for FollowerKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a follower has not converged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FollowerIssueKind {
    /// State is still attributed to the old alias.
    Lagging,
    /// The follower's store exists but could not be read, so whether state is
    /// still attributed to the old alias is unknown.
    Unreadable,
    /// The move needs an operator: the destination is already taken.
    Conflict,
}

/// A follower that has not converged. Its [`Display`](fmt::Display) is the
/// warning line a surface reports.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FollowerIssue {
    pub follower: FollowerKind,
    pub kind: FollowerIssueKind,
    /// Why an unreadable follower could not be read; for a lagging or
    /// conflicting one, the whole warning line.
    pub detail: String,
}

impl FollowerIssue {
    fn lagging(follower: FollowerKind, detail: String) -> Self {
        Self {
            follower,
            kind: FollowerIssueKind::Lagging,
            detail,
        }
    }

    fn unreadable(follower: FollowerKind, detail: String) -> Self {
        Self {
            follower,
            kind: FollowerIssueKind::Unreadable,
            detail,
        }
    }

    fn conflict(follower: FollowerKind, detail: String) -> Self {
        Self {
            follower,
            kind: FollowerIssueKind::Conflict,
            detail,
        }
    }
}

impl fmt::Display for FollowerIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            FollowerIssueKind::Unreadable => {
                write!(f, "{} could not be read: {}", self.follower, self.detail)
            }
            FollowerIssueKind::Lagging | FollowerIssueKind::Conflict => f.write_str(&self.detail),
        }
    }
}

/// What one [`converge`] moved.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct ConvergeReport {
    pub workspace_moved: bool,
    pub memory_rows: usize,
    pub cron_jobs: usize,
    pub acp_sessions: usize,
    pub acp_workspaces: usize,
    pub sessions_repointed: usize,
}

/// How a [`converge`] ended.
#[derive(Debug)]
pub enum ConvergeOutcome {
    /// Nothing is left under the old alias and the recovery record is gone.
    Converged(ConvergeReport),
    /// Some follower still lags, could not be read, or conflicts. The record
    /// stays open, and re-running the same rename resumes it.
    Incomplete {
        report: ConvergeReport,
        outstanding: Vec<FollowerIssue>,
    },
}

impl ConvergeOutcome {
    #[must_use]
    pub fn report(&self) -> &ConvergeReport {
        match self {
            Self::Converged(report) | Self::Incomplete { report, .. } => report,
        }
    }

    #[must_use]
    pub fn is_converged(&self) -> bool {
        matches!(self, Self::Converged(_))
    }

    /// One warning line per outstanding follower; empty once converged.
    #[must_use]
    pub fn warnings(&self) -> Vec<String> {
        match self {
            Self::Converged(_) => Vec::new(),
            Self::Incomplete { outstanding, .. } => {
                outstanding.iter().map(ToString::to_string).collect()
            }
        }
    }
}

/// Why a rename recovery step refused or failed.
#[derive(Debug)]
pub enum RenameRecoveryError {
    /// An alias fails the alias rules, or both aliases are the same.
    InvalidAlias { alias: String, reason: String },
    /// An alias is the reserved `default` agent.
    ReservedAlias { alias: String },
    /// There is no agent to rename: the alias is not configured, no rename of
    /// it is recorded, and no state is left under it.
    NotConfigured { alias: String },
    /// An unfinished rename retired `alias`, the old alias of a rename to
    /// `pending_to`. Display leaves `pending_to` out, since surfaces render
    /// this error before they authorize the caller.
    AliasRetired { alias: String, pending_to: String },
    /// An unfinished rename of `from` is still converging into `to`. Display
    /// leaves `from` out, for the same reason.
    RecoveryPending { from: String, to: String },
    /// A store could not be read, so whether a rename is unfinished, or state
    /// is left under an alias, is unknown.
    Unreadable { store: String, detail: String },
    /// Another process holds the recovery journal lock.
    Busy { detail: String },
    /// The recovery journal could not be written.
    Persist { detail: String },
}

impl fmt::Display for RenameRecoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidAlias { alias, reason } => {
                write!(f, "invalid agent alias `{alias}`: {reason}")
            }
            Self::ReservedAlias { alias } => {
                write!(f, "alias `{alias}` is reserved and cannot be renamed")
            }
            Self::NotConfigured { alias } => write!(f, "agents.{alias} is not configured"),
            Self::AliasRetired { alias, .. } => write!(
                f,
                "alias `{alias}` is retired by an unfinished agent rename and cannot be reused yet"
            ),
            Self::RecoveryPending { to, .. } => write!(
                f,
                "agent `{to}` is the target of an unfinished rename; re-run that rename first"
            ),
            Self::Unreadable { store, detail } => write!(f, "{store} could not be read: {detail}"),
            Self::Busy { detail } => write!(
                f,
                "agent rename recovery is in progress elsewhere; retry shortly ({detail})"
            ),
            Self::Persist { detail } => {
                write!(f, "agent rename recovery could not be recorded: {detail}")
            }
        }
    }
}

impl std::error::Error for RenameRecoveryError {}

/// What [`resolve`] decided a rename is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// The old alias is configured: arm, commit, then converge.
    Fresh,
    /// The config commit already landed and a record now covers the rename:
    /// converge without committing again.
    Resume,
}

/// A rename recorded before its config commit. It holds the journal lock
/// until [`acknowledge_commit`] or [`abandon`] consumes it; dropping it
/// instead releases the lock and leaves the prepared record, whose effect the
/// live config then decides.
#[derive(Debug)]
pub struct Armed {
    record: RecoveryRecord,
    guard: JournalGuard,
}

impl Armed {
    #[must_use]
    pub fn from(&self) -> &str {
        &self.record.from
    }

    #[must_use]
    pub fn to(&self) -> &str {
        &self.record.to
    }
}

/// Decide whether renaming `from` to `to` is a fresh rename or resumes an
/// unfinished one.
///
/// An open record decides first: this rename's own record resumes it, and a
/// record of another rename that retired either alias, or still converges
/// into one of them, refuses it. Otherwise a configured `from` is fresh. A
/// `from` that is gone while `to` is configured may be a rename committed
/// without a record (by an older build, say): the followers are probed, and
/// any state left under `from`, or a store that cannot be read, records the
/// rename as committed so it resumes and `from` stays retired until it
/// converges.
pub async fn resolve(
    config: &Config,
    from: &str,
    to: &str,
    stores: &SurfaceStores<'_>,
) -> Result<Disposition, RenameRecoveryError> {
    validate_rename(from, to)?;
    let journal = AgentRecoveryJournal::for_config(config);
    let records = load_records(config, &journal).inspect_err(|e| log_refusal(from, to, e))?;
    validate_source(config, &records, from)?;
    match match_records(&records, config, from, to) {
        RecordMatch::Refused(error) => {
            log_refusal(from, to, &error);
            return Err(error);
        }
        RecordMatch::Pending(_) => {
            log_resumed(from, to, false);
            return Ok(Disposition::Resume);
        }
        RecordMatch::Clear => {}
    }
    drop(records);
    if config.agent(from).is_some() {
        return Ok(Disposition::Fresh);
    }
    if config.agent(to).is_none() {
        return Err(RenameRecoveryError::NotConfigured {
            alias: from.to_string(),
        });
    }
    discover(config, &journal, from, to, stores).await
}

/// Record the rename of `from` to `to` before its config commit, and hold the
/// journal lock so no other lifecycle operation writes the journal until the
/// surface has committed and called [`acknowledge_commit`] or [`abandon`].
///
/// `config_before_commit` is the config the commit starts from. The record
/// keeps `from`'s workspace for the move only when it sits at the
/// alias-derived location; a custom path is alias-independent and never moves.
pub async fn arm(
    config_before_commit: &Config,
    from: &str,
    to: &str,
) -> Result<Armed, RenameRecoveryError> {
    validate_rename(from, to)?;
    let journal = AgentRecoveryJournal::for_config(config_before_commit);
    // Checked before the lock, which creates the data directory and its lock
    // file, so a request that fails it leaves nothing behind.
    let records =
        load_records(config_before_commit, &journal).inspect_err(|e| log_refusal(from, to, e))?;
    validate_source(config_before_commit, &records, from)?;
    drop(records);
    let guard = lock_journal(&journal, ARM_LOCK_WAIT).await?;
    match prepare_record(&journal, &guard, config_before_commit, from, to) {
        Ok(record) => {
            ::zeroclaw_log::record!(
                INFO,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_attrs(::serde_json::json!({
                        "error_key": KEY_ARMED,
                        "from": from,
                        "to": to,
                        "moves_workspace": record.source_workspace.is_some(),
                    })),
                "agent rename recovery armed before the config commit"
            );
            Ok(Armed { record, guard })
        }
        Err(error) => {
            log_refusal(from, to, &error);
            Err(error)
        }
    }
}

/// The surface's config commit landed: mark the record committed and release
/// the journal lock. Best effort: a failure is logged, and the prepared record
/// still covers the rename, since the live config shows the commit.
///
/// `config_after_commit` must show the commit: `from` gone and `to` present.
/// Otherwise the record is left prepared, where the live config decides its
/// effect, rather than retiring an alias that is still configured.
pub async fn acknowledge_commit(config_after_commit: &Config, armed: Armed) {
    let Armed { mut record, guard } = armed;
    let (from, to) = (record.from.clone(), record.to.clone());
    if !record.is_effective(config_after_commit) {
        log_record_failed(
            &from,
            &to,
            "the config does not show the rename committed; the record stays prepared",
        );
        return;
    }
    record.phase = RecoveryPhase::Committed;
    let journal = AgentRecoveryJournal::for_config(config_after_commit);
    let written = tokio::task::spawn_blocking(move || {
        let written = journal.upsert(&guard, record);
        drop(guard);
        written
    })
    .await;
    match written {
        Ok(Ok(())) => {}
        Ok(Err(e)) => log_record_failed(&from, &to, &e.to_string()),
        Err(e) => log_record_failed(&from, &to, &e.to_string()),
    }
}

/// The surface's config commit failed: drop the prepared record and release
/// the journal lock. Best effort: a failure is logged, and the record, whose
/// commit never landed, retires nothing and is dropped by the next [`arm`].
///
/// A `config` that shows the commit after all keeps the record, so the
/// rename's recovery is not lost.
pub async fn abandon(config: &Config, armed: Armed) {
    let Armed { record, guard } = armed;
    let (from, to) = (record.from.clone(), record.to.clone());
    if record.is_effective(config) {
        log_record_failed(
            &from,
            &to,
            "the config shows the rename committed; the record is kept",
        );
        return;
    }
    let journal = AgentRecoveryJournal::for_config(config);
    let alias = from.clone();
    let removed = tokio::task::spawn_blocking(move || {
        let removed = journal.remove(&guard, RecoveryOperation::Rename, &alias);
        drop(guard);
        removed
    })
    .await;
    match removed {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => log_record_failed(&from, &to, &e.to_string()),
        Err(e) => log_record_failed(&from, &to, &e.to_string()),
    }
}

/// Move every follower of the committed rename of `from` to `to`, re-check
/// them all, and clear the rename's record once nothing is left under `from`.
///
/// Every follower is attempted even when an earlier one fails, and each moves
/// only when its probe finds state under `from`, so a re-run repeats nothing.
/// The re-check decides the outcome: a follower whose move failed but that
/// holds nothing under `from` has converged. A converge without a record is
/// allowed, for a rename discovered or committed without one, but only once
/// `from` is no longer configured. The journal lock is taken only to clear
/// the record, never across follower I/O.
pub async fn converge(
    config: &Config,
    from: &str,
    to: &str,
    stores: &SurfaceStores<'_>,
) -> Result<ConvergeOutcome, RenameRecoveryError> {
    validate_rename(from, to)?;
    let journal = AgentRecoveryJournal::for_config(config);
    let (record, recorded) = {
        let records = load_records(config, &journal).inspect_err(|e| log_refusal(from, to, e))?;
        validate_source(config, &records, from)?;
        let recorded = records.iter().any(|r| is_rename_record(r, from, to));
        match match_records(&records, config, from, to) {
            RecordMatch::Refused(error) => {
                log_refusal(from, to, &error);
                return Err(error);
            }
            RecordMatch::Pending(record) => (Some(record.clone()), recorded),
            RecordMatch::Clear => (None, recorded),
        }
    };
    if record.is_none() && config.agent(from).is_some() {
        return Err(RenameRecoveryError::InvalidAlias {
            alias: from.to_string(),
            reason: format!(
                "agents.{from} is still configured and no committed rename of it is recorded"
            ),
        });
    }

    let plan = FollowerPlan::resolve(config, from, to, record.as_ref(), stores).await;
    let mut report = ConvergeReport::default();
    let attempted = plan.act_all(config, from, to, &mut report).await;
    let outstanding = plan.verify(config, from, attempted).await;

    if !outstanding.is_empty() {
        let warnings: Vec<String> = outstanding.iter().map(ToString::to_string).collect();
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                .with_attrs(::serde_json::json!({
                    "error_key": KEY_INCOMPLETE,
                    "from": from,
                    "to": to,
                    "report": &report,
                    "warnings": warnings,
                })),
            "agent rename has not converged; re-run the same rename to finish it"
        );
        return Ok(ConvergeOutcome::Incomplete {
            report,
            outstanding,
        });
    }
    if recorded {
        clear_record(&journal, from, to).await?;
    }
    ::zeroclaw_log::record!(
        INFO,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
            .with_outcome(::zeroclaw_log::EventOutcome::Success)
            .with_attrs(::serde_json::json!({
                "error_key": KEY_CONVERGED,
                "from": from,
                "to": to,
                "report": &report,
            })),
        "agent rename converged; nothing is left under the old alias"
    );
    Ok(ConvergeOutcome::Converged(report))
}

/// Refuse an `alias` that an unfinished rename retired. Surfaces that bring
/// an agent alias into existence call this. It only compares `alias` against
/// the journal, read from its fixed path, so it applies no alias rules.
pub async fn ensure_alias_not_retired(
    config: &Config,
    alias: &str,
) -> Result<(), RenameRecoveryError> {
    match agent_recovery_journal::retired_alias(config, alias) {
        Ok(None) => Ok(()),
        Ok(Some(record)) => {
            let error = RenameRecoveryError::AliasRetired {
                alias: alias.to_string(),
                pending_to: record.to.clone(),
            };
            log_refusal(&record.from, &record.to, &error);
            Err(error)
        }
        Err(e) => {
            let error = journal_error(e);
            log_refusal(alias, "", &error);
            Err(error)
        }
    }
}

/// Refuse an `alias` that an unfinished rename is still converging into.
/// Surfaces that delete or rename an agent call this, so the pending rename's
/// state is not stranded under an alias that no longer exists.
pub async fn ensure_not_pending_target(
    config: &Config,
    alias: &str,
) -> Result<(), RenameRecoveryError> {
    match agent_recovery_journal::pending_target(config, alias) {
        Ok(None) => Ok(()),
        Ok(Some(record)) => {
            let error = RenameRecoveryError::RecoveryPending {
                from: record.from.clone(),
                to: record.to.clone(),
            };
            log_refusal(&record.from, &record.to, &error);
            Err(error)
        }
        Err(e) => {
            let error = journal_error(e);
            log_refusal("", alias, &error);
            Err(error)
        }
    }
}

/// The alias checks every entry point makes before it reads the journal or
/// touches the filesystem. `to` names a new directory under
/// `<install>/agents`, so it must satisfy the alias grammar. Neither alias may
/// be the reserved one, and they must differ. Whether `from` must satisfy the
/// grammar as well depends on the journal: see [`validate_source`].
fn validate_rename(from: &str, to: &str) -> Result<(), RenameRecoveryError> {
    require_alias_grammar(to)?;
    for alias in [from, to] {
        if zeroclaw_config::alias_refs::is_reserved_agent_alias(alias) {
            return Err(RenameRecoveryError::ReservedAlias {
                alias: alias.to_string(),
            });
        }
    }
    if from == to {
        return Err(RenameRecoveryError::InvalidAlias {
            alias: to.to_string(),
            reason: "new alias must differ from the current name".to_string(),
        });
    }
    Ok(())
}

/// Hold `from` to the alias grammar unless this install already names it: a
/// configured alias, or the old alias of a record in `records`. Config keys
/// and journal records were written by this code or by a config older than
/// the grammar, and renaming such a legacy alias to a valid one is how an
/// operator migrates it. Only a `from` that nothing names comes straight from
/// the request into path derivation, so only that one must satisfy the
/// grammar. The journal is read from its fixed path, which derives nothing
/// from `from`.
fn validate_source(
    config: &Config,
    records: &[RecoveryRecord],
    from: &str,
) -> Result<(), RenameRecoveryError> {
    if config.agent(from).is_some() || records.iter().any(|record| record.from == from) {
        return Ok(());
    }
    require_alias_grammar(from)
}

fn require_alias_grammar(alias: &str) -> Result<(), RenameRecoveryError> {
    zeroclaw_config::helpers::validate_alias_key(alias).map_err(|reason| {
        RenameRecoveryError::InvalidAlias {
            alias: alias.to_string(),
            reason,
        }
    })
}

/// Every journal record. A config without a data directory has no journal.
fn load_records(
    config: &Config,
    journal: &AgentRecoveryJournal,
) -> Result<Vec<RecoveryRecord>, RenameRecoveryError> {
    if config.data_dir.as_os_str().is_empty() {
        return Ok(Vec::new());
    }
    journal.load().map_err(journal_error)
}

fn journal_error(error: JournalError) -> RenameRecoveryError {
    match error {
        JournalError::Busy { .. } => RenameRecoveryError::Busy {
            detail: error.to_string(),
        },
        JournalError::Write { .. } => RenameRecoveryError::Persist {
            detail: error.to_string(),
        },
        JournalError::Unreadable { .. } | JournalError::UnsupportedSchema { .. } => {
            RenameRecoveryError::Unreadable {
                store: JOURNAL_STORE.to_string(),
                detail: error.to_string(),
            }
        }
    }
}

fn is_rename_record(record: &RecoveryRecord, from: &str, to: &str) -> bool {
    record.operation == RecoveryOperation::Rename && record.from == from && record.to == to
}

/// How the effective journal records bear on renaming `from` to `to`.
enum RecordMatch<'r> {
    /// No effective record concerns either alias.
    Clear,
    /// The effective record of this very rename.
    Pending(&'r RecoveryRecord),
    /// An effective record of another rename forbids this one.
    Refused(RenameRecoveryError),
}

/// Match `records` against renaming `from` to `to`, in precedence order: `to`
/// retired by another rename; `from` still receiving another rename's state;
/// `from` retired, by this rename (pending) or another; `to` still receiving
/// another rename's state. Only effective records count: a prepared record
/// whose commit never landed retires nothing.
fn match_records<'r>(
    records: &'r [RecoveryRecord],
    config: &Config,
    from: &str,
    to: &str,
) -> RecordMatch<'r> {
    let effective = || records.iter().filter(|r| r.is_effective(config));
    if let Some(record) = effective().find(|r| r.from == to) {
        return RecordMatch::Refused(RenameRecoveryError::AliasRetired {
            alias: to.to_string(),
            pending_to: record.to.clone(),
        });
    }
    if let Some(record) = effective().find(|r| r.to == from && r.from != from) {
        return RecordMatch::Refused(RenameRecoveryError::RecoveryPending {
            from: record.from.clone(),
            to: record.to.clone(),
        });
    }
    if let Some(record) = effective().find(|r| r.from == from) {
        if is_rename_record(record, from, to) {
            return RecordMatch::Pending(record);
        }
        return RecordMatch::Refused(RenameRecoveryError::AliasRetired {
            alias: from.to_string(),
            pending_to: record.to.clone(),
        });
    }
    if let Some(record) = effective().find(|r| r.to == to) {
        return RecordMatch::Refused(RenameRecoveryError::RecoveryPending {
            from: record.from.clone(),
            to: record.to.clone(),
        });
    }
    RecordMatch::Clear
}

/// Take the journal's writer lock off the async runtime: the wait sleeps the
/// thread between attempts.
async fn lock_journal(
    journal: &AgentRecoveryJournal,
    wait: Duration,
) -> Result<JournalGuard, RenameRecoveryError> {
    let journal = journal.clone();
    match tokio::task::spawn_blocking(move || journal.lock(wait)).await {
        Ok(locked) => locked.map_err(journal_error),
        Err(e) => Err(RenameRecoveryError::Persist {
            detail: e.to_string(),
        }),
    }
}

/// Under the lock: drop void records, re-check `from` and that no effective
/// record forbids the rename against the journal as it now stands (another
/// process may have changed it since it was last read), and record the
/// rename as prepared.
fn prepare_record(
    journal: &AgentRecoveryJournal,
    guard: &JournalGuard,
    config: &Config,
    from: &str,
    to: &str,
) -> Result<RecoveryRecord, RenameRecoveryError> {
    // Holding the lock, no other operation sits between its prepare and its
    // commit, so every prepared record whose commit did not land is void.
    journal.collect_void(guard, config).map_err(journal_error)?;
    let records = journal.load().map_err(journal_error)?;
    validate_source(config, &records, from)?;
    match match_records(&records, config, from, to) {
        RecordMatch::Clear => {}
        RecordMatch::Pending(record) => {
            return Err(RenameRecoveryError::RecoveryPending {
                from: record.from.clone(),
                to: record.to.clone(),
            });
        }
        RecordMatch::Refused(error) => return Err(error),
    }
    let current = config.agent_workspace_dir(from);
    let source_workspace = (current == config.default_agent_workspace_dir(from)).then_some(current);
    let record = new_record(from, to, RecoveryPhase::Prepared, source_workspace);
    journal
        .upsert(guard, record.clone())
        .map_err(journal_error)?;
    Ok(record)
}

fn new_record(
    from: &str,
    to: &str,
    phase: RecoveryPhase,
    source_workspace: Option<PathBuf>,
) -> RecoveryRecord {
    RecoveryRecord {
        operation: RecoveryOperation::Rename,
        from: from.to_string(),
        to: to.to_string(),
        phase,
        source_workspace,
        armed_at: chrono::Utc::now().to_rfc3339(),
    }
}

/// `config` no longer has `from` but has `to`, and no record covers the
/// rename: probe the followers without acting. State left under `from`, or a
/// store that cannot be read, records the rename as committed, so `from`
/// stays retired and the rename resumes; nothing at all means there was no
/// such agent.
async fn discover(
    config: &Config,
    journal: &AgentRecoveryJournal,
    from: &str,
    to: &str,
    stores: &SurfaceStores<'_>,
) -> Result<Disposition, RenameRecoveryError> {
    let plan = FollowerPlan::resolve(config, from, to, None, stores).await;
    let mut residue = 0usize;
    let mut unreadable = None;
    for follower in FollowerKind::ALL {
        match plan.probe(follower, config, from).await {
            Ok(count) => residue += count,
            Err(issue) => {
                unreadable.get_or_insert(issue);
            }
        }
    }
    if unreadable.is_none() && residue == 0 {
        return Err(RenameRecoveryError::NotConfigured {
            alias: from.to_string(),
        });
    }

    let disposition = record_discovered(config, journal, from, to)
        .await
        .inspect_err(|e| log_refusal(from, to, e))?;
    if let Some(issue) = unreadable {
        let error = RenameRecoveryError::Unreadable {
            store: issue.follower.to_string(),
            detail: issue.detail,
        };
        log_refusal(from, to, &error);
        return Err(error);
    }
    log_resumed(from, to, true);
    Ok(disposition)
}

/// Record a rename found committed without a record. Another process may
/// have recorded it, or a rename that forbids it, since [`resolve`] read the
/// journal; under the lock that record decides instead. `from` is re-checked
/// against the journal as it now stands, since a record that named it may
/// have been cleared meanwhile.
async fn record_discovered(
    config: &Config,
    journal: &AgentRecoveryJournal,
    from: &str,
    to: &str,
) -> Result<Disposition, RenameRecoveryError> {
    let guard = lock_journal(journal, ARM_LOCK_WAIT).await?;
    let records = journal.load().map_err(journal_error)?;
    validate_source(config, &records, from)?;
    match match_records(&records, config, from, to) {
        RecordMatch::Pending(_) => return Ok(Disposition::Resume),
        RecordMatch::Refused(error) => return Err(error),
        RecordMatch::Clear => {}
    }
    // Only a `to` at the alias-derived location takes the old workspace.
    let default_to = config.default_agent_workspace_dir(to);
    let source_workspace = (config.agent_workspace_dir(to) == default_to)
        .then(|| config.default_agent_workspace_dir(from));
    journal
        .upsert(
            &guard,
            new_record(from, to, RecoveryPhase::Committed, source_workspace),
        )
        .map_err(journal_error)?;
    Ok(Disposition::Resume)
}

/// Drop the record of the converged rename. Another process may have
/// replaced or cleared it since this converge read the journal, so it is
/// removed only while it still names this rename.
async fn clear_record(
    journal: &AgentRecoveryJournal,
    from: &str,
    to: &str,
) -> Result<(), RenameRecoveryError> {
    let journal = journal.clone();
    let (from, to) = (from.to_string(), to.to_string());
    let cleared = tokio::task::spawn_blocking(move || -> Result<(), JournalError> {
        let guard = journal.lock(CLEAR_LOCK_WAIT)?;
        let records = journal.load()?;
        if records.iter().any(|r| is_rename_record(r, &from, &to)) {
            journal.remove(&guard, RecoveryOperation::Rename, &from)?;
        }
        Ok(())
    })
    .await;
    match cleared {
        Ok(cleared) => cleared.map_err(journal_error),
        Err(e) => Err(RenameRecoveryError::Persist {
            detail: e.to_string(),
        }),
    }
}

/// A follower's store as resolved for one rename.
enum Slot<T> {
    /// The store does not exist or keeps no per-agent state: nothing moves.
    OutOfScope,
    /// The store exists but could not be inspected or opened. Carries why.
    Unreadable(String),
    /// The store, open.
    Open(T),
}

/// Where the old alias's workspace moves from and to.
struct WorkspaceMove {
    source: PathBuf,
    destination: PathBuf,
    /// Every spelling of `source` an ACP row may have recorded as its working
    /// directory: as configured, and with symlinks resolved.
    spellings: Vec<String>,
}

/// Where each follower's state lives for one rename, resolved once from the
/// live config, what exists on disk, and the genuine handles the surface
/// holds. Resolving opens only stores that already exist.
struct FollowerPlan {
    workspace: Option<WorkspaceMove>,
    memory: Slot<Arc<dyn Memory>>,
    acp: Slot<Arc<AcpSessionStore>>,
    sessions: Slot<Arc<dyn SessionBackend>>,
}

impl FollowerPlan {
    async fn resolve(
        config: &Config,
        from: &str,
        to: &str,
        record: Option<&RecoveryRecord>,
        stores: &SurfaceStores<'_>,
    ) -> Self {
        Self {
            workspace: workspace_move(config, from, to, record).await,
            memory: memory_slot(config, stores).await,
            acp: match stores.acp {
                Some(store) => Slot::Open(Arc::clone(store)),
                None => {
                    open_existing(&AcpSessionStore::db_path(&config.data_dir), || {
                        AcpSessionStore::new(&config.data_dir).map(Arc::new)
                    })
                    .await
                }
            },
            sessions: match stores.session_backend {
                Some(backend) => Slot::Open(Arc::clone(backend)),
                None => {
                    open_existing(&SqliteSessionBackend::db_path(&config.data_dir), || {
                        SqliteSessionBackend::new(&config.data_dir)
                            .map(|backend| Arc::new(backend) as Arc<dyn SessionBackend>)
                    })
                    .await
                }
            },
        }
    }

    /// How much state `follower` still attributes to `from`.
    async fn probe(
        &self,
        follower: FollowerKind,
        config: &Config,
        from: &str,
    ) -> Result<usize, FollowerIssue> {
        let unreadable = |detail: String| FollowerIssue::unreadable(follower, detail);
        match follower {
            FollowerKind::Workspace => match &self.workspace {
                None => Ok(0),
                Some(workspace) => match inspect_lifecycle_path(&workspace.source).await {
                    PathPresence::Present => Ok(1),
                    PathPresence::Absent => Ok(0),
                    PathPresence::Uninspectable(reason) => Err(unreadable(reason)),
                },
            },
            FollowerKind::Memory => match &self.memory {
                Slot::OutOfScope => Ok(0),
                Slot::Unreadable(reason) => Err(unreadable(reason.clone())),
                Slot::Open(memory) => memory
                    .count_agent(from)
                    .await
                    .map_err(|e| unreadable(format!("{e:#}"))),
            },
            FollowerKind::Cron => match inspect_lifecycle_path(&crate::cron::db_path(config)).await
            {
                PathPresence::Absent => Ok(0),
                PathPresence::Uninspectable(reason) => Err(unreadable(reason)),
                PathPresence::Present => crate::cron::agent_residue_count(config, from)
                    .map(Option::unwrap_or_default)
                    .map_err(|e| unreadable(format!("{e:#}"))),
            },
            FollowerKind::Acp => match &self.acp {
                Slot::OutOfScope => Ok(0),
                Slot::Unreadable(reason) => Err(unreadable(reason.clone())),
                Slot::Open(store) => {
                    let mut residue = store
                        .count_sessions_by_agent(from)
                        .map_err(|e| unreadable(format!("{e:#}")))?;
                    for spelling in self.workspace_spellings() {
                        residue += store
                            .count_sessions_under_workspace(spelling)
                            .map_err(|e| unreadable(format!("{e:#}")))?;
                    }
                    Ok(residue)
                }
            },
            FollowerKind::Sessions => match &self.sessions {
                Slot::OutOfScope => Ok(0),
                Slot::Unreadable(reason) => Err(unreadable(reason.clone())),
                Slot::Open(backend) => backend
                    .count_agent_attribution(from)
                    .map_err(|e| unreadable(e.to_string())),
            },
        }
    }

    fn workspace_spellings(&self) -> &[String] {
        self.workspace
            .as_ref()
            .map_or(&[], |workspace| workspace.spellings.as_slice())
    }

    /// Probe every follower in order and move the ones holding state under
    /// `from`. Returns each follower with the issue its probe or move hit.
    async fn act_all(
        &self,
        config: &Config,
        from: &str,
        to: &str,
        report: &mut ConvergeReport,
    ) -> Vec<(FollowerKind, Option<FollowerIssue>)> {
        let mut attempted = Vec::with_capacity(FollowerKind::ALL.len());
        // ACP rows keep the old workspace's absolute path. They follow the
        // workspace only once it has left the old location (moved now, moved
        // earlier, or never there); while it stays there, so do they.
        let mut workspace_settled = true;
        for follower in FollowerKind::ALL {
            let issue = match self.probe(follower, config, from).await {
                Err(issue) => Some(issue),
                Ok(0) => None,
                Ok(_) => {
                    self.act(follower, config, from, to, workspace_settled, report)
                        .await
                }
            };
            if follower == FollowerKind::Workspace {
                workspace_settled = issue.is_none();
            }
            attempted.push((follower, issue));
        }
        attempted
    }

    async fn act(
        &self,
        follower: FollowerKind,
        config: &Config,
        from: &str,
        to: &str,
        workspace_settled: bool,
        report: &mut ConvergeReport,
    ) -> Option<FollowerIssue> {
        match follower {
            FollowerKind::Workspace => {
                let workspace = self.workspace.as_ref()?;
                let moved = move_workspace(workspace).await;
                if moved.is_ok() {
                    report.workspace_moved = true;
                    // Best effort: `<install>/agents/<from>` is empty now
                    // unless something else was kept beside the workspace.
                    if let Some(alias_dir) = config.default_agent_workspace_dir(from).parent() {
                        let _ = tokio::fs::remove_dir(alias_dir).await;
                    }
                }
                moved.err()
            }
            FollowerKind::Memory => {
                let Slot::Open(memory) = &self.memory else {
                    return None;
                };
                match memory.rename_agent(from, to).await {
                    Ok(rows) => {
                        report.memory_rows = rows;
                        None
                    }
                    Err(e) => Some(FollowerIssue::lagging(
                        follower,
                        format!("memory rename: {e:#}"),
                    )),
                }
            }
            FollowerKind::Cron => match crate::cron::rename_jobs_by_agent(config, from, to) {
                Ok(jobs) => {
                    report.cron_jobs = jobs;
                    None
                }
                Err(e) => Some(FollowerIssue::lagging(
                    follower,
                    format!("cron rename: {e:#}"),
                )),
            },
            FollowerKind::Acp => {
                let Slot::Open(store) = &self.acp else {
                    return None;
                };
                let mut failure = None;
                match store.rename_sessions_by_agent(from, to) {
                    Ok(rows) => report.acp_sessions = rows,
                    Err(e) => failure = Some(format!("acp rename: {e:#}")),
                }
                if let (Some(workspace), true) = (&self.workspace, workspace_settled) {
                    let destination = workspace.destination.to_string_lossy();
                    for spelling in &workspace.spellings {
                        match store.relocate_session_workspaces(spelling, &destination) {
                            Ok(rows) => report.acp_workspaces += rows,
                            Err(e) => {
                                failure.get_or_insert_with(|| format!("acp rename: {e:#}"));
                            }
                        }
                    }
                }
                failure.map(|detail| FollowerIssue::lagging(follower, detail))
            }
            FollowerKind::Sessions => {
                let Slot::Open(backend) = &self.sessions else {
                    return None;
                };
                match backend.rename_agent_attribution(from, to) {
                    Ok(rows) => {
                        report.sessions_repointed = rows;
                        None
                    }
                    Err(e) => Some(FollowerIssue::lagging(
                        follower,
                        format!("session attribution rename: {e}"),
                    )),
                }
            }
        }
    }

    /// Re-probe every follower. The re-probe is authoritative: a follower
    /// holding nothing under `from` has converged whatever its move reported,
    /// one still holding state reports why its move did not take it, or that
    /// it still lags, and one that cannot be read stays unreadable.
    async fn verify(
        &self,
        config: &Config,
        from: &str,
        attempted: Vec<(FollowerKind, Option<FollowerIssue>)>,
    ) -> Vec<FollowerIssue> {
        let mut outstanding = Vec::new();
        for (follower, issue) in attempted {
            match self.probe(follower, config, from).await {
                Ok(0) => {}
                Ok(_) => outstanding.push(issue.unwrap_or_else(|| {
                    FollowerIssue::lagging(
                        follower,
                        format!("{follower} still attributes state to `{from}`"),
                    )
                })),
                Err(unreadable) => outstanding.push(unreadable),
            }
        }
        outstanding
    }
}

/// The workspace move of this rename, or `None` when the workspace does not
/// follow the alias: `to` keeps a custom path, or `from`'s recorded workspace
/// was custom. The source is the recorded one when a record covers the
/// rename, and otherwise `from`'s alias-derived location.
async fn workspace_move(
    config: &Config,
    from: &str,
    to: &str,
    record: Option<&RecoveryRecord>,
) -> Option<WorkspaceMove> {
    let destination = config.default_agent_workspace_dir(to);
    if config.agent_workspace_dir(to) != destination {
        return None;
    }
    let source = match record {
        Some(record) => record.source_workspace.clone()?,
        None => config.default_agent_workspace_dir(from),
    };
    let mut spellings = vec![source.to_string_lossy().into_owned()];
    if let Some(canonical) = canonical_spelling(&source).await {
        let canonical = canonical.to_string_lossy().into_owned();
        if !spellings.contains(&canonical) {
            spellings.push(canonical);
        }
    }
    Some(WorkspaceMove {
        source,
        destination,
        spellings,
    })
}

/// `path` with the symlinks in its existing part resolved. A path that no
/// longer exists (a workspace an earlier run already moved) resolves its
/// nearest existing ancestor and keeps the rest, so rows recorded under the
/// resolved spelling are still found.
async fn canonical_spelling(path: &Path) -> Option<PathBuf> {
    let mut missing = Vec::new();
    let mut current = path;
    loop {
        if let Ok(mut canonical) = tokio::fs::canonicalize(current).await {
            canonical.extend(missing.iter().rev());
            return Some(canonical);
        }
        missing.push(current.file_name()?);
        current = current.parent().filter(|p| !p.as_os_str().is_empty())?;
    }
}

/// Move the old workspace to the new alias-derived location. An absent
/// destination is created, and an empty directory there is replaced; anything
/// else there is left for the operator.
async fn move_workspace(workspace: &WorkspaceMove) -> Result<(), FollowerIssue> {
    let WorkspaceMove {
        source,
        destination,
        ..
    } = workspace;
    let follower = FollowerKind::Workspace;
    let move_failed = |e: &dyn fmt::Display| {
        FollowerIssue::lagging(
            follower,
            format!(
                "workspace move {} -> {} failed: {e}",
                source.display(),
                destination.display()
            ),
        )
    };
    match inspect_lifecycle_path(destination).await {
        PathPresence::Uninspectable(reason) => {
            return Err(FollowerIssue::unreadable(follower, reason));
        }
        PathPresence::Absent => {
            if let Some(parent) = destination.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| move_failed(&e))?;
            }
        }
        PathPresence::Present => match is_empty_directory(destination).await {
            Ok(true) => tokio::fs::remove_dir(destination)
                .await
                .map_err(|e| move_failed(&e))?,
            Ok(false) => {
                return Err(FollowerIssue::conflict(
                    follower,
                    format!(
                        "workspace destination {} already exists and is not empty",
                        destination.display()
                    ),
                ));
            }
            Err(e) => {
                return Err(FollowerIssue::unreadable(
                    follower,
                    format!("cannot inspect {}: {e}", destination.display()),
                ));
            }
        },
    }
    tokio::fs::rename(source, destination)
        .await
        .map_err(|e| move_failed(&e))
}

/// Whether `path` is a real directory with nothing in it. A symlink or a file
/// is not.
async fn is_empty_directory(path: &Path) -> std::io::Result<bool> {
    if !tokio::fs::symlink_metadata(path).await?.is_dir() {
        return Ok(false);
    }
    Ok(tokio::fs::read_dir(path)
        .await?
        .next_entry()
        .await?
        .is_none())
}

/// The memory store holding `from`'s attribution. A surface handle is used
/// unless it is the `NoneMemory` placeholder a surface falls back to when its
/// configured backend could not be built (or it booted without agents): that
/// placeholder's empty answers say nothing about the configured store.
async fn memory_slot(config: &Config, stores: &SurfaceStores<'_>) -> Slot<Arc<dyn Memory>> {
    let backend = zeroclaw_memory::classify_memory_backend(
        &zeroclaw_memory::backend_kind_from_dotted(&config.memory.backend),
    );
    if let Some(handle) = stores.memory {
        let placeholder = matches!(handle.role(), Role::Memory(MemoryKind::None))
            && backend != MemoryBackendKind::None;
        if !placeholder {
            return Slot::Open(Arc::clone(handle));
        }
    }
    let open = || -> anyhow::Result<Arc<dyn Memory>> {
        Ok(Arc::from(zeroclaw_memory::create_memory_from_config(
            config, None,
        )?))
    };
    match backend {
        // No per-agent rows: an unknown backend name builds markdown memory.
        MemoryBackendKind::None | MemoryBackendKind::Markdown | MemoryBackendKind::Unknown => {
            Slot::OutOfScope
        }
        MemoryBackendKind::Sqlite | MemoryBackendKind::Lucid => {
            match zeroclaw_memory::sqlite_db_path_for_config(config) {
                Some(path) => open_existing(&path, open).await,
                None => Slot::OutOfScope,
            }
        }
        MemoryBackendKind::Postgres | MemoryBackendKind::Qdrant => opened(open()),
    }
}

/// Open the store whose file is `path` only if that file exists, so resolving
/// a follower never creates its store.
async fn open_existing<T>(path: &Path, open: impl FnOnce() -> anyhow::Result<T>) -> Slot<T> {
    match inspect_lifecycle_path(path).await {
        PathPresence::Absent => Slot::OutOfScope,
        PathPresence::Uninspectable(reason) => Slot::Unreadable(reason),
        PathPresence::Present => opened(open()),
    }
}

fn opened<T>(store: anyhow::Result<T>) -> Slot<T> {
    match store {
        Ok(store) => Slot::Open(store),
        Err(e) => Slot::Unreadable(format!("{e:#}")),
    }
}

fn log_resumed(from: &str, to: &str, discovered: bool) {
    ::zeroclaw_log::record!(
        INFO,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(
            ::serde_json::json!({
                "error_key": KEY_RESUMED,
                "from": from,
                "to": to,
                "discovered": discovered,
            })
        ),
        "agent rename resumes an unfinished rename"
    );
}

/// Log a refusal the recovery journal or an unreadable store decided. Plain
/// input errors are the caller's to report.
fn log_refusal(from: &str, to: &str, error: &RenameRecoveryError) {
    let (error_key, message) = match error {
        RenameRecoveryError::AliasRetired { .. } | RenameRecoveryError::RecoveryPending { .. } => (
            KEY_REFUSED,
            "agent lifecycle operation refused by an unfinished agent rename",
        ),
        RenameRecoveryError::Unreadable { .. } => (
            KEY_UNREADABLE,
            "agent rename recovery could not read a store it must check",
        ),
        _ => return,
    };
    ::zeroclaw_log::record!(
        WARN,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
            .with_attrs(::serde_json::json!({
                "error_key": error_key,
                "from": from,
                "to": to,
                "error": error.to_string(),
            })),
        message
    );
}

fn log_record_failed(from: &str, to: &str, detail: &str) {
    ::zeroclaw_log::record!(
        WARN,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
            .with_attrs(::serde_json::json!({
                "error_key": KEY_RECORD_FAILED,
                "from": from,
                "to": to,
                "error": detail,
            })),
        "agent rename recovery record was not updated"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;
    use zeroclaw_api::memory_traits::{MemoryCategory, MemoryEntry};
    use zeroclaw_config::agent_recovery_journal::{pending_target, retired_alias};
    use zeroclaw_config::alias_refs::{self, AliasKind};
    use zeroclaw_config::schema::AliasedAgentConfig;
    use zeroclaw_memory::{NoneMemory, SqliteMemory};

    const FROM: &str = "scout";
    const TO: &str = "ranger";
    /// A configured alias older than the alias grammar, and the valid alias
    /// an operator migrates it to.
    const LEGACY: &str = "Legacy-Agent";
    const MIGRATED: &str = "legacy_agent";

    /// An install under `tmp` whose only agents are `aliases`, each able to
    /// own cron jobs. Nothing is created on disk.
    fn fixture(tmp: &TempDir, aliases: &[&str]) -> Config {
        let mut config = Config {
            config_path: tmp.path().join("config.toml"),
            data_dir: tmp.path().join("data"),
            ..Config::default()
        };
        config.agents.clear();
        config
            .risk_profiles
            .entry("default".to_string())
            .or_default();
        config
            .runtime_profiles
            .entry("default".to_string())
            .or_default();
        for alias in aliases {
            config.agents.insert((*alias).to_string(), agent());
        }
        config
    }

    fn agent() -> AliasedAgentConfig {
        AliasedAgentConfig {
            risk_profile: "default".into(),
            runtime_profile: "default".into(),
            ..AliasedAgentConfig::default()
        }
    }

    /// `config` with `from` renamed to `to` the way every surface commits it.
    fn renamed(config: &Config, from: &str, to: &str) -> Config {
        let mut after = config.clone();
        alias_refs::rename_with_cascade(&mut after, &AliasKind::Agent, from, to).unwrap();
        after
    }

    fn committed(config: &Config) -> Config {
        renamed(config, FROM, TO)
    }

    /// A surface's commit: arm, commit, acknowledge. Nothing has converged.
    async fn arm_and_commit(before: &Config) -> Config {
        let armed = arm(before, FROM, TO).await.unwrap();
        let after = committed(before);
        acknowledge_commit(&after, armed).await;
        after
    }

    fn records(config: &Config) -> Vec<RecoveryRecord> {
        AgentRecoveryJournal::for_config(config).load().unwrap()
    }

    fn outstanding(outcome: &ConvergeOutcome) -> &[FollowerIssue] {
        match outcome {
            ConvergeOutcome::Converged(_) => &[],
            ConvergeOutcome::Incomplete { outstanding, .. } => outstanding,
        }
    }

    fn lagging_followers(outcome: &ConvergeOutcome) -> Vec<FollowerKind> {
        outstanding(outcome)
            .iter()
            .map(|issue| issue.follower)
            .collect()
    }

    /// Put a file in `alias`'s alias-derived workspace, and return the
    /// workspace.
    fn seed_workspace(config: &Config, alias: &str) -> PathBuf {
        let workspace = config.default_agent_workspace_dir(alias);
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("MEMORY.md"), alias).unwrap();
        workspace
    }

    /// Put a file where `<install>/agents/<TO>` must be a directory.
    fn block_destination(config: &Config) -> PathBuf {
        let alias_dir = config
            .default_agent_workspace_dir(TO)
            .parent()
            .unwrap()
            .to_path_buf();
        std::fs::create_dir_all(alias_dir.parent().unwrap()).unwrap();
        std::fs::write(&alias_dir, "in the way").unwrap();
        alias_dir
    }

    fn sqlite_memory(config: &Config) -> SqliteMemory {
        SqliteMemory::new("sqlite", &config.data_dir).unwrap()
    }

    /// Give `alias` its memory identity and `rows` memories.
    async fn seed_memory(config: &Config, alias: &str, rows: usize) {
        let memory = sqlite_memory(config);
        let agent_id = memory.ensure_agent_uuid(alias).await.unwrap();
        for idx in 0..rows {
            memory
                .store_with_agent(
                    &format!("{alias}-{idx}"),
                    "remembered",
                    MemoryCategory::Core,
                    None,
                    None,
                    None,
                    Some(&agent_id),
                )
                .await
                .unwrap();
        }
    }

    async fn memory_identity(config: &Config, alias: &str) -> usize {
        sqlite_memory(config).count_agent(alias).await.unwrap()
    }

    async fn memory_rows(config: &Config, alias: &str) -> usize {
        sqlite_memory(config)
            .export_agent(alias)
            .await
            .unwrap()
            .len()
    }

    /// Add a cron job owned by `alias`. Its risk-profile check creates the
    /// owner's workspace, so that is pointed away from the one under test.
    fn seed_cron(config: &Config, alias: &str) {
        let mut scratch = config.clone();
        if let Some(agent) = scratch.agents.get_mut(alias) {
            agent.workspace.path = Some(config.install_root_dir().join("scratch").join(alias));
        }
        crate::cron::add_job(&scratch, alias, "*/5 * * * *", "echo hello").unwrap();
    }

    fn cron_residue(config: &Config, alias: &str) -> Option<usize> {
        crate::cron::agent_residue_count(config, alias).unwrap()
    }

    fn seed_acp(config: &Config, session: &str, alias: &str, workspace: &Path) {
        AcpSessionStore::new(&config.data_dir)
            .unwrap()
            .create_session(session, alias, &workspace.to_string_lossy(), None)
            .unwrap();
    }

    /// The owner and working directory of an ACP session.
    fn acp_row(config: &Config, session: &str) -> (String, PathBuf) {
        let row = AcpSessionStore::new(&config.data_dir)
            .unwrap()
            .load_session(session)
            .unwrap()
            .unwrap();
        (row.agent_alias, PathBuf::from(row.workspace_dir))
    }

    fn seed_session(config: &Config, session: &str, alias: &str) {
        SqliteSessionBackend::new(&config.data_dir)
            .unwrap()
            .set_session_agent_alias(session, alias)
            .unwrap();
    }

    fn session_owner(config: &Config, session: &str) -> Option<String> {
        SqliteSessionBackend::new(&config.data_dir)
            .unwrap()
            .get_session_agent_alias(session)
            .unwrap()
    }

    /// Seed every follower with state under `FROM`, and return the workspace.
    async fn seed_every_follower(config: &Config) -> PathBuf {
        let workspace = seed_workspace(config, FROM);
        seed_memory(config, FROM, 2).await;
        seed_cron(config, FROM);
        seed_acp(config, "acp-1", FROM, &workspace);
        seed_session(config, "chat-1", FROM);
        workspace
    }

    /// A memory backend that keeps only per-agent identity counts and counts
    /// the renames it is asked for. Like the SQL backends, it refuses to merge
    /// into an alias that already owns memory.
    #[derive(Default)]
    struct CountingMemory {
        agents: parking_lot::Mutex<HashMap<String, usize>>,
        renames: AtomicUsize,
    }

    impl zeroclaw_api::attribution::Attributable for CountingMemory {
        fn role(&self) -> Role {
            Role::Memory(MemoryKind::Sqlite)
        }

        fn alias(&self) -> &str {
            "counting"
        }
    }

    #[async_trait::async_trait]
    impl Memory for CountingMemory {
        fn name(&self) -> &str {
            "counting"
        }

        async fn store(
            &self,
            _key: &str,
            _content: &str,
            _category: MemoryCategory,
            _session_id: Option<&str>,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        async fn recall(
            &self,
            _query: &str,
            _limit: usize,
            _session_id: Option<&str>,
            _since: Option<&str>,
            _until: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            Ok(Vec::new())
        }

        async fn get(&self, _key: &str) -> anyhow::Result<Option<MemoryEntry>> {
            Ok(None)
        }

        async fn list(
            &self,
            _category: Option<&MemoryCategory>,
            _session_id: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            Ok(Vec::new())
        }

        async fn forget(&self, _key: &str) -> anyhow::Result<bool> {
            Ok(false)
        }

        async fn forget_for_agent(&self, _key: &str, _agent_id: &str) -> anyhow::Result<bool> {
            Ok(false)
        }

        async fn count(&self) -> anyhow::Result<usize> {
            Ok(self.agents.lock().values().sum())
        }

        async fn health_check(&self) -> bool {
            true
        }

        async fn store_with_agent(
            &self,
            _key: &str,
            _content: &str,
            _category: MemoryCategory,
            _session_id: Option<&str>,
            _namespace: Option<&str>,
            _importance: Option<f64>,
            _agent_id: Option<&str>,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        async fn recall_for_agents(
            &self,
            _allowed_agent_ids: &[&str],
            _query: &str,
            _limit: usize,
            _session_id: Option<&str>,
            _since: Option<&str>,
            _until: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            Ok(Vec::new())
        }

        async fn rename_agent(&self, from: &str, to: &str) -> anyhow::Result<usize> {
            self.renames.fetch_add(1, Ordering::SeqCst);
            let mut agents = self.agents.lock();
            if agents.get(to).is_some_and(|count| *count > 0) {
                anyhow::bail!("cannot rename agent memory to `{to}`: refusing to merge");
            }
            let moved = agents.remove(from).unwrap_or(0);
            if moved > 0 {
                agents.insert(to.to_string(), moved);
            }
            Ok(usize::from(moved > 0))
        }

        async fn count_agent(&self, agent_alias: &str) -> anyhow::Result<usize> {
            Ok(self.agents.lock().get(agent_alias).copied().unwrap_or(0))
        }
    }

    #[tokio::test]
    async fn a_fresh_rename_moves_every_follower_and_clears_its_record() {
        let tmp = TempDir::new().unwrap();
        let before = fixture(&tmp, &[FROM]);
        let source = seed_every_follower(&before).await;
        let stores = SurfaceStores::none();

        assert_eq!(
            resolve(&before, FROM, TO, &stores).await.unwrap(),
            Disposition::Fresh
        );
        let armed = arm(&before, FROM, TO).await.unwrap();
        assert_eq!((armed.from(), armed.to()), (FROM, TO));
        assert_eq!(
            retired_alias(&before, FROM).unwrap(),
            None,
            "a prepared record retires nothing until its commit lands"
        );
        let after = committed(&before);
        acknowledge_commit(&after, armed).await;

        let recorded = records(&after);
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].phase, RecoveryPhase::Committed);
        assert_eq!(
            recorded[0].source_workspace.as_deref(),
            Some(source.as_path())
        );
        assert!(retired_alias(&after, FROM).unwrap().is_some());
        assert!(pending_target(&after, TO).unwrap().is_some());

        let outcome = converge(&after, FROM, TO, &stores).await.unwrap();
        assert!(outcome.is_converged(), "{:?}", outcome.warnings());
        assert!(outcome.warnings().is_empty());
        let report = outcome.report();
        assert!(report.workspace_moved);
        assert_eq!(
            report.memory_rows, 1,
            "the identity row carries the memories"
        );
        assert_eq!(report.cron_jobs, 1);
        assert_eq!(report.acp_sessions, 1);
        assert_eq!(report.acp_workspaces, 1);
        assert_eq!(report.sessions_repointed, 1);

        let destination = after.default_agent_workspace_dir(TO);
        assert_eq!(
            std::fs::read_to_string(destination.join("MEMORY.md")).unwrap(),
            FROM
        );
        assert!(
            !source.parent().unwrap().exists(),
            "the emptied old alias directory is removed"
        );
        assert_eq!(memory_identity(&after, FROM).await, 0);
        assert_eq!(memory_rows(&after, TO).await, 2);
        assert_eq!(cron_residue(&after, FROM), Some(0));
        assert_eq!(cron_residue(&after, TO), Some(1));
        assert_eq!(acp_row(&after, "acp-1"), (TO.to_string(), destination));
        assert_eq!(session_owner(&after, "chat-1").as_deref(), Some(TO));

        assert!(!AgentRecoveryJournal::for_config(&after).path().exists());
        assert_eq!(retired_alias(&after, FROM).unwrap(), None);
        assert_eq!(pending_target(&after, TO).unwrap(), None);
    }

    #[tokio::test]
    async fn a_rename_interrupted_after_its_commit_resumes() {
        let tmp = TempDir::new().unwrap();
        let before = fixture(&tmp, &[FROM]);
        let source = seed_workspace(&before, FROM);
        seed_cron(&before, FROM);
        let stores = SurfaceStores::none();

        let after = arm_and_commit(&before).await;
        // The process stops here: nothing has converged.
        assert!(source.exists());
        assert_eq!(
            resolve(&after, FROM, TO, &stores).await.unwrap(),
            Disposition::Resume
        );

        let outcome = converge(&after, FROM, TO, &stores).await.unwrap();
        assert!(outcome.is_converged(), "{:?}", outcome.warnings());
        assert!(outcome.report().workspace_moved);
        assert_eq!(outcome.report().cron_jobs, 1);
        assert!(records(&after).is_empty());
    }

    #[tokio::test]
    async fn a_rename_interrupted_before_acknowledging_its_commit_resumes() {
        let tmp = TempDir::new().unwrap();
        let before = fixture(&tmp, &[FROM]);
        seed_workspace(&before, FROM);
        let stores = SurfaceStores::none();

        let armed = arm(&before, FROM, TO).await.unwrap();
        let after = committed(&before);
        // The process stops after the commit, before acknowledging it.
        drop(armed);
        assert_eq!(records(&after)[0].phase, RecoveryPhase::Prepared);
        assert!(
            retired_alias(&after, FROM).unwrap().is_some(),
            "the live config shows the commit, so the prepared record retires the old alias"
        );

        assert_eq!(
            resolve(&after, FROM, TO, &stores).await.unwrap(),
            Disposition::Resume
        );
        let outcome = converge(&after, FROM, TO, &stores).await.unwrap();
        assert!(outcome.is_converged(), "{:?}", outcome.warnings());
        assert!(records(&after).is_empty());
    }

    #[tokio::test]
    async fn a_rename_committed_without_a_record_is_discovered_and_recorded() {
        let tmp = TempDir::new().unwrap();
        let before = fixture(&tmp, &[FROM]);
        seed_cron(&before, FROM);
        // Committed with no record, as an older build did.
        let after = committed(&before);
        let stores = SurfaceStores::none();

        assert_eq!(
            resolve(&after, FROM, TO, &stores).await.unwrap(),
            Disposition::Resume
        );
        let recorded = records(&after);
        assert_eq!(recorded.len(), 1);
        assert_eq!(
            (
                recorded[0].from.as_str(),
                recorded[0].to.as_str(),
                recorded[0].phase
            ),
            (FROM, TO, RecoveryPhase::Committed)
        );
        assert_eq!(
            recorded[0].source_workspace,
            Some(after.default_agent_workspace_dir(FROM))
        );
        assert!(retired_alias(&after, FROM).unwrap().is_some());

        // Asking again resumes the same record rather than adding one.
        assert_eq!(
            resolve(&after, FROM, TO, &stores).await.unwrap(),
            Disposition::Resume
        );
        assert_eq!(records(&after).len(), 1);

        let outcome = converge(&after, FROM, TO, &stores).await.unwrap();
        assert!(outcome.is_converged(), "{:?}", outcome.warnings());
        assert_eq!(cron_residue(&after, TO), Some(1));
        assert!(records(&after).is_empty());
    }

    #[tokio::test]
    async fn discovery_that_cannot_read_a_store_still_retires_the_old_alias() {
        let tmp = TempDir::new().unwrap();
        let after = committed(&fixture(&tmp, &[FROM]));
        // A directory where the cron database belongs cannot be read.
        std::fs::create_dir_all(crate::cron::db_path(&after)).unwrap();

        let err = resolve(&after, FROM, TO, &SurfaceStores::none())
            .await
            .unwrap_err();
        assert!(
            matches!(&err, RenameRecoveryError::Unreadable { store, .. } if store == "cron"),
            "{err:?}"
        );
        assert!(
            err.to_string().starts_with("cron could not be read: "),
            "{err}"
        );
        let retired = retired_alias(&after, FROM)
            .unwrap()
            .expect("an unreadable follower may hold state, so the old alias stays retired");
        assert_eq!(retired.phase, RecoveryPhase::Committed);
        assert_eq!(retired.to, TO);
    }

    #[tokio::test]
    async fn a_rename_with_nothing_left_behind_is_not_configured_and_records_nothing() {
        let tmp = TempDir::new().unwrap();
        let after = committed(&fixture(&tmp, &[FROM]));
        let stores = SurfaceStores::none();

        let err = resolve(&after, FROM, TO, &stores).await.unwrap_err();
        assert!(
            matches!(&err, RenameRecoveryError::NotConfigured { alias } if alias == FROM),
            "{err:?}"
        );
        assert_eq!(err.to_string(), format!("agents.{FROM} is not configured"));

        // Neither alias configured is not configured either.
        let neither = fixture(&tmp, &[]);
        assert!(matches!(
            resolve(&neither, FROM, TO, &stores).await,
            Err(RenameRecoveryError::NotConfigured { .. })
        ));
        assert!(
            !after.data_dir.exists(),
            "resolving created no journal, lock, or store"
        );
    }

    #[tokio::test]
    async fn a_retried_rename_keeps_the_memory_identity_it_already_moved() {
        let tmp = TempDir::new().unwrap();
        let before = fixture(&tmp, &[FROM]);
        seed_workspace(&before, FROM);
        // An identity row with no memories under it.
        seed_memory(&before, FROM, 0).await;
        let after = arm_and_commit(&before).await;
        let blocker = block_destination(&after);
        let stores = SurfaceStores::none();

        let first = converge(&after, FROM, TO, &stores).await.unwrap();
        assert_eq!(lagging_followers(&first), vec![FollowerKind::Workspace]);
        assert_eq!(first.report().memory_rows, 1);
        assert_eq!(memory_identity(&after, TO).await, 1);

        std::fs::remove_file(&blocker).unwrap();
        let second = converge(&after, FROM, TO, &stores).await.unwrap();
        assert!(second.is_converged(), "{:?}", second.warnings());
        assert_eq!(
            second.report().memory_rows,
            0,
            "memory had nothing left to move"
        );
        assert_eq!(
            memory_identity(&after, TO).await,
            1,
            "a second rename would have dropped the moved identity as an orphan"
        );
        assert_eq!(memory_identity(&after, FROM).await, 0);
    }

    #[tokio::test]
    async fn a_retried_rename_does_not_merge_memory_it_already_moved() {
        let tmp = TempDir::new().unwrap();
        let before = fixture(&tmp, &[FROM]);
        seed_workspace(&before, FROM);
        seed_memory(&before, FROM, 3).await;
        let after = arm_and_commit(&before).await;
        let blocker = block_destination(&after);
        let stores = SurfaceStores::none();

        let first = converge(&after, FROM, TO, &stores).await.unwrap();
        assert_eq!(lagging_followers(&first), vec![FollowerKind::Workspace]);
        assert_eq!(memory_rows(&after, TO).await, 3);

        std::fs::remove_file(&blocker).unwrap();
        let second = converge(&after, FROM, TO, &stores).await.unwrap();
        assert!(second.is_converged(), "{:?}", second.warnings());
        assert_eq!(memory_rows(&after, TO).await, 3);
        assert_eq!(memory_rows(&after, FROM).await, 0);
    }

    #[tokio::test]
    async fn a_genuine_memory_handle_is_renamed_through_exactly_once() {
        let tmp = TempDir::new().unwrap();
        let before = fixture(&tmp, &[FROM]);
        seed_workspace(&before, FROM);
        let after = arm_and_commit(&before).await;
        let blocker = block_destination(&after);
        let counting = Arc::new(CountingMemory::default());
        counting.agents.lock().insert(FROM.to_string(), 1);
        let memory: Arc<dyn Memory> = counting.clone();
        let stores = SurfaceStores {
            memory: Some(&memory),
            ..SurfaceStores::none()
        };

        let first = converge(&after, FROM, TO, &stores).await.unwrap();
        assert_eq!(lagging_followers(&first), vec![FollowerKind::Workspace]);
        assert_eq!(first.report().memory_rows, 1);

        std::fs::remove_file(&blocker).unwrap();
        let second = converge(&after, FROM, TO, &stores).await.unwrap();
        assert!(second.is_converged(), "{:?}", second.warnings());
        assert_eq!(
            counting.renames.load(Ordering::SeqCst),
            1,
            "a follower with nothing under the old alias is not renamed again"
        );
        assert_eq!(counting.agents.lock().get(TO).copied(), Some(1));
    }

    #[tokio::test]
    async fn a_none_memory_placeholder_does_not_hide_the_configured_store() {
        let tmp = TempDir::new().unwrap();
        let before = fixture(&tmp, &[FROM]);
        seed_memory(&before, FROM, 2).await;
        let after = arm_and_commit(&before).await;
        // What a surface holds when its configured backend failed to build.
        let placeholder: Arc<dyn Memory> = Arc::new(NoneMemory::new("none"));
        let stores = SurfaceStores {
            memory: Some(&placeholder),
            ..SurfaceStores::none()
        };

        let outcome = converge(&after, FROM, TO, &stores).await.unwrap();
        assert!(outcome.is_converged(), "{:?}", outcome.warnings());
        assert_eq!(outcome.report().memory_rows, 1);
        assert_eq!(memory_rows(&after, TO).await, 2);
    }

    #[tokio::test]
    async fn run_history_left_under_the_old_alias_is_found_and_moved() {
        let tmp = TempDir::new().unwrap();
        let after = committed(&fixture(&tmp, &[FROM]));
        // A finished one-shot: its job row is gone, and its run row is still
        // cleanup-owned by the old alias.
        let now = chrono::Utc::now();
        crate::cron::record_run(
            &after,
            "finished-one-shot",
            now,
            now,
            "ok",
            crate::cron::RunOutcomes {
                execution: "ok",
                delivery: "not_required",
                persistence: "not_bound",
            },
            crate::cron::RunProvenance {
                principal: None,
                executing_agent: Some(FROM),
                job_source: Some("imperative"),
            },
            Some("done"),
            1,
        )
        .unwrap();
        let stores = SurfaceStores::none();

        assert_eq!(
            resolve(&after, FROM, TO, &stores).await.unwrap(),
            Disposition::Resume
        );
        let outcome = converge(&after, FROM, TO, &stores).await.unwrap();
        assert!(outcome.is_converged(), "{:?}", outcome.warnings());
        assert_eq!(outcome.report().cron_jobs, 0, "only run history moved");
        assert_eq!(cron_residue(&after, FROM), Some(0));
        assert_eq!(cron_residue(&after, TO), Some(1));
    }

    #[tokio::test]
    async fn invalid_or_reserved_aliases_are_refused_before_touching_disk() {
        let tmp = TempDir::new().unwrap();
        let config = fixture(&tmp, &[FROM]);
        let stores = SurfaceStores::none();

        for (from, to) in [
            ("../x", TO),
            (FROM, "../x"),
            ("default", TO),
            (FROM, "default"),
            (FROM, FROM),
        ] {
            let refusals = [
                resolve(&config, from, to, &stores).await.unwrap_err(),
                arm(&config, from, to).await.unwrap_err(),
                converge(&config, from, to, &stores).await.unwrap_err(),
            ];
            for refusal in refusals {
                let expected = if from == "default" || to == "default" {
                    matches!(refusal, RenameRecoveryError::ReservedAlias { .. })
                } else {
                    matches!(refusal, RenameRecoveryError::InvalidAlias { .. })
                };
                assert!(expected, "{from} -> {to}: {refusal:?}");
            }
        }
        assert!(!config.data_dir.exists(), "no journal or lock was created");
        let agents_dir = config
            .default_agent_workspace_dir(FROM)
            .parent()
            .and_then(Path::parent)
            .unwrap()
            .to_path_buf();
        assert!(
            !agents_dir.exists(),
            "nothing under the agents directory was created"
        );
    }

    #[tokio::test]
    async fn a_configured_legacy_alias_renames_end_to_end() {
        let tmp = TempDir::new().unwrap();
        let before = fixture(&tmp, &[LEGACY]);
        assert!(
            zeroclaw_config::helpers::validate_alias_key(LEGACY).is_err(),
            "the fixture alias must predate the alias grammar"
        );
        let source = seed_workspace(&before, LEGACY);
        seed_memory(&before, LEGACY, 1).await;
        seed_cron(&before, LEGACY);
        seed_acp(&before, "acp-legacy", LEGACY, &source);
        seed_session(&before, "chat-legacy", LEGACY);
        let stores = SurfaceStores::none();

        assert_eq!(
            resolve(&before, LEGACY, MIGRATED, &stores).await.unwrap(),
            Disposition::Fresh
        );
        let armed = arm(&before, LEGACY, MIGRATED).await.unwrap();
        let after = renamed(&before, LEGACY, MIGRATED);
        acknowledge_commit(&after, armed).await;
        assert!(retired_alias(&after, LEGACY).unwrap().is_some());

        let outcome = converge(&after, LEGACY, MIGRATED, &stores).await.unwrap();
        assert!(outcome.is_converged(), "{:?}", outcome.warnings());
        let report = outcome.report();
        assert!(report.workspace_moved);
        assert_eq!(
            (
                report.memory_rows,
                report.cron_jobs,
                report.acp_sessions,
                report.acp_workspaces,
                report.sessions_repointed,
            ),
            (1, 1, 1, 1, 1)
        );
        let destination = after.default_agent_workspace_dir(MIGRATED);
        assert_eq!(
            std::fs::read_to_string(destination.join("MEMORY.md")).unwrap(),
            LEGACY
        );
        assert!(!source.exists());
        assert_eq!(memory_rows(&after, MIGRATED).await, 1);
        assert_eq!(cron_residue(&after, MIGRATED), Some(1));
        assert_eq!(
            acp_row(&after, "acp-legacy"),
            (MIGRATED.to_string(), destination)
        );
        assert_eq!(
            session_owner(&after, "chat-legacy").as_deref(),
            Some(MIGRATED)
        );
        assert!(records(&after).is_empty());
    }

    #[tokio::test]
    async fn a_recorded_rename_of_a_legacy_alias_resumes_after_a_crash() {
        let tmp = TempDir::new().unwrap();
        let before = fixture(&tmp, &[LEGACY]);
        let source = seed_workspace(&before, LEGACY);
        seed_cron(&before, LEGACY);
        let armed = arm(&before, LEGACY, MIGRATED).await.unwrap();
        let after = renamed(&before, LEGACY, MIGRATED);
        acknowledge_commit(&after, armed).await;
        // The process stops here. The old alias is no longer configured, so
        // only the journal record still names it.
        assert!(after.agent(LEGACY).is_none());
        assert!(source.exists());
        let stores = SurfaceStores::none();

        assert_eq!(
            resolve(&after, LEGACY, MIGRATED, &stores).await.unwrap(),
            Disposition::Resume
        );
        let outcome = converge(&after, LEGACY, MIGRATED, &stores).await.unwrap();
        assert!(outcome.is_converged(), "{:?}", outcome.warnings());
        assert!(outcome.report().workspace_moved);
        assert_eq!(outcome.report().cron_jobs, 1);
        assert!(!source.exists());
        assert!(records(&after).is_empty());

        // Once the record is cleared nothing names the old alias, so a request
        // naming it again is held to the alias grammar.
        assert!(matches!(
            resolve(&after, LEGACY, MIGRATED, &stores).await,
            Err(RenameRecoveryError::InvalidAlias { alias, .. }) if alias == LEGACY
        ));
    }

    #[tokio::test]
    async fn a_custom_workspace_never_moves() {
        let tmp = TempDir::new().unwrap();
        let mut before = fixture(&tmp, &[FROM]);
        let custom = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&custom).unwrap();
        std::fs::write(custom.join("MEMORY.md"), "custom").unwrap();
        before.agents.get_mut(FROM).unwrap().workspace.path = Some(custom.clone());
        // A directory at the old alias-derived location is not this agent's
        // workspace.
        let leftover = seed_workspace(&before, FROM);
        let after = committed(&before);
        assert_eq!(after.agent_workspace_dir(TO), custom);
        let stores = SurfaceStores::none();

        let err = resolve(&after, FROM, TO, &stores).await.unwrap_err();
        assert!(
            matches!(err, RenameRecoveryError::NotConfigured { .. }),
            "{err:?}"
        );
        assert!(!after.data_dir.exists());

        // A recorded rename of the same agent converges without moving either.
        let armed = arm(&before, FROM, TO).await.unwrap();
        acknowledge_commit(&after, armed).await;
        assert_eq!(records(&after)[0].source_workspace, None);
        let outcome = converge(&after, FROM, TO, &stores).await.unwrap();
        assert!(outcome.is_converged(), "{:?}", outcome.warnings());
        assert!(!outcome.report().workspace_moved);
        assert_eq!(
            std::fs::read_to_string(custom.join("MEMORY.md")).unwrap(),
            "custom"
        );
        assert!(leftover.exists());
        assert!(!after.default_agent_workspace_dir(TO).exists());
    }

    #[tokio::test]
    async fn a_blocked_destination_keeps_the_record_until_the_workspace_settles() {
        let tmp = TempDir::new().unwrap();
        let before = fixture(&tmp, &[FROM]);
        let source = seed_workspace(&before, FROM);
        seed_cron(&before, FROM);
        seed_acp(&before, "acp-1", FROM, &source);
        let after = arm_and_commit(&before).await;
        let blocker = block_destination(&after);
        let stores = SurfaceStores::none();

        let outcome = converge(&after, FROM, TO, &stores).await.unwrap();
        assert_eq!(
            lagging_followers(&outcome),
            vec![FollowerKind::Workspace, FollowerKind::Acp]
        );
        assert_eq!(outstanding(&outcome)[0].kind, FollowerIssueKind::Unreadable);
        assert_eq!(
            outstanding(&outcome)[1].to_string(),
            format!("acp still attributes state to `{FROM}`")
        );
        assert_eq!(outcome.warnings().len(), 2);
        assert!(!outcome.report().workspace_moved);
        assert_eq!(outcome.report().cron_jobs, 1, "the other followers moved");
        assert_eq!(
            acp_row(&after, "acp-1"),
            (TO.to_string(), source.clone()),
            "the session keeps the directory the workspace is still in"
        );
        assert!(source.exists());
        assert_eq!(records(&after).len(), 1, "the record stays open");

        // Clearing the leftovers by hand does not clear the record.
        std::fs::remove_file(&blocker).unwrap();
        std::fs::remove_dir_all(&source).unwrap();
        assert!(retired_alias(&after, FROM).unwrap().is_some());

        let retried = converge(&after, FROM, TO, &stores).await.unwrap();
        assert!(retried.is_converged(), "{:?}", retried.warnings());
        assert_eq!(retried.report().acp_workspaces, 1);
        assert_eq!(
            acp_row(&after, "acp-1"),
            (TO.to_string(), after.default_agent_workspace_dir(TO))
        );
        assert!(records(&after).is_empty());
        assert_eq!(retired_alias(&after, FROM).unwrap(), None);
    }

    #[tokio::test]
    async fn a_non_empty_destination_conflicts_and_an_empty_one_is_replaced() {
        let tmp = TempDir::new().unwrap();
        let before = fixture(&tmp, &[FROM]);
        let source = seed_workspace(&before, FROM);
        let after = arm_and_commit(&before).await;
        let destination = after.default_agent_workspace_dir(TO);
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(destination.join("keep.md"), "theirs").unwrap();
        let stores = SurfaceStores::none();

        let outcome = converge(&after, FROM, TO, &stores).await.unwrap();
        let issues = outstanding(&outcome);
        assert_eq!(issues.len(), 1, "{:?}", outcome.warnings());
        assert_eq!(issues[0].kind, FollowerIssueKind::Conflict);
        assert_eq!(
            issues[0].to_string(),
            format!(
                "workspace destination {} already exists and is not empty",
                destination.display()
            )
        );
        assert!(source.exists());
        assert_eq!(
            std::fs::read_to_string(destination.join("keep.md")).unwrap(),
            "theirs"
        );

        std::fs::remove_file(destination.join("keep.md")).unwrap();
        let retried = converge(&after, FROM, TO, &stores).await.unwrap();
        assert!(retried.is_converged(), "{:?}", retried.warnings());
        assert!(retried.report().workspace_moved);
        assert_eq!(
            std::fs::read_to_string(destination.join("MEMORY.md")).unwrap(),
            FROM
        );
        assert!(!source.exists());
    }

    #[tokio::test]
    async fn an_open_record_guards_both_of_its_aliases() {
        let tmp = TempDir::new().unwrap();
        let before = fixture(&tmp, &[FROM]);
        let after = arm_and_commit(&before).await;
        let stores = SurfaceStores::none();

        // Renaming the pending target away, or onto the retired alias, or the
        // retired alias anywhere else, is refused.
        let pending = resolve(&after, TO, "wolf", &stores).await.unwrap_err();
        assert!(
            matches!(&pending, RenameRecoveryError::RecoveryPending { from, to } if from == FROM && to == TO),
            "{pending:?}"
        );
        assert!(!pending.to_string().contains(FROM), "{pending}");
        let onto = resolve(&after, "owl", FROM, &stores).await.unwrap_err();
        assert!(
            matches!(&onto, RenameRecoveryError::AliasRetired { alias, pending_to } if alias == FROM && pending_to == TO),
            "{onto:?}"
        );
        assert!(!onto.to_string().contains(TO), "{onto}");
        let elsewhere = resolve(&after, FROM, "wolf", &stores).await.unwrap_err();
        assert!(
            matches!(&elsewhere, RenameRecoveryError::AliasRetired { alias, .. } if alias == FROM),
            "{elsewhere:?}"
        );

        // Renaming another agent into the pending target is refused as well.
        let mut with_owl = after.clone();
        with_owl.agents.insert("owl".to_string(), agent());
        assert!(matches!(
            resolve(&with_owl, "owl", TO, &stores).await,
            Err(RenameRecoveryError::RecoveryPending { .. })
        ));

        // The creation and deletion guards agree, and so do arm and converge.
        assert!(matches!(
            ensure_not_pending_target(&after, TO).await,
            Err(RenameRecoveryError::RecoveryPending { .. })
        ));
        assert!(matches!(
            ensure_alias_not_retired(&after, FROM).await,
            Err(RenameRecoveryError::AliasRetired { .. })
        ));
        assert!(ensure_alias_not_retired(&after, TO).await.is_ok());
        assert!(ensure_not_pending_target(&after, FROM).await.is_ok());
        assert!(ensure_alias_not_retired(&after, "owl").await.is_ok());
        assert!(matches!(
            arm(&before, FROM, "wolf").await,
            Err(RenameRecoveryError::AliasRetired { .. })
        ));
        assert!(matches!(
            converge(&after, FROM, "wolf", &stores).await,
            Err(RenameRecoveryError::AliasRetired { .. })
        ));
        assert_eq!(records(&after).len(), 1, "no refusal touched the record");
    }

    #[tokio::test]
    async fn an_abandoned_rename_leaves_no_record_and_moves_nothing() {
        let tmp = TempDir::new().unwrap();
        let before = fixture(&tmp, &[FROM]);
        let source = seed_workspace(&before, FROM);

        let armed = arm(&before, FROM, TO).await.unwrap();
        // While one surface is between its arm and its commit, no other can arm.
        assert!(matches!(
            arm(&before, FROM, "wolf").await,
            Err(RenameRecoveryError::Busy { .. })
        ));
        abandon(&before, armed).await;

        assert!(records(&before).is_empty());
        assert!(!AgentRecoveryJournal::for_config(&before).path().exists());
        assert!(source.exists());
        assert_eq!(retired_alias(&before, FROM).unwrap(), None);
        assert_eq!(
            resolve(&before, FROM, TO, &SurfaceStores::none())
                .await
                .unwrap(),
            Disposition::Fresh
        );
    }

    #[tokio::test]
    async fn acknowledge_and_abandon_defer_to_the_config_they_are_given() {
        let tmp = TempDir::new().unwrap();
        let before = fixture(&tmp, &[FROM]);

        // Acknowledged against a config still holding the old alias: the
        // record stays prepared and retires nothing.
        let armed = arm(&before, FROM, TO).await.unwrap();
        acknowledge_commit(&before, armed).await;
        assert_eq!(records(&before)[0].phase, RecoveryPhase::Prepared);
        assert_eq!(retired_alias(&before, FROM).unwrap(), None);

        // Abandoned against a config that shows the commit: the record stays,
        // so the rename can still be finished.
        let armed = arm(&before, FROM, TO).await.unwrap();
        let after = committed(&before);
        abandon(&after, armed).await;
        assert_eq!(records(&after).len(), 1);
        assert!(retired_alias(&after, FROM).unwrap().is_some());
    }

    #[tokio::test]
    async fn converge_refuses_a_rename_whose_commit_never_landed() {
        let tmp = TempDir::new().unwrap();
        let before = fixture(&tmp, &[FROM]);
        let source = seed_workspace(&before, FROM);
        let stores = SurfaceStores::none();

        assert!(matches!(
            converge(&before, FROM, TO, &stores).await,
            Err(RenameRecoveryError::InvalidAlias { .. })
        ));
        let armed = arm(&before, FROM, TO).await.unwrap();
        assert!(matches!(
            converge(&before, FROM, TO, &stores).await,
            Err(RenameRecoveryError::InvalidAlias { .. })
        ));
        abandon(&before, armed).await;
        assert!(source.exists());
    }

    #[tokio::test]
    async fn acp_rows_follow_every_spelling_of_the_moved_workspace() {
        let tmp = TempDir::new().unwrap();
        let real_install = tmp.path().join("install");
        std::fs::create_dir_all(&real_install).unwrap();
        // On unix the install root is reached through a symlink, so a row that
        // recorded the resolved path spells the workspace differently. Windows
        // resolves to a verbatim `\\?\` path, which differs as well.
        #[cfg(unix)]
        let install = {
            let link = tmp.path().join("install-link");
            std::os::unix::fs::symlink(&real_install, &link).unwrap();
            link
        };
        #[cfg(not(unix))]
        let install = real_install;
        let mut before = fixture(&tmp, &[FROM]);
        before.config_path = install.join("config.toml");
        let source = seed_workspace(&before, FROM);
        let canonical = std::fs::canonicalize(&source).unwrap();
        assert_ne!(canonical, source, "the workspace must have two spellings");
        let custom = tmp.path().join("elsewhere");
        seed_acp(&before, "raw", FROM, &source);
        seed_acp(&before, "canonical", FROM, &canonical);
        seed_acp(&before, "nested", "other", &source.join("proj"));
        seed_acp(&before, "custom", FROM, &custom);

        let after = arm_and_commit(&before).await;
        let outcome = converge(&after, FROM, TO, &SurfaceStores::none())
            .await
            .unwrap();
        assert!(outcome.is_converged(), "{:?}", outcome.warnings());
        assert_eq!(outcome.report().acp_sessions, 3);
        assert_eq!(outcome.report().acp_workspaces, 3);

        let destination = after.default_agent_workspace_dir(TO);
        assert_eq!(
            acp_row(&after, "raw"),
            (TO.to_string(), destination.clone())
        );
        assert_eq!(
            acp_row(&after, "canonical"),
            (TO.to_string(), destination.clone())
        );
        assert_eq!(
            acp_row(&after, "nested"),
            ("other".to_string(), destination.join("proj"))
        );
        assert_eq!(acp_row(&after, "custom"), (TO.to_string(), custom));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn acp_rows_follow_a_workspace_an_earlier_run_already_moved() {
        let tmp = TempDir::new().unwrap();
        let real_install = tmp.path().join("install");
        std::fs::create_dir_all(&real_install).unwrap();
        let install = tmp.path().join("install-link");
        std::os::unix::fs::symlink(&real_install, &install).unwrap();
        let mut before = fixture(&tmp, &[FROM]);
        before.config_path = install.join("config.toml");
        let source = seed_workspace(&before, FROM);
        let canonical = std::fs::canonicalize(&source).unwrap();
        seed_acp(&before, "canonical", FROM, &canonical.join("proj"));
        let after = arm_and_commit(&before).await;

        // An earlier run moved the workspace and removed the old alias
        // directory, then stopped before the ACP rows followed.
        let destination = after.default_agent_workspace_dir(TO);
        std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
        std::fs::rename(&source, &destination).unwrap();
        std::fs::remove_dir(source.parent().unwrap()).unwrap();

        let outcome = converge(&after, FROM, TO, &SurfaceStores::none())
            .await
            .unwrap();
        assert!(outcome.is_converged(), "{:?}", outcome.warnings());
        assert!(!outcome.report().workspace_moved);
        assert_eq!(
            acp_row(&after, "canonical"),
            (TO.to_string(), destination.join("proj"))
        );
    }

    #[tokio::test]
    async fn converging_a_converged_rename_again_changes_nothing() {
        let tmp = TempDir::new().unwrap();
        let before = fixture(&tmp, &[FROM]);
        seed_every_follower(&before).await;
        let after = arm_and_commit(&before).await;
        let stores = SurfaceStores::none();
        let first = converge(&after, FROM, TO, &stores).await.unwrap();
        assert!(first.is_converged(), "{:?}", first.warnings());
        let destination = after.default_agent_workspace_dir(TO);

        let again = converge(&after, FROM, TO, &stores).await.unwrap();
        assert!(again.is_converged(), "{:?}", again.warnings());
        let report = again.report();
        assert!(!report.workspace_moved);
        assert_eq!(
            (
                report.memory_rows,
                report.cron_jobs,
                report.acp_sessions,
                report.acp_workspaces,
                report.sessions_repointed,
            ),
            (0, 0, 0, 0, 0)
        );
        assert_eq!(
            std::fs::read_to_string(destination.join("MEMORY.md")).unwrap(),
            FROM
        );
        assert_eq!(memory_rows(&after, TO).await, 2);
        assert_eq!(cron_residue(&after, TO), Some(1));
        assert_eq!(acp_row(&after, "acp-1"), (TO.to_string(), destination));
        assert_eq!(session_owner(&after, "chat-1").as_deref(), Some(TO));
        assert!(records(&after).is_empty());
    }

    #[tokio::test]
    async fn a_corrupt_journal_fails_every_entry_point_closed() {
        let tmp = TempDir::new().unwrap();
        let config = fixture(&tmp, &[FROM]);
        let journal = AgentRecoveryJournal::for_config(&config);
        std::fs::create_dir_all(&config.data_dir).unwrap();
        std::fs::write(journal.path(), "{not json").unwrap();
        let stores = SurfaceStores::none();

        let err = resolve(&config, FROM, TO, &stores).await.unwrap_err();
        assert!(
            matches!(&err, RenameRecoveryError::Unreadable { store, .. } if store == JOURNAL_STORE),
            "{err:?}"
        );
        assert!(matches!(
            arm(&config, FROM, TO).await,
            Err(RenameRecoveryError::Unreadable { .. })
        ));
        assert!(matches!(
            converge(&config, FROM, TO, &stores).await,
            Err(RenameRecoveryError::Unreadable { .. })
        ));
        assert!(matches!(
            ensure_alias_not_retired(&config, FROM).await,
            Err(RenameRecoveryError::Unreadable { .. })
        ));
        assert!(matches!(
            ensure_not_pending_target(&config, TO).await,
            Err(RenameRecoveryError::Unreadable { .. })
        ));
        assert_eq!(
            std::fs::read_to_string(journal.path()).unwrap(),
            "{not json"
        );
    }

    #[test]
    fn warning_lines_and_errors_keep_their_shapes() {
        let names: Vec<String> = FollowerKind::ALL.iter().map(ToString::to_string).collect();
        assert_eq!(names, ["workspace", "memory", "cron", "acp", "sessions"]);

        let unreadable = FollowerIssue::unreadable(FollowerKind::Cron, "not a database".into());
        assert_eq!(
            unreadable.to_string(),
            "cron could not be read: not a database"
        );
        assert_eq!(
            serde_json::to_value(&unreadable).unwrap(),
            serde_json::json!({
                "follower": "cron",
                "kind": "unreadable",
                "detail": "not a database",
            })
        );
        let lagging = FollowerIssue::lagging(FollowerKind::Memory, "memory rename: boom".into());
        assert_eq!(lagging.to_string(), "memory rename: boom");

        let outcome = ConvergeOutcome::Incomplete {
            report: ConvergeReport::default(),
            outstanding: vec![unreadable, lagging],
        };
        assert!(!outcome.is_converged());
        assert_eq!(
            outcome.warnings(),
            [
                "cron could not be read: not a database",
                "memory rename: boom"
            ]
        );

        let error = |e: RenameRecoveryError| e.to_string();
        assert_eq!(
            error(RenameRecoveryError::NotConfigured { alias: FROM.into() }),
            "agents.scout is not configured"
        );
        assert_eq!(
            error(RenameRecoveryError::AliasRetired {
                alias: FROM.into(),
                pending_to: TO.into(),
            }),
            "alias `scout` is retired by an unfinished agent rename and cannot be reused yet"
        );
        assert_eq!(
            error(RenameRecoveryError::RecoveryPending {
                from: FROM.into(),
                to: TO.into(),
            }),
            "agent `ranger` is the target of an unfinished rename; re-run that rename first"
        );
        assert_eq!(
            error(RenameRecoveryError::Unreadable {
                store: "acp".into(),
                detail: "locked".into(),
            }),
            "acp could not be read: locked"
        );
        assert_eq!(
            error(RenameRecoveryError::Busy {
                detail: "held".into()
            }),
            "agent rename recovery is in progress elsewhere; retry shortly (held)"
        );
        assert_eq!(
            error(RenameRecoveryError::Persist {
                detail: "full".into()
            }),
            "agent rename recovery could not be recorded: full"
        );
    }

    /// Surfaces await these inside handler futures with a 16 KiB budget, so
    /// each must be `Send` and leave most of that budget to the handler.
    #[test]
    fn entry_points_are_send_and_their_futures_stay_small() {
        fn send<T: Send>(value: T) -> T {
            value
        }
        send(Option::<Armed>::None);
        let config = Config::default();
        let stores = SurfaceStores::none();
        let sizes = [
            (
                "resolve",
                std::mem::size_of_val(&send(resolve(&config, FROM, TO, &stores))),
            ),
            ("arm", std::mem::size_of_val(&send(arm(&config, FROM, TO)))),
            (
                "converge",
                std::mem::size_of_val(&send(converge(&config, FROM, TO, &stores))),
            ),
            (
                "ensure_alias_not_retired",
                std::mem::size_of_val(&send(ensure_alias_not_retired(&config, FROM))),
            ),
            (
                "ensure_not_pending_target",
                std::mem::size_of_val(&send(ensure_not_pending_target(&config, TO))),
            ),
        ];
        for (name, size) in sizes {
            assert!(size <= 4 * 1024, "the {name} future is {size} bytes");
        }
    }
}
