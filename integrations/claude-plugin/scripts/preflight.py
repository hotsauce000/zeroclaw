"""Read/plan-only Claude Code plugin MCP. Requires Python 3.9+, stdlib only."""

import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import threading
import time

TIMEOUT_SECONDS = 5.0
MAX_CLI_BYTES = 65536
MAX_INPUT_BYTES = 32768
MAX_OUTPUT_BYTES = 16384
PROTOCOL_VERSIONS = {"2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"}
AUTH_STATUS_ARGV = ("auth", "status", "--json")


def require(condition, reason):
    if not condition:
        raise ValueError(reason)


def load_package_metadata():
    try:
        path = Path(__file__).resolve().parents[1] / ".claude-plugin" / "plugin.json"
        with path.open("rb") as manifest:
            raw = manifest.read(MAX_INPUT_BYTES + 1)
        require(len(raw) <= MAX_INPUT_BYTES, "invalid_package_metadata")
        parsed = json.loads(raw)
        require(type(parsed) is dict and type(parsed.get("metadata")) is dict, "invalid_package_metadata")
        version = parsed.get("version")
        minimum = parsed["metadata"].get("minimumClaudeCodeVersion")
        require(all(isinstance(value, str) and re.fullmatch(r"\d{1,3}\.\d{1,3}\.\d{1,3}", value)
                    for value in (version, minimum)), "invalid_package_metadata")
        return {"version": version, "minimum": tuple(int(n) for n in minimum.split("."))}
    except (OSError, ValueError, TypeError, RecursionError):
        return None


PACKAGE_METADATA = load_package_metadata()
VERSION = PACKAGE_METADATA["version"] if PACKAGE_METADATA is not None else None
MIN_CLAUDE_VERSION = PACKAGE_METADATA["minimum"] if PACKAGE_METADATA is not None else None


def arguments_object(arguments, allowed):
    require(type(arguments) is dict and not (arguments.keys() - allowed), "invalid_arguments")


def directory_reference(value):
    require(isinstance(value, str) and 0 < len(value) <= 512 and
            not any(ord(c) < 32 or ord(c) == 127 for c in value), "invalid_directory")
    path = Path(value)
    require(path.is_absolute() and ".." not in path.parts, "absolute_directory_required")
    return path


def account_directory(arguments, env):
    value = arguments.get("claude_config_dir", env.get("CLAUDE_CONFIG_DIR"))
    if value is None:
        return {"source": "native_default", "path": None}
    path = directory_reference(value)
    require(path.is_dir(), "account_directory_must_exist")
    return {"source": "selected" if "claude_config_dir" in arguments else "inherited",
            "path": str(path)}


def terminate(process):
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass


def reject_nonfinite_json(_value):
    raise ValueError("invalid_json")


def run_cli(executable, argv, env):
    if os.name != "posix":
        return "unsupported_platform", None
    # stderr is discarded at the pipe boundary: it may contain credentials.
    try:
        process = subprocess.Popen([executable, *argv], stdin=subprocess.DEVNULL,
                                   stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                   env=env, shell=False, start_new_session=True,
                                   bufsize=0)
    except OSError:
        return "unusable_cli", None
    output = bytearray()
    exceeded = threading.Event()

    def read_output():
        while True:
            block = process.stdout.read(4096)
            if not block:
                break
            if len(output) + len(block) > MAX_CLI_BYTES:
                exceeded.set()
                terminate(process)
                break
            output.extend(block)

    reader = threading.Thread(target=read_output, daemon=True)
    reader.start()
    deadline = time.monotonic() + TIMEOUT_SECONDS
    outcome = "ok"
    try:
        process.wait(timeout=TIMEOUT_SECONDS)
        reader.join(max(0, deadline - time.monotonic()))
        if reader.is_alive():
            outcome = "cli_timeout"
    except subprocess.TimeoutExpired:
        outcome = "cli_timeout"
    if exceeded.is_set():
        outcome = "cli_output_limit"
    if outcome != "ok":
        terminate(process)
        try:
            process.wait(timeout=0.2)
        except subprocess.TimeoutExpired:
            pass
        reader.join(0.2)
    if not reader.is_alive():
        process.stdout.close()
    if outcome != "ok":
        return outcome, None
    if process.returncode != 0:
        if process.returncode == 1 and tuple(argv) == AUTH_STATUS_ARGV:
            try:
                parsed = json.loads(output, parse_constant=reject_nonfinite_json)
            except (ValueError, TypeError, RecursionError):
                return "command_failed", None
            if type(parsed) is dict and parsed.get("loggedIn") is False:
                return "ok", b'{"loggedIn":false}'
        return "command_failed", None
    return "ok", bytes(output)


