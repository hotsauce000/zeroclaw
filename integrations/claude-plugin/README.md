# ZeroClaw Claude Code onboarding plugin

This local Code plugin provides `/zeroclaw:onboard` and two read/plan MCP tools.
It checks native Claude authentication and previews a fresh ZeroClaw instance.
The host's Claude account can assist with setup; ZeroClaw's independent model
engine still requires configuration. A native-Code model backend is unsupported.
The current `claude-code` provider alias resolves to direct Anthropic HTTP.

## Prerequisites and loading

Use unmodified Claude Code **2.1.289+** and **Python 3.9+**, available as
`python3` on the same local Mac, Linux or WSL host. Code 2.1.289 is this
package's tested minimum; older versions are not claimed supported. Python's
standard library is sufficient;
there is no dependency download, bootstrap executable or control-MCP service.
Native Windows auth probing returns `unsupported_platform` before discovering
or spawning Claude. Use a written plan, or run Code and the helper inside WSL
with its own native account selection; the plugin does not launch WSL or copy
Windows credentials automatically.
If Python is absent, the skill can explain the written plan and terminal
prerequisites, but the MCP checks cannot run. No fresh-machine installation
claim includes automatic runtime or ZeroClaw provisioning.

Load the package for one local session, substituting its absolute path:

```sh
claude --plugin-dir /absolute/path/to/integrations/claude-plugin
```

Invoke `/zeroclaw:onboard`. `/mcp` should show `plugin:zeroclaw:preflight`.
The package follows the [Code plugin layout][components] and can be shared as
this directory or a ZIP through the [native plugin distribution flow][publish].
Session loading avoids a persistent marketplace/settings change. If installing,
the operator chooses the native scope; this package changes no Claude settings.
Web chat cannot execute this local helper. Local Code 2.1.289 discovery was
verified in an isolated bare session with a synthetic API key: the skill and
both read-only MCP tools appeared. No tool, real account or model request was
used in that discovery check. Desktop/Cowork and account sync remain unverified.

## Read and plan contract

`bootstrap.status` accepts optional `claude_config_dir` and `expected_billing`
(`subscription`, `api`, or `cloud_or_gateway`). It runs only `claude --version`
and `claude auth status --json`, with argv arrays and the selected directory in
the child environment. Existing selectors remain intact. The helper preserves
the parent's environment and does not read credential files or invoke login.
Native auth status uses exit 1 when logged out. The helper admits only that
exact command/exit combination with well-formed JSON and `loggedIn: false`,
then discards the nonzero payload apart from the validated login flag. Other
nonzero results, including version failures, remain sanitized failures. See the
[native CLI contract][cli]. The documented `api_key_helper` method is recognized;
unproven third-party modes keep unknown billing.
Code itself owns its authentication/storage behavior; a real auth-status smoke
and refresh/expiry behavior remain unverified in this integration.
The helper reads only its packaged `.claude-plugin/plugin.json` at startup to
derive the package version and Code minimum from their canonical manifest fields.
Missing, oversized or invalid metadata is rejected with `invalid_package_metadata`
protocol errors, without printing raw file data or parse errors.

Only recognized method/provider enums, a boolean login flag and constructed
billing categories leave preflight. Identity, tokens, unknown fields and raw
errors are discarded. Environment indicators expose presence, never values.
Conflicting selectors/custom endpoints report ambiguous billing for native
`/status` review. Status alone never verifies inference or plugin sync.
Subscription, headless native tokens, Console OAuth, API keys, federation and
cloud/gateway billing remain distinct. Native login and Console profile
isolation follow [Code's own authentication contract][auth].

`bootstrap.plan` requires `instance_root`, `provider_alias` and `agent_alias`.
It accepts optional `claude_config_dir`, `risk_preset`, `accept_yolo`,
`engine_backend` and `accept_api_billing`. Directory inputs are absolute,
at most 512 characters and free of traversal/control characters. Account
directories must exist; a fresh instance root must not exist, overlap the native
account directory or use a symlinked/nonexistent parent. Aliases use lowercase
letters, digits and underscores, begin with a letter and are at most 48 characters.

The default engine route, `native_claude_code`, returns `unsupported` with no
execution handoff. The independent `anthropic_api` route requires
`accept_api_billing: true` and returns `requires_configuration` with the real
interactive Quickstart argv. Provider alias and risk selection remain terminal
choices because Quickstart exposes no flags for them. No credentials belong in
the plan or argv. Billing selection is a proposal, not provider authentication.

The default risk reference is `balanced`; `yolo` requires `accept_yolo: true`.
Both refer to ZeroClaw's canonical `RISK_PRESETS` rather than copied policy
values. Effective policy is `unresolved` until native Quickstart review and
configuration inspection. Recheck the fresh root immediately before execution;
a read-only preview cannot reserve it or prove a later policy. The repository's
protocol design is `docs/book/src/developing/claude-code-onboarding.md`.

The server exposes exactly two tools. Unknown methods/install/apply requests
are rejected. Plans and cancellations perform no writes or subprocess calls.
No service installation, account switching, credential export, permission
bypass, connector authorization, control handoff or model request is implemented.
Future native-Code inference needs a separate loop/session/permission contract
and assessment against the [native-client terms][terms].

## Verification and rollback

From the repository root:

```sh
python3 -I -B -m unittest discover -s integrations/claude-plugin/tests -v
claude plugin validate --strict integrations/claude-plugin
```

Tests use synthetic process fixtures and isolated directories. They cover real
argv/environment forwarding, missing/wrong binaries, malformed/hostile output,
credential/billing distinctions, bounded capture/timeouts, protocol-only stdout
and unchanged config sentinels after rejected install/cancel requests. Manifest
validation does not prove a live Code session loaded the tools or authenticated
an account. No live auth/model smoke was run. The session-loaded package is
removed by ending that session; it creates no ZeroClaw instance state to undo.

[components]: https://code.claude.com/docs/en/plugins/components
[publish]: https://code.claude.com/docs/en/plugins/publish
[auth]: https://code.claude.com/docs/en/authentication
[cli]: https://code.claude.com/docs/en/cli-reference
[terms]: https://code.claude.com/docs/en/legal-and-compliance
