//! Plugin instance rows, channel bindings included: the pure half.
//!
//! A plugin package's host-owned state, its private `config` map and its
//! egress grant, lives on one `[[plugins.entries]]` row per *instance*, keyed
//! by `PluginInstanceScope::config_entry_key` over `(package, capability,
//! binding)`. The default tool binding's binding is the package name. A
//! channel instance's binding is the alias of the `[channels.plugin.<alias>]`
//! table that names the package, so which channel instances exist is live
//! config, read at use time: this module stores and caches nothing.
//!
//! [`instance_rows`] is the one enumeration of a package's rows. Every key it
//! yields comes from the constructor the runtime's activation plan admits the
//! same binding through, so a key the CLI prints is the key the runtime
//! resolves.
//!
//! Like `egress_ceremony`, this module owns only decisions; every user-facing
//! string stays in the CLI so it routes through Fluent.

use zeroclaw_config::schema::Config;
use zeroclaw_plugins::error::PluginError;
use zeroclaw_plugins::instance::PluginInstanceScope;
use zeroclaw_plugins::{PluginCapability, PluginManifest, PluginPermission};

/// The composite channel family of explicit `[channels.plugin.<alias>]`
/// bindings: a bound instance registers as the channel `plugin.<alias>`.
pub const PLUGIN_CHANNEL_FAMILY: &str = "plugin";

/// One `[[plugins.entries]]` row a package's instance owns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginInstanceRow {
    /// The capability world the instance serves: `Tool` for the default tool
    /// binding, `Channel` for a bound alias.
    pub capability: PluginCapability,
    /// The package name for the default tool binding; the configured alias
    /// for a channel binding.
    pub binding: String,
    /// The `zpi1_` config entry key, from
    /// `PluginInstanceScope::config_entry_key`.
    pub key: String,
}

impl PluginInstanceRow {
    /// Whether this row belongs to a `[channels.plugin.<alias>]` binding
    /// rather than to the default tool binding.
    #[must_use]
    pub fn is_channel(&self) -> bool {
        self.capability == PluginCapability::Channel
    }

    /// The name the CLI prints for this instance.
    ///
    /// The default tool binding prints the bare `package`, so tool output is
    /// exactly what it was before channel rows were enumerated. A channel row
    /// prints `package (plugin.<alias>)`, which tells two instances of one
    /// package apart and names the binding the operator edits.
    #[must_use]
    pub fn display_name(&self, package: &str) -> String {
        if self.is_channel() {
            format!("{package} ({PLUGIN_CHANNEL_FAMILY}.{})", self.binding)
        } else {
            package.to_string()
        }
    }

    /// The row names a pre-typed-config install could carry for this
    /// instance, as `egress_ceremony::resolve_grant_state` takes them.
    ///
    /// The default tool binding's legacy row was keyed by the package name. A
    /// channel row has none: channel construction postdates typed config, so
    /// no runtime ever read an alias-named row. Offering the package name here
    /// would point the rename step at the tool binding's legacy row and move
    /// that instance's grant onto this one.
    #[must_use]
    pub fn legacy_candidates(&self, package: &str) -> Vec<String> {
        if self.is_channel() {
            Vec::new()
        } else {
            vec![package.to_string()]
        }
    }
}

/// Whether an instance of `manifest` owns host state, and so is owed a row.
///
/// A row is owed when the instance has host-owned state to hold: a private
/// config object (`config_schema`), a declared egress destination, or a
/// governed transport whose reach the operator must grant. A manifest with
/// none of those owns no state and gets no row. The rule is the same for every
/// instance of the package, tool or channel.
///
/// The transport arm is deliberate. The second grant path is the plugin whose
/// destination *is* deployment configuration, a self-hosted Gitea or a LAN
/// Nextcloud, which its author cannot declare, so it ships a transport with no
/// `[egress]` table and often no `config_schema`. Without a row there is
/// nowhere to author that grant: `config set plugins.entries.<key>.egress_hosts`
/// only resolves keys already present in live config, and `plugin info` would
/// not even print the opaque key to address. Pinned by
/// `a_network_permission_alone_earns_a_row_so_the_operator_can_grant_reach`.
#[must_use]
pub fn manifest_owns_instance_state(manifest: &PluginManifest) -> bool {
    manifest.config_schema.is_some()
        || !manifest.egress.hosts.is_empty()
        || manifest_has_governed_transport(manifest)
}

/// Whether `manifest` requests a transport the plugin egress authority
/// governs: `http_client`, `websocket_client` or `socket_client`, the
/// permissions `zeroclaw_plugins::egress` requires for HTTP, for WebSocket, and
/// for TCP, TLS or STARTTLS reach. Each reaches only the destinations its
/// instance row grants.
#[must_use]
pub fn manifest_has_governed_transport(manifest: &PluginManifest) -> bool {
    manifest.permissions.iter().any(|permission| {
        matches!(
            permission,
            PluginPermission::HttpClient
                | PluginPermission::WebSocketClient
                | PluginPermission::SocketClient
        )
    })
}