def environment_indicators(env):
    selectors = {
        "bearer_token": ("ANTHROPIC_AUTH_TOKEN",),
        "api_key": ("ANTHROPIC_API_KEY",),
        "headless_code_token": ("CLAUDE_CODE_OAUTH_TOKEN",),
        "console_profile": ("ANTHROPIC_PROFILE",),
        "federation_variables": ("ANTHROPIC_FEDERATION_RULE_ID", "ANTHROPIC_ORGANIZATION_ID"),
        "cloud_provider": ("CLAUDE_CODE_USE_BEDROCK", "CLAUDE_CODE_USE_VERTEX", "CLAUDE_CODE_USE_FOUNDRY"),
        "custom_endpoint": ("ANTHROPIC_BASE_URL",),
    }
    return [label for label, names in selectors.items() if any(env.get(n) for n in names)]


def auth_summary(parsed, env, expected):
    require(type(parsed) is dict and type(parsed.get("loggedIn")) is bool, "malformed_auth_status")
    modes = {"claude.ai": "claude_subscription", "oauth_token": "native_code_token",
             "api_key": "anthropic_api", "apiKeyHelper": "anthropic_api", "api_key_helper": "anthropic_api",
             "anthropic_profile": "console_api", "console": "console_api",
             "bedrock": "cloud_provider", "vertex": "cloud_provider",
             "foundry": "cloud_provider", "gateway": "cloud_gateway", "none": "unauthenticated"}
    method = parsed.get("authMethod")
    method = method if isinstance(method, str) and method in modes else "unknown"
    provider = parsed.get("apiProvider")
    provider = provider if isinstance(provider, str) and provider in {
        "firstParty", "bedrock", "vertex", "foundry", "gateway", "customEndpoint"} else "unknown"
    mode = modes.get(method, "unknown")
    if method == "anthropic_profile":
        profile_mode = parsed.get("profileAuthMode")
        mode = {"user_oauth": "console_api", "oidc_federation": "federation_api"}.get(profile_mode, "unknown")
    expected_providers = {
        "claude_subscription": {"firstParty"}, "native_code_token": {"firstParty"},
        "anthropic_api": {"firstParty"}, "console_api": {"firstParty"}, "federation_api": {"firstParty"},
        "cloud_provider": {"bedrock", "vertex", "foundry"}, "cloud_gateway": {"gateway"},
    }
    if mode in expected_providers and provider not in expected_providers[mode]:
        mode = "ambiguous"
    indicators = environment_indicators(env)
    # auth status is a report, not an inference probe. Conflicting selectors or
    # a custom endpoint need the operator's native /status review; never change them.
    if mode in {"claude_subscription", "native_code_token"} and indicators:
        mode = "ambiguous"
    conflicting_selectors = {
        "anthropic_api": {"cloud_provider", "custom_endpoint", "bearer_token"},
        "console_api": {"cloud_provider", "custom_endpoint", "bearer_token", "api_key", "headless_code_token"},
        "federation_api": {"cloud_provider", "custom_endpoint", "bearer_token", "api_key", "headless_code_token"},
    }
    if set(indicators) & conflicting_selectors.get(mode, set()):
        mode = "ambiguous"
    if not parsed["loggedIn"]:
        mode = "unauthenticated"
    billing = {"claude_subscription": "claude_account_plan", "native_code_token": "claude_account_plan",
               "anthropic_api": "anthropic_api", "console_api": "anthropic_api",
               "federation_api": "anthropic_api", "cloud_provider": "cloud_provider_account",
               "cloud_gateway": "gateway_account", "unauthenticated": "none"}.get(mode, "unknown")
    match = None if billing == "unknown" else billing in {
        "subscription": {"claude_account_plan"}, "api": {"anthropic_api"},
        "cloud_or_gateway": {"cloud_provider_account", "gateway_account"}}[expected]
    # Only constructed enums and a validated boolean leave this boundary.
    return {"logged_in": parsed["loggedIn"], "reported_method": method,
            "reported_provider": provider, "credential_mode": mode, "billing_source": billing,
            "expected_billing": expected, "billing_matches_expectation": match,
            "billing_status": "unknown" if match is None else "matches" if match else "mismatch",
            "environment_indicators": indicators, "inference_verified": False}


