# Configuration

Enki loads configuration from TOML files in a cascade:

1. `~/.config/enki.toml` — global defaults
2. `<project>/.enki/enki.toml` — per-project overrides

Later files override earlier ones field-by-field. All fields are optional; unset values use defaults.

## Full example

```toml
[git]
commit_suffix = "created by enki"

[workers]
max_workers = 10
max_heavy = 5
max_standard = 5
max_light = 10
sonnet_only = false

[agent]
command = "claude-agent-acp"
args = []
env = {}
```

## Reference

### `[git]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `commit_suffix` | string | `"created by enki"` | Appended to commit messages (`\n\n<suffix>`). Set to `""` to disable. |

### `[workers]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `max_workers` | integer | `10` | Maximum total concurrent workers. |
| `max_heavy` | integer | `5` | Maximum concurrent heavy-tier workers. |
| `max_standard` | integer | `5` | Maximum concurrent standard-tier workers. |
| `max_light` | integer | `10` | Maximum concurrent light-tier workers. |
| `sonnet_only` | boolean | `false` | Force all workers to use Sonnet instead of the agent's default model. Sets the model via ACP `set_session_config_option` after session creation. |

### `[agent]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `command` | string | `"claude-agent-acp"` | Agent binary. The built-in `claude-agent-acp` is auto-installed via npm. Any other value is resolved from PATH or as an absolute path. |
| `args` | string[] | `[]` | Extra CLI arguments passed to the agent binary. |
| `env` | table | `{}` | Environment variables set on the agent process. Enki's own env vars (`ENKI_BIN`, `ENKI_DIR`, `ENKI_SESSION_ID`) take precedence. |

## Custom agents

Any ACP-compatible binary can be used. The binary must speak JSON-RPC 2.0 over stdio and implement the [Agent Client Protocol](https://agentclientprotocol.com/).

```toml
[agent]
command = "/usr/local/bin/my-agent"
args = ["--some-flag"]
env = { MY_API_KEY = "sk-..." }
```

## Sonnet-only mode

When `sonnet_only = true`, enki reads the model config options from the ACP `NewSessionResponse` and selects the first option matching "sonnet" (case-insensitive). This only works with agents that expose a model selector via ACP config options (like `claude-agent-acp`). If no sonnet model is found, a warning is logged and the agent's default model is used.
