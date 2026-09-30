//! CLI for alias CRUD: `zeroclaw {agents,providers,channels}
//! {create,list,rename,delete}`.

use anyhow::{Context, Result, bail};
use zeroclaw::{AgentsCommands, ChannelsCommands, ProvidersCommands};
use zeroclaw_config::alias_refs::{
    self, AliasKind, CascadeError, CascadePolicy, ProviderCategory, RenameError,
};
use zeroclaw_config::schema::Config;
#[cfg(feature = "agent-runtime")]
use zeroclaw_runtime::agent_rename_recovery::{
    self as rename_recovery, ConvergeOutcome, ConvergeReport, Disposition, RenameRecoveryError,
    SurfaceStores,
};

mod export;

/// Resolve a `cli-*` Fluent key for alias-CRUD CLI output. Under `agent-runtime`
/// (default + what CI/release build) this routes through Fluent; without it the
/// runtime i18n crate is absent, so the English `fallback` is used.
#[allow(unused_variables)]
fn mt(key: &str, fallback: &str) -> String {
    #[cfg(feature = "agent-runtime")]
    {
        zeroclaw_runtime::i18n::get_required_cli_string(key)
    }
    #[cfg(not(feature = "agent-runtime"))]
    {
        fallback.to_string() // i18n-exempt: English fallback when Fluent (agent-runtime) is disabled
    }
}

/// `mt` with `{$name}` arguments.
#[allow(unused_variables)]
fn mta(key: &str, args: &[(&str, &str)], fallback: &str) -> String {
    #[cfg(feature = "agent-runtime")]
    {
        zeroclaw_runtime::i18n::get_required_cli_string_with_args(key, args)
    }
    #[cfg(not(feature = "agent-runtime"))]
    {
        fallback.to_string() // i18n-exempt: English fallback when Fluent (agent-runtime) is disabled
    }
}

fn parse_provider_category(category: &str) -> Result<ProviderCategory> {
    match category {
        "models" => Ok(ProviderCategory::Models),
        "tts" => Ok(ProviderCategory::Tts),
        "transcription" => Ok(ProviderCategory::Transcription),
        other => bail!(
            "{}",
            mta(
                "cli-alias-unknown-provider-category",
                &[("category", other)],
                "unknown provider category `{$category}` (expected models | tts | transcription)"
            )
        ),
    }
}

/// The map-key section path for a kind (e.g. `agents`, `providers.models.anthropic`,
/// `channels.discord`).
fn section_path(kind: &AliasKind) -> String {
    match kind {
        AliasKind::Agent => "agents".to_string(),
        AliasKind::Provider { category, family } => {
            let cat = match category {
                ProviderCategory::Models => "models",
                ProviderCategory::Tts => "tts",
                ProviderCategory::Transcription => "transcription",
            };
            format!("providers.{cat}.{family}")
        }
        AliasKind::Channel { channel_type } => format!("channels.{channel_type}"),
    }
}

fn list_section(config: &Config, section: &str) -> Result<()> {
    match config.get_map_keys(section) {
        Some(mut keys) => {
            keys.sort();
            if keys.is_empty() {
                println!(
                    "{}",
                    mta(
                        "cli-alias-list-empty",
                        &[("section", section)],
                        "(no entries under {$section})"
                    )
                );
            } else {
                for k in keys {
                    println!("{k}");
                }
            }
        }
        None => bail!(
            "{}",
            mta(
                "cli-alias-no-such-section",
                &[("section", section)],
                "no such config section: {$section}"
            )
        ),
    }
    Ok(())
}

