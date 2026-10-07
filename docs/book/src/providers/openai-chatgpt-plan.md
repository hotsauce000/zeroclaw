# ChatGPT Plan Usage

This opt-in provider uses OpenAI's open-source Sign in with ChatGPT flow.
It is separate from [Codex subscription authentication](./openai-codex-subscription.md).
The first implementation supports text requests with tools disabled through
`auth plan-check`. Agent tools, credential imports, device login, and managed
hosting are unsupported. Synthetic tests establish the protocol integration;
live account entitlement and model availability require a separately authorized
smoke test.
Native Windows is unsupported in this slice; use Linux, macOS, or WSL.

## Sign in and bind a provider

Use a separate instance directory. Login creates a stable opaque host identifier
in its existing auth profile store before displaying the authorization URL.
Open the URL in a browser on the same machine, review the ChatGPT plan-use grant,
and complete the loopback callback while the command is running.

```sh
zeroclaw --config-dir /path/to/new-instance auth login \
  --model-provider chatgpt-plan --profile subscriber
```

Add an explicit provider reference to that instance's `config.toml`:

```toml
[providers.models.openai.subscriber]
kind = "chatgpt-plan"
model = "<account-visible-model-slug>"
wire_api = "responses"

[providers.models.openai.subscriber.chatgpt_plan_auth]
registration = "chatgpt-plan:subscriber"
```

The credential store owns the issued client ID, verified subject, scopes,
expiry, and encrypted tokens. Config contains only the reference. A different
account or workspace needs a separate profile label. Reauthorizing a saved
profile reuses its issued client ID; a different client or subject is rejected
without replacing its credentials. A missing plan-use scope fails closed.

Global `auth use` selection cannot redirect a plan provider. Keep
`requires_openai_auth` unset: it retains its existing Codex meaning. API keys,
custom endpoints and headers, provider fallbacks, reliability API-key rotation,
temperature, and output-token limits cannot be combined with this provider.

## Verify one text response

Select a model visible to this account; the normal model-catalog command reads
the bound account's catalog:

```sh
zeroclaw --config-dir /path/to/new-instance models refresh \
  --model-provider openai.subscriber
zeroclaw --config-dir /path/to/new-instance auth plan-check \
  --model-provider openai.subscriber --message "Reply READY"
```

The check consumes the selected account's ChatGPT allowance. It uses public
`https://api.openai.com/v1/responses`, sends `store:false` and `stream:true`,
and succeeds only on `response.completed`. The HTTP request and SSE body share
a 300-second total deadline. Truncation, incomplete responses, revocation and
quota failures surface as errors; no metered fallback is selected.
This check does not establish full agent/tool readiness.

## Credential lifecycle and rollback

Tokens are always encrypted for this grant, including when the legacy
`secrets.encrypt` preference is disabled. Profile writes use private atomic
temporary files and reject credential leaf symlinks. Kernel-held refresh locks
coordinate processes sharing an instance, reread current state after acquisition,
and retain replacement refresh tokens. Separate roots remain independent;
directory aliases to the same root coordinate together.

On Unix, canonical store operations also hold a kernel lease on
`auth-profiles.guard`. While held, they publish `auth-profiles.lock` as a hard
link to that gate so older releases still observe their existing exclusion
protocol. After a process dies, a current release can reclaim only a sentinel
whose inode matches the locked gate, including death during marker or token
replacement. Normal release removes the sentinel, so a clean downgrade can
acquire the legacy lock. Keep the gate file in place; deleting or replacing lock
files while processes use the instance breaks this coordination.
Lock interoperability does not preserve plan fields in older serializers. Keep
the separate instance on a plan-aware release; an older credential writer can
discard registration metadata and refresh uncertainty if it rewrites that store.

A PID-only sentinel left by a pre-upgrade release cannot be safely reclaimed
automatically: older writers do not participate in the kernel lease. Such a
sentinel remains fail-closed whether its PID is live or dead. Stop all processes
using the instance before an operator repairs that legacy lock. A filesystem
without hard-link or kernel-lock support fails closed. These guarantees cover
process death on one host, not power loss or distributed filesystem locking.

Refresh automatically near expiry, or use `auth refresh --model-provider
chatgpt-plan --profile subscriber`. CLI `auth logout` for these registrations
is explicitly unsupported in this slice; disconnect the app in ChatGPT settings.
Remove the explicit provider binding before retiring the instance. Do not turn
it into an API-key profile implicitly. Keep `kind = "chatgpt-plan"` with the
binding: older factories reject this implementation marker, protecting against
accidental API-key billing after a downgrade.

An unresolved refresh transaction blocks reuse until fresh sign-in. The store
records a refresh-start marker before egress and clears it only together with
the committed replacement tokens, including after interrupted processes.

OpenAI documents [registration](https://developers.openai.com/siwc/token-sharing-open-source/sign-in),
[sessions](https://developers.openai.com/siwc/token-sharing-open-source/profiles-and-sessions),
[token metadata](https://developers.openai.com/siwc/token-sharing-open-source/token-reference),
[inference](https://developers.openai.com/siwc/token-sharing-open-source/models-and-inference),
and [preview restrictions](https://developers.openai.com/siwc/token-sharing-open-source/preview-limitations).