def status(arguments):
    require(PACKAGE_METADATA is not None, "invalid_package_metadata")
    arguments_object(arguments, {"claude_config_dir", "expected_billing"})
    expected = arguments.get("expected_billing", "subscription")
    require(expected in ("subscription", "api", "cloud_or_gateway"), "invalid_expected_billing")
    native = {"cli_status": "unsupported_platform", "version": None,
              "account_directory": {"source": "not_checked", "path": None}, "auth": None}
    result = {"native_code": native,
              "zeroclaw_engine": {"status": "requires_configuration", "native_code_backend": "unsupported",
                                  "claude_code_alias": "direct_anthropic_http", "inference_verified": False}}
    if os.name != "posix":
        return result
    env = dict(os.environ)
    directory = account_directory(arguments, env)
    if directory["path"] is not None:
        env["CLAUDE_CONFIG_DIR"] = directory["path"]
    native.update(cli_status="missing_cli", account_directory=directory)
    executable = shutil.which("claude", path=env.get("PATH", ""))
    if executable is None:
        return result
    outcome, output = run_cli(executable, ["--version"], env)
    if outcome != "ok":
        native["cli_status"] = "unusable_cli" if outcome == "command_failed" else outcome
        return result
    version = re.fullmatch(rb"(\d{1,3})\.(\d{1,3})\.(\d{1,3}) \(Claude Code\)", output.strip())
    if version is None:
        native["cli_status"] = "unrecognized_cli"
        return result
    numbers = tuple(int(n) for n in version.groups())
    native["version"] = ".".join(str(n) for n in numbers)
    if numbers < MIN_CLAUDE_VERSION:
        native["cli_status"] = "unsupported_cli_version"
        return result
    outcome, output = run_cli(executable, AUTH_STATUS_ARGV, env)
    if outcome != "ok":
        native["cli_status"] = "auth_status_failed" if outcome == "command_failed" else outcome
        return result
    try:
        native["auth"] = auth_summary(json.loads(output), env, expected)
    except (ValueError, TypeError, RecursionError):
        native["cli_status"] = "malformed_auth_status"
        return result
    native["cli_status"] = "available"
    return result


def plan(arguments):
    require(PACKAGE_METADATA is not None, "invalid_package_metadata")
    arguments_object(arguments, {"instance_root", "provider_alias", "agent_alias", "risk_preset",
                                 "accept_yolo", "engine_backend", "accept_api_billing", "claude_config_dir"})
    root = directory_reference(arguments.get("instance_root"))
    require(not os.path.lexists(root), "fresh_instance_root_required")
    require(root.parent.is_dir() and root.parent.resolve() == root.parent, "canonical_existing_parent_required")
    account = account_directory(arguments, os.environ)
    account_path = account["path"] or str(Path(os.environ.get("HOME", str(Path.home()))) / ".claude")
    account_path = Path(account_path).resolve()
    require(root != account_path and account_path not in root.parents and root not in account_path.parents,
            "account_instance_overlap")
    provider_alias = arguments.get("provider_alias")
    agent_alias = arguments.get("agent_alias")
    require(all(isinstance(v, str) and re.fullmatch(r"[a-z][a-z0-9_]{0,47}", v)
                for v in (provider_alias, agent_alias)), "invalid_alias")
    risk = arguments.get("risk_preset", "balanced")
    accept_yolo = arguments.get("accept_yolo", False)
    require(risk in ("balanced", "yolo") and type(accept_yolo) is bool and
            accept_yolo == (risk == "yolo"), "explicit_risk_choice_required")
    backend = arguments.get("engine_backend", "native_claude_code")
    accept_api = arguments.get("accept_api_billing", False)
    require(backend in ("native_claude_code", "anthropic_api") and type(accept_api) is bool and
            accept_api == (backend == "anthropic_api"), "explicit_engine_billing_choice_required")
    supported = backend == "anthropic_api"
    provider_ref = "anthropic." + provider_alias if supported else None
    return {"status": "requires_configuration" if supported else "unsupported",
            "instance_root": str(root), "writes_performed": False,
            "native_account_directory": account,
            "provider": {"alias": provider_alias, "engine_backend": backend,
                         "config_reference": provider_ref, "authentication_status": "requires_configuration",
                         "billing_source": "anthropic_api" if supported else "unsupported_backend"},
            "agent": {"alias": agent_alias, "model_provider": provider_ref, "risk_profile": risk},
            "risk": {"preset": risk, "canonical_source": "zeroclaw_config::presets::RISK_PRESETS",
                     "effective_policy_status": "unresolved", "accept_yolo": accept_yolo},
            "inference_verified": False,
            "terminal_handoff": {"human_terminal_required": True,
                                 "argv": ["zeroclaw", "--config-dir", str(root), "quickstart",
                                          "--model-provider", "anthropic", "--agent", agent_alias],
                                 "provider_alias_selection": provider_alias, "risk_preset_selection": risk,
                                 "recheck_fresh_root_before_execution": True} if supported else None}