fn create_entry(config: &mut Config, section: &str, alias: &str) -> Result<()> {
    // Shared guarded boundary: refuses the reserved `default` agent here too (an
    // operator create surface), and delegates unchanged for every other section.
    // The Reserved rejection and the recovery-journal refusals are localized via
    // Fluent like the delete/rename guards below; Invalid (unknown section) keeps
    // its pre-existing bare error. A retired alias names the rename that must
    // finish before the alias can be reused.
    let created = match alias_refs::create_map_key_checked(config, section, alias) {
        Ok(created) => created,
        Err(alias_refs::CreateError::Reserved(_)) => bail!(
            "{}",
            mt(
                "cli-alias-create-reserved-default",
                "the `default` agent is reserved and cannot be created"
            )
        ),
        Err(alias_refs::CreateError::Retired { alias, pending_to }) => bail!(
            "{}",
            mta(
                "cli-alias-create-retired",
                &[("alias", alias.as_str()), ("to", pending_to.as_str())],
                "alias `{$alias}` is retired by an unfinished rename to `{$to}`; run `zeroclaw agents rename {$alias} {$to}` first"
            )
        ),
        Err(alias_refs::CreateError::RecoveryUnreadable(detail)) => bail!(
            "{}",
            mta(
                "cli-alias-recovery-unreadable",
                &[("error", detail.as_str())],
                "agent lifecycle recovery journal could not be read: {$error}"
            )
        ),
        Err(alias_refs::CreateError::Invalid(msg)) => return Err(anyhow::Error::msg(msg)),
    };
    if created {
        config.mark_dirty(&format!("{section}.{alias}"));
        println!(
            "{}",
            mta(
                "cli-alias-created",
                &[("section", section), ("alias", alias)],
                "created {$section}.{$alias}"
            )
        );
    } else {
        println!(
            "{}",
            mta(
                "cli-alias-exists",
                &[("section", section), ("alias", alias)],
                "{$section}.{$alias} already exists (no change)"
            )
        );
    }
    Ok(())
}

/// Print the dry-run impact (blockers + scrubs) for a delete.
fn print_impact(kind: &AliasKind, alias: &str, config: &Config) {
    let report = alias_refs::plan_delete(config, kind, alias);
    let section = section_path(kind);
    if report.blockers.is_empty() {
        let count = report.scrubs.len().to_string();
        println!(
            "{}",
            mta(
                "cli-alias-impact-scrub-header",
                &[
                    ("section", section.as_str()),
                    ("alias", alias),
                    ("count", count.as_str())
                ],
                "deleting {$section}.{$alias} would scrub {$count} reference(s):"
            )
        );
    } else {
        let count = report.blockers.len().to_string();
        println!(
            "{}",
            mta(
                "cli-alias-impact-blocked-header",
                &[
                    ("section", section.as_str()),
                    ("alias", alias),
                    ("count", count.as_str())
                ],
                "deleting {$section}.{$alias} is BLOCKED by {$count} hard reference(s):"
            )
        );
        for b in &report.blockers {
            println!(
                "  {}",
                mta(
                    "cli-alias-impact-blocker",
                    &[("path", b.path.as_str())],
                    "✗ {$path} (hard reference)"
                )
            );
        }
    }
    for s in &report.scrubs {
        println!(
            "  {}",
            mta(
                "cli-alias-impact-scrub",
                &[("path", s.path.as_str())],
                "• {$path} (would be scrubbed)"
            )
        );
    }
}

/// Delete an aliased entry's config references (config-layer only).
fn delete_config(
    config: &mut Config,
    kind: &AliasKind,
    alias: &str,
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    let section = section_path(kind);
    if dry_run {
        print_impact(kind, alias, config);
        return Ok(());
    }
    if !yes {
        print_impact(kind, alias, config);
        println!(
            "\n{}",
            mt(
                "cli-alias-no-changes",
                "No changes made. Re-run with --yes to apply (or --dry-run to preview)."
            )
        );
        return Ok(());
    }
    apply_delete(config, kind, alias)
}