/// The config entry key of the channel instance that the binding `alias`
/// makes of `manifest`'s package.
///
/// Derived with `PluginInstanceScope::from_manifest(manifest, Channel, alias,
/// [])`, the constructor the activation plan admits a
/// `[channels.plugin.<alias>]` binding through. The key covers `(package,
/// capability, binding)` and never the grant set, so the empty grant set here
/// derives the key of the scope the runtime builds with the manifest's
/// permissions.
///
/// # Errors
///
/// The constructor's refusal: a manifest that does not declare `channel`, or
/// an alias the instance identity rules reject.
pub fn channel_instance_key(manifest: &PluginManifest, alias: &str) -> Result<String, PluginError> {
    PluginInstanceScope::from_manifest(
        manifest,
        PluginCapability::Channel,
        alias,
        std::iter::empty(),
    )?
    .id()
    .config_entry_key()
}

/// The aliases of every `[channels.plugin.<alias>]` binding whose `package` is
/// `package`, sorted.
///
/// Disabled bindings are included. `enabled = false` keeps an instance from
/// starting; it does not end the instance, and its row keeps the instance's
/// config and grant for when it is enabled again.
#[must_use]
pub fn bound_channel_aliases(config: &Config, package: &str) -> Vec<String> {
    let mut aliases: Vec<String> = config
        .channels
        .plugin
        .iter()
        .filter(|(_, binding)| binding.package == package)
        .map(|(alias, _)| alias.clone())
        .collect();
    aliases.sort_unstable();
    aliases
}

/// Every `[[plugins.entries]]` row `manifest`'s package owns: the default tool
/// binding first, then one row per bound channel alias, sorted by alias.
///
/// Both kinds follow [`manifest_owns_instance_state`]. The tool row needs the
/// `tool` capability. A channel row needs the `channel` capability and a
/// `[channels.plugin.<alias>]` binding that names the package: without one a
/// channel package has no instance, so it yields no row rather than a
/// package-level key nothing reads.
///
/// # Errors
///
/// A key derivation failure from the instance constructor.
pub fn instance_rows(
    config: &Config,
    manifest: &PluginManifest,
) -> Result<Vec<PluginInstanceRow>, PluginError> {
    if !manifest_owns_instance_state(manifest) {
        return Ok(Vec::new());
    }

    let mut rows = Vec::new();
    if manifest.capabilities.contains(&PluginCapability::Tool) {
        let scope = PluginInstanceScope::for_package_binding(
            manifest,
            PluginCapability::Tool,
            std::iter::empty(),
        )?;
        rows.push(PluginInstanceRow {
            capability: PluginCapability::Tool,
            binding: scope.id().binding().to_string(),
            key: scope.id().config_entry_key()?,
        });
    }
    if manifest.capabilities.contains(&PluginCapability::Channel) {
        for alias in bound_channel_aliases(config, &manifest.name) {
            let key = channel_instance_key(manifest, &alias)?;
            rows.push(PluginInstanceRow {
                capability: PluginCapability::Channel,
                binding: alias,
                key,
            });
        }
    }
    Ok(rows)
}

/// Whether `row`'s instance holds a transport that can reach a declared
/// destination.
///
/// A declaration counts only with a transport that can use it. A row persists
/// across `plugin remove`, so a grant seeded for a version that could not
/// reach the network would silently become live reach when a later version of
/// the same package adds a transport. That later install meets an existing
/// row, which is never extended, so the operator grants it deliberately.
///
/// The two kinds of row count different transports, on purpose. A channel row
/// counts any governed transport ([`manifest_has_governed_transport`]): a
/// channel that speaks only over a socket, such as IRC, email or MQTT, or only
/// over a WebSocket relay would otherwise never have its declaration seeded or
/// its gap reported. The default tool binding keeps the rule the tool ceremony
/// shipped with, `http_client` only, which predates the WebSocket and socket
/// transports. Widening it changes what `plugin install` grants and what
/// `plugin list` reports for tool packages already installed, so it is left to
/// a change of its own; the CLI's install-time `declared_egress_hosts` applies
/// the same tool rule.
#[must_use]
pub fn row_has_usable_transport(manifest: &PluginManifest, row: &PluginInstanceRow) -> bool {
    if row.is_channel() {
        manifest_has_governed_transport(manifest)
    } else {
        manifest.permissions.contains(&PluginPermission::HttpClient)
    }
}

/// The declared destinations `row`'s instance can use: the manifest's
/// `[egress]` hosts when [`row_has_usable_transport`] holds, otherwise none.
/// Empty as well when the manifest declares nothing.
///
/// This is the declaration, never a grant: nothing here confers network reach.
#[must_use]
pub fn declared_hosts_for_row(manifest: &PluginManifest, row: &PluginInstanceRow) -> Vec<String> {
    if row_has_usable_transport(manifest, row) {
        manifest.egress.hosts.clone()
    } else {
        Vec::new()
    }
}