def object_schema(properties, required=()):
    return {"type": "object", "properties": properties, "required": list(required), "additionalProperties": False}


PATH_SCHEMA = {"type": "string", "maxLength": 512, "description": "Operator-selected absolute directory reference; never credentials."}
ALIAS_SCHEMA = {"type": "string", "pattern": "^[a-z][a-z0-9_]{0,47}$"}
TOOLS = [
    {"name": "bootstrap.status", "description": "Read native Claude Code version/auth status; no login, inference or configuration writes.",
     "inputSchema": object_schema({"claude_config_dir": PATH_SCHEMA,
                                   "expected_billing": {"type": "string", "enum": ["subscription", "api", "cloud_or_gateway"]}})},
    {"name": "bootstrap.plan", "description": "Preview a fresh ZeroClaw instance and terminal handoff. Native-Code engine is unsupported; no install/apply.",
     "inputSchema": object_schema({"instance_root": PATH_SCHEMA, "provider_alias": ALIAS_SCHEMA, "agent_alias": ALIAS_SCHEMA,
                                   "claude_config_dir": PATH_SCHEMA,
                                   "risk_preset": {"type": "string", "enum": ["balanced", "yolo"], "default": "balanced"},
                                   "accept_yolo": {"type": "boolean", "default": False},
                                   "engine_backend": {"type": "string", "enum": ["native_claude_code", "anthropic_api"]},
                                   "accept_api_billing": {"type": "boolean", "default": False}},
                                  ("instance_root", "provider_alias", "agent_alias"))},
]
for tool in TOOLS:
    tool["annotations"] = {"readOnlyHint": True, "destructiveHint": False, "idempotentHint": True, "openWorldHint": False}


def error(request_id, code, message):
    return {"jsonrpc": "2.0", "id": request_id, "error": {"code": code, "message": message}}


def handle(request):
    if type(request) is not dict:
        return error(None, -32600, "invalid_request")
    request_id = request.get("id")
    valid_id = request_id is None or type(request_id) is int or (
        isinstance(request_id, str) and len(request_id) <= 64 and request_id.isascii())
    if request.get("jsonrpc") != "2.0" or not valid_id or not isinstance(request.get("method"), str):
        return error(None, -32600, "invalid_request")
    if PACKAGE_METADATA is None:
        return error(request_id, -32603, "invalid_package_metadata")
    method = request["method"]
    if "id" not in request:
        return None
    params = request.get("params", {})
    if type(params) is not dict:
        return error(request_id, -32602, "invalid_params")
    if method == "initialize":
        requested = params.get("protocolVersion")
        result = {"protocolVersion": requested if isinstance(requested, str) and requested in PROTOCOL_VERSIONS else "2024-11-05",
                  "capabilities": {"tools": {"listChanged": False}},
                  "serverInfo": {"name": "zeroclaw-onboarding-preflight", "version": VERSION}}
    elif method == "ping":
        result = {}
    elif method == "tools/list":
        result = {"tools": TOOLS}
    elif method == "tools/call":
        name = params.get("name")
        arguments = params.get("arguments", {})
        try:
            if name == "bootstrap.status":
                value = status(arguments)
            elif name == "bootstrap.plan":
                value = plan(arguments)
            else:
                return error(request_id, -32602, "unknown_tool")
            result = {"content": [{"type": "text", "text": json.dumps(value, separators=(",", ":"))}], "isError": False}
        except ValueError:
            result = {"content": [{"type": "text", "text": '{"status":"invalid_input"}'}], "isError": True}
    else:
        return error(request_id, -32601, "unknown_method")
    return {"jsonrpc": "2.0", "id": request_id, "result": result}


def stdio():
    while True:
        line = sys.stdin.buffer.readline(MAX_INPUT_BYTES + 1)
        if not line:
            return
        if len(line) > MAX_INPUT_BYTES:
            response = error(None, -32600, "input_limit")
        else:
            try:
                response = handle(json.loads(line))
            except (ValueError, UnicodeError, RecursionError):
                response = error(None, -32700, "invalid_json")
            except (OSError, TypeError):
                response = error(None, -32603, "internal_error")
        if response is not None:
            encoded = json.dumps(response, separators=(",", ":")).encode("utf-8")
            if len(encoded) > MAX_OUTPUT_BYTES:
                encoded = json.dumps(error(None, -32603, "output_limit")).encode("utf-8")
            sys.stdout.buffer.write(encoded + b"\n")
            sys.stdout.buffer.flush()
        if len(line) > MAX_INPUT_BYTES:
            return


if __name__ == "__main__":
    if sys.argv[1:] == ["--stdio"]:
        stdio()
    else:
        sys.exit(2)