/// Apply the config-layer delete (scrub refs + remove entry) and mark the dirty
/// paths. Bails on a hard-ref refusal or a missing alias. The caller persists.
fn apply_delete(config: &mut Config, kind: &AliasKind, alias: &str) -> Result<()> {
    let section = section_path(kind);
    match alias_refs::delete_with_cascade(config, kind, alias, CascadePolicy::RefuseOnHard) {
        Ok(report) => {
            for path in report.dirty_paths() {
                config.mark_dirty(&path);
            }
            let count = report.applied.len().to_string();
            println!(
                "{}",
                mta(
                    "cli-alias-deleted",
                    &[
                        ("section", section.as_str()),
                        ("alias", alias),
                        ("count", count.as_str())
                    ],
                    "deleted {$section}.{$alias} (scrubbed {$count} reference(s))"
                )
            );
            Ok(())
        }
        Err(CascadeError::Refused(report)) => {
            let count = report.blockers.len().to_string();
            println!(
                "{}",
                mta(
                    "cli-alias-delete-refused-header",
                    &[("count", count.as_str())],
                    "refused: {$count} hard reference(s) block the delete:"
                )
            );
            for b in &report.blockers {
                println!("  ✗ {}", b.path);
            }
            bail!(
                "{}",
                mt(
                    "cli-alias-delete-refused-hint",
                    "delete refused — resolve the hard references first"
                )
            );
        }
        Err(CascadeError::NotFound(p)) => bail!(
            "{}",
            mta(
                "cli-alias-not-configured",
                &[("path", p.as_str())],
                "{$path} is not configured"
            )
        ),
        Err(e) => {
            let es = e.to_string();
            bail!(
                "{}",
                mta(
                    "cli-alias-delete-failed",
                    &[("error", es.as_str())],
                    "delete failed: {$error}"
                )
            )
        }
    }
}

/// Rename an aliased entry's config references (config-layer only).
fn rename_config(config: &mut Config, kind: &AliasKind, from: &str, to: &str) -> Result<()> {
    match alias_refs::rename_with_cascade(config, kind, from, to) {
        Ok(report) => {
            for path in &report.dirty_paths {
                config.mark_dirty(path);
            }
            let section = section_path(kind);
            let count = report.dirty_paths.len().to_string();
            println!(
                "{}",
                mta(
                    "cli-alias-renamed",
                    &[
                        ("section", section.as_str()),
                        ("from", from),
                        ("to", to),
                        ("count", count.as_str())
                    ],
                    "renamed {$section}.{$from} → {$section}.{$to} (rewrote {$count} reference path(s))"
                )
            );
            Ok(())
        }
        Err(RenameError::NotFound(p)) => bail!(
            "{}",
            mta(
                "cli-alias-not-configured",
                &[("path", p.as_str())],
                "{$path} is not configured"
            )
        ),
        Err(RenameError::InvalidName(m)) => bail!(
            "{}",
            mta(
                "cli-alias-rename-invalid",
                &[("message", m.as_str())],
                "invalid new alias: {$message}"
            )
        ),
        Err(RenameError::Reserved(a)) => bail!(
            "{}",
            mta(
                "cli-alias-rename-reserved",
                &[("alias", a.as_str())],
                "alias `{$alias}` is reserved and cannot be renamed"
            )
        ),
        Err(RenameError::PostCondition(m)) => bail!(
            "{}",
            mta(
                "cli-alias-rename-postcondition",
                &[("message", m.as_str())],
                "rename cascade post-condition failed: {$message}"
            )
        ),
    }
}

async fn save(config: &mut Config) -> Result<()> {
    Box::pin(config.save_dirty())
        .await
        .context("failed to persist config")
}

// ── agents ──────────────────────────────────────────────────────────────────

