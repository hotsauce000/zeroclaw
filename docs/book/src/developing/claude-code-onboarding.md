# Claude Code onboarding protocol

The package at `integrations/claude-plugin/` is a local Claude Code plugin,
distinct from ZeroClaw's WASM plugins and its model providers. It contains one
user-invoked onboarding skill and a Python standard-library stdio MCP helper.
The helper is an external read/plan adapter; it cannot apply configuration,
install services, authenticate an account or run inference.

## Ownership and canonical sources

Native Code owns credential selection, storage and login. The helper selects an
existing account-directory reference for a child `auth status` invocation and
returns only allowlisted nonsecret status fields. It reads no credential file,
copies no account token and never changes credential selectors or managed settings.
An auth report is not proof of a working model request.
The [native CLI contract][cli] defines auth-status exit 1 for a logged-out account.
The helper admits that exit only for its fixed auth-status argv and a parsed
`loggedIn` value exactly equal to the boolean false. It reconstructs a minimal
login-state payload rather than retaining nonzero output fields. Malformed,
non-finite, logged-in or other nonzero output is rejected. Version probes keep
their normal nonzero failure behavior. The documented `api_key_helper` method
maps to the API billing category; unproven third-party modes stay unknown.

ZeroClaw owns provider configuration and effective runtime policy. The existing
`claude-code` alias folds into the Anthropic HTTP adapter in
`crates/zeroclaw-providers/src/lib.rs`. This plugin implements no native-Code
ZeroClaw model backend. Its default engine plan is therefore `unsupported`;
an explicitly selected independent API route is `requires_configuration`.

Risk choices reference `zeroclaw_config::presets::RISK_PRESETS`, whose owner is
`crates/zeroclaw-config/src/presets.rs`. The helper contains no copied preset
values. A plan's risk name is not effective policy: it always reports
`effective_policy_status: unresolved`. Quickstart preserves existing profiles
with matching preset names. A fresh-root recheck and terminal review must precede
creation; the preview neither reserves a path nor authorizes the eventual policy.

## Transport and bounds

The `.mcp.json` entry starts `python3 -I -B` with a packaged script path as a
single argv element. Python 3.9+ and local Code 2.1.289+ on Mac, Linux or WSL are
explicit prerequisites. Code 2.1.289 is the tested minimum for this package.
The helper does not download dependencies and does not need a bootstrap or
control-MCP binary. An absent helper runtime permits written skill guidance only.
Native Windows is unsupported for auth probing: the helper returns
`unsupported_platform` before executable discovery or any Claude process starts.
Written-plan guidance or a separate operator-selected WSL Code session are the
alternatives; the helper never starts WSL or copies account credentials.

Package version and the tested Code minimum are canonical fields in
`.claude-plugin/plugin.json`. The helper reads that packaged, nonsecret manifest
once at startup and derives its server version identity and CLI requirement.
It bounds the read and rejects missing, oversized or invalid metadata with a
fixed `invalid_package_metadata` response rather than raw parser/file output.

The [stdio MCP transport][transport] uses newline-delimited JSON-RPC on stdout.
The helper supports initialization, ping and tools discovery/calls, negotiating
the requested supported protocol version or its `2024-11-05` baseline. It lists
only `bootstrap.status` and `bootstrap.plan`, both marked read-only. Unknown
methods and tool names are rejected; there is no install/apply capability.
Notifications, including cancellation, produce no output or state changes.
Calls are sequential; an auth-status subprocess can finish or time out before a
queued cancellation is consumed. Cancellation cannot undo a later human command.

Input is bounded to 32 KiB per frame and responses to 16 KiB. Native CLI stdout
is capped during capture at 64 KiB, and stderr goes directly to the null device.
Each native command has a five-second deadline. Timeout/overflow kills the
POSIX process group. Non-POSIX probing is rejected at both the status and process
boundaries; no native Windows process-cleanup guarantee is offered. The helper
does not retain raw command output in logs or files.

## Boundary evidence and remaining seams

The package tests spawn the actual helper over stdio and synthetic native
executables. They prove argv/account-directory forwarding, output sanitization,
missing/malformed/error/timeout handling and unchanged existing config sentinels.
Package metadata declares the same runtime and capability limits, and the
installed native Code validator checks the package structure.

Local Code 2.1.289 skill/MCP discovery was verified in an isolated bare session
using a synthetic API key, with no tool call, real account lookup or model request.
Real auth-status behavior, authenticated account discovery, account sync,
Desktop/Cowork execution and live model inference remain unverified.
The native-client [authentication][auth] and [legal conditions][terms] retain
authority over future designs. A genuine Code-backed engine needs separate
session binding, streaming/cancellation, result/usage attribution and tool-loop
permission contracts. Successful host assistance cannot fill those seams.

No core provider/config/runtime/CLI files are changed by this package. Removing
the session-loaded plugin removes its helper and skill without an instance
configuration rollback.

[transport]: https://modelcontextprotocol.io/specification/2025-06-18/basic/transports
[auth]: https://code.claude.com/docs/en/authentication
[cli]: https://code.claude.com/docs/en/cli-reference
[terms]: https://code.claude.com/docs/en/legal-and-compliance
