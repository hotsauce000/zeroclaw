---
name: onboard
description: Check native Claude Code account mode and plan a fresh ZeroClaw instance with explicit engine billing and risk choices.
disable-model-invocation: true
---

# ZeroClaw onboarding

Guide the operator through a reviewable plan. This plugin supports local Claude
Code. The host's signed-in setup conversation and ZeroClaw's independent model
engine are separate. Native-Code inference for the ZeroClaw engine is currently
unsupported; the existing `claude-code` provider alias uses direct Anthropic HTTP.

1. Check prerequisites: unmodified Claude Code 2.1.289+ and `python3` 3.9+ on
   a local Mac, Linux or WSL host. Code 2.1.289 is the tested minimum.
   Native Windows probing is unsupported and must return `unsupported_platform`
   without starting Claude. Offer written-plan guidance or an operator-selected
   Code session inside WSL; do not automatically launch WSL or copy credentials.
   The bundled helper uses only Python's standard library.
   If the MCP server is unavailable, explain the missing prerequisite and continue
   with a written plan. Do not claim preflight passed or automatically install a
   runtime. In web chat, Desktop Chat or Cowork, hand off to a local Code terminal;
   this package has not verified those execution surfaces.
2. Ask which native account directory the operator intends to use. Omit
   `claude_config_dir` to preserve the session's inherited selection/default.
   An explicitly selected directory must already exist. Call `bootstrap.status`
   on this plugin's `preflight` MCP server with that reference and the expected
   billing mode, normally `subscription`. Never ask for or paste credential
   contents. Report only the helper's sanitized fields, including unknown or
   mismatched billing. A different selected directory checks that directory;
   it does not switch the current host conversation to that account.
3. If native sign-in is needed, give the operator this terminal-only pattern,
   with the selected directory quoted as one argument:

   ```sh
   env CLAUDE_CONFIG_DIR='/absolute/operator-selected/account-directory' claude auth login --claudeai
   ```

   The operator completes Code's own browser/terminal flow. Never run login,
   `/logout`, `setup-token`, or `ant auth login` through this helper, and never
   collect their output. `/login` is also native. `setup-token` is a headless
   native-Code credential, not a ZeroClaw HTTP-provider credential supplied by
   this plugin. `ant auth login` is Console API billing, not included Pro/Max
   usage. Do not unset credential selectors, edit managed settings, or silently
   fall back to an API key. Have the operator inspect native `/status` when the
   helper reports ambiguity. See [native authentication][auth].
4. Ask for a fresh, nonexistent, absolute ZeroClaw root under an existing
   canonical parent, a provider alias, and an agent alias. Choose `balanced`
   as the default supervised risk preset. Select `yolo` only after explicit
   instance-specific consent, represented by `accept_yolo: true`. Explain that
   the preset expands ZeroClaw autonomy and reduces its safeguards; Claude host,
   administrator and OS restrictions retain their own authority.
5. Ask the operator to choose the engine route explicitly. For subscription
   native-Code inference, use `engine_backend: native_claude_code` and show the
   `unsupported` result. Stop at that plan. To proceed with independent Anthropic
   API billing, require that explicit choice and set
   `engine_backend: anthropic_api, accept_api_billing: true`. Call
   `bootstrap.plan` with the selected root, named references and risk choice.
   Never reinterpret account sign-in or connector authorization as engine auth.
6. Show the returned plan and status. The risk reference is a preset name;
   `effective_policy_status: unresolved` means permissions have not been proven.
   No install/apply tool exists. If ZeroClaw is absent, link to its
   [current releases][releases] and [Quickstart][quickstart]; let the operator
   review and run the appropriate installation path in a terminal. Do not install
   an OS service. This package needs neither `zeroclaw-bootstrap` nor control-MCP.
7. For the explicitly selected API route, show `terminal_handoff.argv` as an
   argument array. If rendering a shell command, quote every argument using that
   shell's quoting rules; never concatenate operator input unquoted. The operator
   rechecks that the root is still fresh and starts interactive Quickstart.
   Its flags only preselect provider type and agent: select the planned provider
   alias and canonical risk preset in the terminal, review the effective policy,
   and confirm Create there. Quickstart preserves existing same-named profiles,
   so a label alone cannot establish effective permissions. Credentials belong
   exclusively in the native masked prompt, never in argv, chat or this MCP.
8. On cancellation, leave the plan and stop. After human setup, continue to
   report `requires_configuration` until configuration is independently inspected;
   claim inference only after a separately authorized bounded engine test. This
   first slice performs no model requests, pairing or permission changes.

[auth]: https://code.claude.com/docs/en/authentication
[releases]: https://github.com/zeroclaw-labs/zeroclaw/releases/latest
[quickstart]: https://docs.zeroclaw.com/master/en/getting-started/quickstart.html