pub async fn handle_agents(cmd: AgentsCommands, config: &mut Config) -> Result<()> {
    match cmd {
        AgentsCommands::List => list_section(config, "agents"),
        AgentsCommands::Create { alias } => {
            create_entry(config, "agents", &alias)?;
            save(config).await
        }
        AgentsCommands::Export { alias, out, force } => {
            export::run(config, &alias, &out, force).await
        }
        AgentsCommands::Rename { from, to } => Box::pin(rename_agent(config, &from, &to)).await,
        AgentsCommands::Delete {
            alias,
            dry_run,
            yes,
        } => {
            if alias_refs::is_reserved_agent_alias(&alias) {
                bail!(
                    "{}",
                    mt(
                        "cli-alias-delete-reserved-default",
                        "the `default` agent is reserved and cannot be deleted"
                    )
                );
            }
            if dry_run {
                print_impact(&AliasKind::Agent, &alias, config);
                return Ok(());
            }
            if !yes {
                print_impact(&AliasKind::Agent, &alias, config);
                println!(
                    "\n{}",
                    mt(
                        "cli-alias-no-changes",
                        "No changes made. Re-run with --yes to apply (or --dry-run to preview)."
                    )
                );
                return Ok(());
            }
            // Owned-state HARD gate (live ACP sessions) runs BEFORE the config
            // cascade so a refusal mutates nothing.
            agent_delete_precheck(config, &alias)?;
            // An unfinished rename still owes state moves out of its old alias
            // and into its target. Deleting either would strand that state, so
            // both refuse until the rename converges.
            #[cfg(feature = "agent-runtime")]
            agent_delete_recovery_guard(config, &alias).await?;
            // Resolve the workspace dir while the entry still exists (a custom
            // `workspace.path` is read off it), then apply + PERSIST the config
            // change before any irreversible owned-state side effects — so a
            // later failure can't leave the config and owned state split.
            let workspace = config.agent_workspace_dir(&alias);
            apply_delete(config, &AliasKind::Agent, &alias)?;
            save(config).await?;
            agent_delete_owned_state(config, &alias, &workspace).await
        }
    }
}

/// Rename an agent through the recovery contract every rename surface shares.
///
/// A fresh rename is recorded before its config commit, and the state kept
/// under the old alias follows once the commit lands. Re-running a rename that
/// did not finish resumes it instead of reporting the old alias missing, and
/// the command fails until every follower has converged.
#[cfg(feature = "agent-runtime")]
async fn rename_agent(config: &mut Config, from: &str, to: &str) -> Result<()> {
    // The CLI holds no store handles: the contract opens each follower's store
    // from the config and what exists on disk.
    let stores = SurfaceStores::none();
    match rename_recovery::resolve(config, from, to, &stores)
        .await
        .map_err(rename_refused)?
    {
        Disposition::Fresh => {
            let armed = rename_recovery::arm(config, from, to)
                .await
                .map_err(rename_refused)?;
            // Commit on a working copy, so `config` shows the rename only once
            // the save has landed.
            let mut working = config.clone();
            let committed = match rename_config(&mut working, &AliasKind::Agent, from, to) {
                Ok(()) => save(&mut working).await,
                Err(e) => Err(e),
            };
            if let Err(e) = committed {
                rename_recovery::abandon(config, armed).await;
                return Err(e);
            }
            *config = working;
            rename_recovery::acknowledge_commit(config, armed).await;
        }
        Disposition::Resume => println!(
            "{}",
            mta(
                "cli-alias-rename-resuming",
                &[("from", from), ("to", to)],
                "resuming the unfinished rename of {$from} to {$to}"
            )
        ),
    }

    match rename_recovery::converge(config, from, to, &stores).await {
        Ok(ConvergeOutcome::Converged(report)) => {
            print_repointed(&report);
            Ok(())
        }
        Ok(ConvergeOutcome::Incomplete {
            report,
            outstanding,
        }) => {
            print_repointed(&report);
            for issue in &outstanding {
                let warning = issue.to_string();
                eprintln!(
                    "{}",
                    mta(
                        "cli-alias-warn",
                        &[("warning", warning.as_str())],
                        "warning: {$warning}"
                    )
                );
            }
            bail!(
                "{}",
                mta(
                    "cli-alias-rename-recovery-incomplete",
                    &[("from", from), ("to", to)],
                    "agent rename did not finish; re-run `zeroclaw agents rename {$from} {$to}` to converge"
                )
            )
        }
        Err(e) => Err(rename_recovery_failed(&e)),
    }
}

/// Without the agent runtime there is no recovery contract: the config rename
/// is saved and the operator is told the owned state did not follow.
#[cfg(not(feature = "agent-runtime"))]
async fn rename_agent(config: &mut Config, from: &str, to: &str) -> Result<()> {
    rename_config(config, &AliasKind::Agent, from, to)?;
    save(config).await?;
    warn_agent_owned_state();
    Ok(())
}

/// Report what one converge moved. ACP counts the sessions re-attributed to
/// the new alias; a session whose saved working directory moved with the
/// workspace is the same session, so it is not counted twice.
#[cfg(feature = "agent-runtime")]
fn print_repointed(report: &ConvergeReport) {
    let memory = report.memory_rows.to_string();
    let cron = report.cron_jobs.to_string();
    let acp = report.acp_sessions.to_string();
    let sessions = report.sessions_repointed.to_string();
    println!(
        "{}",
        mta(
            "cli-alias-owned-repointed",
            &[
                ("memory", memory.as_str()),
                ("cron", cron.as_str()),
                ("acp", acp.as_str()),
                ("sessions", sessions.as_str())
            ],
            "owned-state re-pointed: memory {$memory} · cron {$cron} · acp {$acp} · sessions {$sessions}"
        )
    );
}

/// Localize why the rename recovery contract refused a rename. A refusal an
/// unfinished rename decided names the rename to re-run, and a store or
/// journal that could not be read never reads as a missing alias: the same
/// rename can be retried once it is readable.
#[cfg(feature = "agent-runtime")]
fn rename_refused(error: RenameRecoveryError) -> anyhow::Error {
    let message = match &error {
        RenameRecoveryError::NotConfigured { alias } => {
            let path = format!("{}.{alias}", section_path(&AliasKind::Agent));
            mta(
                "cli-alias-not-configured",
                &[("path", path.as_str())],
                "{$path} is not configured",
            )
        }
        // Either alias can be the malformed one, so the text names it instead
        // of assuming the new alias like the config-only rename does.
        RenameRecoveryError::InvalidAlias { alias, reason } => mta(
            "cli-alias-rename-agent-invalid",
            &[("alias", alias.as_str()), ("message", reason.as_str())],
            "invalid agent alias `{$alias}`: {$message}",
        ),
        RenameRecoveryError::ReservedAlias { alias } => mta(
            "cli-alias-rename-reserved",
            &[("alias", alias.as_str())],
            "alias `{$alias}` is reserved and cannot be renamed",
        ),
        RenameRecoveryError::AliasRetired { alias, pending_to } => mta(
            "cli-alias-rename-retired",
            &[("alias", alias.as_str()), ("to", pending_to.as_str())],
            "alias `{$alias}` is retired by an unfinished rename to `{$to}`; run `zeroclaw agents rename {$alias} {$to}` first",
        ),
        RenameRecoveryError::RecoveryPending { from, to } => mta(
            "cli-alias-rename-pending-target",
            &[("from", from.as_str()), ("to", to.as_str())],
            "agent `{$to}` is the target of an unfinished rename from `{$from}`; run `zeroclaw agents rename {$from} {$to}` first",
        ),
        RenameRecoveryError::SourceReconfigured { .. } => error.to_string(),
        RenameRecoveryError::Unreadable { .. }
        | RenameRecoveryError::Busy { .. }
        | RenameRecoveryError::Persist { .. } => return rename_recovery_failed(&error),
    };
    anyhow::Error::msg(message)
}

/// A recovery step that failed rather than refused: a store or the recovery
/// journal could not be read or written, or another process holds the journal.
#[cfg(feature = "agent-runtime")]
fn rename_recovery_failed(error: &RenameRecoveryError) -> anyhow::Error {
    let detail = error.to_string();
    anyhow::Error::msg(mta(
        "cli-alias-rename-recovery-error",
        &[("error", detail.as_str())],
        "agent rename recovery failed: {$error}",
    ))
}

/// Refuse to delete an alias an unfinished rename retired, or one it is still
/// converging into.
#[cfg(feature = "agent-runtime")]
async fn agent_delete_recovery_guard(config: &Config, alias: &str) -> Result<()> {
    let checked = match rename_recovery::ensure_alias_not_retired(config, alias).await {
        Ok(()) => rename_recovery::ensure_not_pending_target(config, alias).await,
        Err(e) => Err(e),
    };
    match checked {
        Ok(()) => Ok(()),
        Err(RenameRecoveryError::AliasRetired { alias, pending_to }) => bail!(
            "{}",
            mta(
                "cli-alias-delete-retired",
                &[("alias", alias.as_str()), ("to", pending_to.as_str())],
                "alias `{$alias}` is retired by an unfinished rename to `{$to}`; run `zeroclaw agents rename {$alias} {$to}` before deleting anything"
            )
        ),
        Err(e @ RenameRecoveryError::RecoveryPending { .. }) => Err(rename_refused(e)),
        Err(e) => Err(rename_recovery_failed(&e)),
    }
}

/// Memory + optional session-backend handles opened from `data_dir` for the
/// owned-state cascade.
#[cfg(all(feature = "gateway", feature = "agent-runtime"))]
type OwnedStateHandles = (
    std::sync::Arc<dyn zeroclaw_memory::Memory>,
    Option<std::sync::Arc<dyn zeroclaw_infra::session_backend::SessionBackend>>,
);

#[cfg(all(feature = "gateway", feature = "agent-runtime"))]
fn build_owned_state_handles(config: &Config) -> Result<OwnedStateHandles> {
    use std::sync::Arc;
    let mem: Arc<dyn zeroclaw_memory::Memory> = if config.agents.is_empty() {
        Arc::new(zeroclaw_memory::NoneMemory::new("none"))
    } else {
        Arc::from(
            zeroclaw_memory::create_memory_from_config(config, None)
                .context("open memory backend for the owned-state cascade")?,
        )
    };
    let session_backend = if config.gateway.session_persistence {
        Some(
            zeroclaw_infra::make_session_backend(
                &config.data_dir,
                &config.channels.session_backend,
            )
            .context("open session backend for the owned-state cascade")?,
        )
    } else {
        None
    };
    Ok((mem, session_backend))
}

#[cfg(all(feature = "gateway", feature = "agent-runtime"))]
fn agent_delete_precheck(config: &Config, alias: &str) -> Result<()> {
    // Fail closed: refuse if live ACP sessions exist, or if the store can't be
    // read to verify (mirrors the gateway delete gate).
    let live = crate::gateway::agent_owned_state::live_acp_session_count(config, alias)
        .context("could not verify live ACP sessions")?;
    if live > 0 {
        let count = live.to_string();
        bail!(
            "{}",
            mta(
                "cli-alias-live-acp-sessions",
                &[("count", count.as_str()), ("alias", alias)],
                "{$count} live ACP session(s) for `{$alias}` — end them first"
            )
        );
    }
    Ok(())
}

#[cfg(not(all(feature = "gateway", feature = "agent-runtime")))]
fn agent_delete_precheck(_config: &Config, _alias: &str) -> Result<()> {
    Ok(())
}

#[cfg(all(feature = "gateway", feature = "agent-runtime"))]
async fn agent_delete_owned_state(
    config: &Config,
    alias: &str,
    workspace: &std::path::Path,
) -> Result<()> {
    let (mem, session_backend) = build_owned_state_handles(config)?;
    let ts = chrono::Utc::now().format("%Y%m%d%H%M%S");
    let archive_dir = config
        .data_dir
        .join("agents")
        .join("_deleted")
        .join(format!("{alias}-{ts}"));
    tokio::fs::create_dir_all(&archive_dir).await.ok();
    // Archive the workspace dir alongside the owned-state exports. `workspace`
    // was resolved by the caller before the config entry was removed, so a
    // custom `workspace.path` is preserved (post-removal it would default).
    if workspace.exists()
        && let Err(e) = tokio::fs::rename(&workspace, archive_dir.join("workspace")).await
    {
        let es = e.to_string();
        eprintln!(
            "{}",
            mta(
                "cli-alias-warn-workspace-archive",
                &[("error", es.as_str())],
                "warning: workspace archive failed: {$error}"
            )
        );
    }
    let report = crate::gateway::agent_owned_state::cascade_owned_state(
        config,
        &mem,
        session_backend.as_ref(),
        alias,
        &archive_dir,
    )
    .await;
    let memory = report.memory_purged.to_string();
    let cron = report.cron_removed.to_string();
    let acp = report.acp_removed.to_string();
    let sessions = report.sessions_cleared.to_string();
    let archive = archive_dir.display().to_string();
    println!(
        "{}",
        mta(
            "cli-alias-owned-cascaded",
            &[
                ("memory", memory.as_str()),
                ("cron", cron.as_str()),
                ("acp", acp.as_str()),
                ("sessions", sessions.as_str()),
                ("archive", archive.as_str())
            ],
            "owned-state cascaded: memory {$memory} · cron {$cron} · acp {$acp} · sessions {$sessions} → {$archive}"
        )
    );
    for w in &report.warnings {
        eprintln!(
            "{}",
            mta(
                "cli-alias-warn",
                &[("warning", w.as_str())],
                "warning: {$warning}"
            )
        );
    }
    Ok(())
}

#[cfg(not(all(feature = "gateway", feature = "agent-runtime")))]
async fn agent_delete_owned_state(
    _config: &Config,
    _alias: &str,
    _workspace: &std::path::Path,
) -> Result<()> {
    warn_agent_owned_state();
    Ok(())
}

#[cfg(not(all(feature = "gateway", feature = "agent-runtime")))]
fn warn_agent_owned_state() {
    eprintln!(
        "{}",
        mt(
            "cli-alias-owned-state-unavailable",
            "note: config references were updated, but the agent's owned state \
             (memory rows, workspace dir, cron/acp/session rows) was NOT cascaded \
             by this CLI yet — use the gateway API for the full owned-state cascade."
        )
    );
}

// ── providers ─────────────────────────────────────────────────────────────────

pub async fn handle_providers(cmd: ProvidersCommands, config: &mut Config) -> Result<()> {
    match cmd {
        ProvidersCommands::List { category } => {
            let cats = match category {
                Some(c) => vec![parse_provider_category(&c)?],
                None => vec![
                    ProviderCategory::Models,
                    ProviderCategory::Tts,
                    ProviderCategory::Transcription,
                ],
            };
            for cat in cats {
                let cat_name = match cat {
                    ProviderCategory::Models => "models",
                    ProviderCategory::Tts => "tts",
                    ProviderCategory::Transcription => "transcription",
                };
                // Enumerate families under this category, then their aliases.
                if let Some(families) = config.get_map_keys(&format!("providers.{cat_name}")) {
                    let mut families = families;
                    families.sort();
                    for family in families {
                        if let Some(mut aliases) =
                            config.get_map_keys(&format!("providers.{cat_name}.{family}"))
                        {
                            aliases.sort();
                            for a in aliases {
                                println!("{cat_name}.{family}.{a}");
                            }
                        }
                    }
                }
            }
            Ok(())
        }
        ProvidersCommands::Create {
            category,
            family,
            alias,
        } => {
            let cat = parse_provider_category(&category)?;
            let section = section_path(&AliasKind::Provider {
                category: cat,
                family,
            });
            create_entry(config, &section, &alias)?;
            save(config).await
        }
        ProvidersCommands::Rename {
            category,
            family,
            from,
            to,
        } => {
            let category = parse_provider_category(&category)?;
            rename_config(
                config,
                &AliasKind::Provider { category, family },
                &from,
                &to,
            )?;
            save(config).await
        }
        ProvidersCommands::Delete {
            category,
            family,
            alias,
            dry_run,
            yes,
        } => {
            let category = parse_provider_category(&category)?;
            let kind = AliasKind::Provider { category, family };
            delete_config(config, &kind, &alias, dry_run, yes)?;
            if yes && !dry_run {
                save(config).await?;
            }
            Ok(())
        }
    }
}

// ── channels ─────────────────────────────────────────────────────────────────

pub async fn handle_channels(cmd: ChannelsCommands, config: &mut Config) -> Result<()> {
    match cmd {
        ChannelsCommands::List { channel_type } => {
            // `channels` is a struct of per-type maps, not one flat map, so with
            // no filter we walk the canonical channel-type list.
            let types: Vec<String> = match channel_type {
                Some(t) => vec![t],
                None => zeroclaw_config::schema::v2::V3_CHANNEL_TYPES
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect(),
            };
            let mut types = types;
            types.sort();
            for t in types {
                if let Some(mut aliases) = config.get_map_keys(&format!("channels.{t}")) {
                    aliases.sort();
                    for a in aliases {
                        println!("{t}.{a}");
                    }
                }
            }
            Ok(())
        }
        ChannelsCommands::Create {
            channel_type,
            alias,
        } => {
            create_entry(config, &format!("channels.{channel_type}"), &alias)?;
            save(config).await
        }
        ChannelsCommands::Rename {
            channel_type,
            from,
            to,
        } => {
            rename_config(config, &AliasKind::Channel { channel_type }, &from, &to)?;
            save(config).await
        }
        ChannelsCommands::Delete {
            channel_type,
            alias,
            dry_run,
            yes,
        } => {
            let kind = AliasKind::Channel { channel_type };
            delete_config(config, &kind, &alias, dry_run, yes)?;
            if yes && !dry_run {
                save(config).await?;
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_provider_category_maps_known_and_rejects_unknown() {
        assert_eq!(
            parse_provider_category("models").unwrap(),
            ProviderCategory::Models
        );
        assert_eq!(
            parse_provider_category("tts").unwrap(),
            ProviderCategory::Tts
        );
        assert_eq!(
            parse_provider_category("transcription").unwrap(),
            ProviderCategory::Transcription
        );
        assert!(parse_provider_category("bogus").is_err());
    }

    #[test]
    fn section_path_for_each_kind() {
        assert_eq!(section_path(&AliasKind::Agent), "agents");
        assert_eq!(
            section_path(&AliasKind::Provider {
                category: ProviderCategory::Models,
                family: "anthropic".to_string(),
            }),
            "providers.models.anthropic"
        );
        assert_eq!(
            section_path(&AliasKind::Provider {
                category: ProviderCategory::Tts,
                family: "elevenlabs".to_string(),
            }),
            "providers.tts.elevenlabs"
        );
        assert_eq!(
            section_path(&AliasKind::Channel {
                channel_type: "discord".to_string(),
            }),
            "channels.discord"
        );
    }

    #[cfg(feature = "agent-runtime")]
    #[test]
    fn rename_refusals_name_the_rename_that_must_finish_first() {
        let retired = rename_refused(RenameRecoveryError::AliasRetired {
            alias: "scout".to_string(),
            pending_to: "ranger".to_string(),
        })
        .to_string();
        let pending = rename_refused(RenameRecoveryError::RecoveryPending {
            from: "scout".to_string(),
            to: "ranger".to_string(),
        })
        .to_string();
        for message in [&retired, &pending] {
            assert!(
                message.contains("zeroclaw agents rename scout ranger"),
                "{message}"
            );
            assert!(!message.contains("{cli-"), "missing Fluent key: {message}");
        }
        assert_ne!(retired, pending);
    }

    #[cfg(feature = "agent-runtime")]
    #[test]
    fn a_rename_recovery_failure_never_reads_as_a_missing_alias() {
        let missing = rename_refused(RenameRecoveryError::NotConfigured {
            alias: "scout".to_string(),
        })
        .to_string();
        assert!(missing.contains("agents.scout"), "{missing}");

        for error in [
            RenameRecoveryError::Unreadable {
                store: "cron".to_string(),
                detail: "disk I/O error".to_string(),
            },
            RenameRecoveryError::Busy {
                detail: "journal locked".to_string(),
            },
            RenameRecoveryError::Persist {
                detail: "read-only file system".to_string(),
            },
        ] {
            let detail = error.to_string();
            let message = rename_refused(error).to_string();
            assert!(message.contains(&detail), "{message}");
            assert!(!message.contains("{cli-"), "missing Fluent key: {message}");
            assert!(!message.contains("is not configured"), "{message}");
            assert_ne!(message, missing);
        }
    }
}
