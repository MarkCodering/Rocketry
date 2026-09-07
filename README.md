# Rocketry

A Rust agent runtime with a Mission Control terminal interface. One durable execution engine powers the Rust SDK, CLI, TUI, and authenticated REST/SSE server.

![Mission Control visual fixture](docs/mission-control.png)

## Install and launch

Requires Rust 1.88+ and Cargo on macOS or Linux. From this checkout:

```sh
./scripts/install.sh
rocketry --help
rocketry
```

The installer runs `cargo install --path crates/cli --locked`; the binary is normally installed at `~/.cargo/bin/rocketry`. If needed, add `export PATH="$HOME/.cargo/bin:$PATH"` to your shell startup file and open a new shell. Rerun the installer after pulling changes. It accepts Cargo install options, for example `./scripts/install.sh --offline`. To uninstall, run `cargo uninstall rocketry-cli`.

To build and run directly:

```sh
cargo build --release -p rocketry-cli --locked
./target/release/rocketry --demo
```

The labeled demo makes no model API calls. To exercise its model–tool loop with workspace inspection:

```sh
rocketry --demo --backend host
rocketry --demo --backend host run "Inspect this workspace" --json
```

## Providers and environment discovery

Export credentials in the shell **before** starting Rocketry. Nonempty `OPENAI_API_KEY` and `ANTHROPIC_API_KEY` automatically register matching provider and agent profiles. Ollama is available as a keyless local profile; an optional `OLLAMA_API_KEY` is used as a bearer token. Keys are retained as environment-variable references, never displayed in the TUI or written to configuration.

| Provider profile | Credential | Default model | Model override | Endpoint override |
| --- | --- | --- | --- | --- |
| `openai` | `OPENAI_API_KEY` | `gpt-4.1` | `OPENAI_MODEL` | `OPENAI_BASE_URL` (default `https://api.openai.com/v1`) |
| `anthropic` | `ANTHROPIC_API_KEY` | `claude-sonnet-4-5` | `ANTHROPIC_MODEL` | `ANTHROPIC_BASE_URL` (default `https://api.anthropic.com/v1`) |
| `ollama` | Optional `OLLAMA_API_KEY` | `qwen3:8b` | `OLLAMA_MODEL` | `OLLAMA_BASE_URL`, then `OLLAMA_HOST` (default `http://localhost:11434/v1`) |

```sh
# Set OPENAI_API_KEY in your shell, then:
rocketry --agent openai --model gpt-4.1

# Set ANTHROPIC_API_KEY in your shell, then:
rocketry --agent anthropic

# Start Ollama and pull the desired model separately, then:
OLLAMA_MODEL=qwen3:8b rocketry --agent ollama
```

Ollama uses OpenAI-compatible Chat Completions. Rocketry adds `http://` to a scheme-less Ollama host and appends `/v1` when absent. An Ollama key alone keeps the local endpoint; set `OLLAMA_BASE_URL` explicitly to use another compatible endpoint. Local access needs no key ([Ollama authentication](https://docs.ollama.com/api/authentication), [compatibility](https://docs.ollama.com/api/openai-compatibility)). Rocketry does not start Ollama, download models, source `.env` files, or validate credentials during discovery.

Without `rocketry.toml`, the `navigator` agent uses the first detected profile in this order: OpenAI, Anthropic, then explicitly configured Ollama (any of its listed variables). With none detected, startup selects the labeled demo; `/agent ollama` remains available. `--agent` overrides the default and `--demo` explicitly selects demo mode. A real provider failure never triggers demo or provider fallback.

Explicit `rocketry.toml` provider/agent entries and its default agent take precedence over discovery. For custom agents, Gemini, Docker, policies, MCP servers, or tuned limits, copy `rocketry.example.toml` to `rocketry.toml` and replace model placeholders. Existing files are never rewritten. Run `rocketry config check` and `rocketry doctor` to inspect configuration and credential presence. Remote connections use server profiles and credentials; local key discovery does not configure the server.

## Change models in the TUI

Type `/` in the composer for a filtered command menu. Use **↑/↓** to select, **Tab** to complete, and **Enter** to execute. Unknown commands stay local; `//path` sends a literal `/path` to the model.

- `/agent` opens profile selection; `/agent anthropic` changes the provider/agent profile.
- `/model` opens an editable model field; `/model <model-id>` sets the model directly for the current provider. Submit an empty field to restore its configured default.
- Model/profile changes open a fresh session. Finish the selected run or use `/new` before switching. Saved runs keep their model override, including after restart and resume.
- `rocketry --agent openai --model <model-id> run "Your task"` provides the same model selection in the shell. The ID must be supported by that provider; access is checked by the provider on the first request.

## What ships

- Native OpenAI Responses, Anthropic Messages, Gemini Generate Content, and OpenAI-compatible Chat Completions adapters; streaming text, tool calls, structured output, usage, and opaque continuation blocks.
- A durable model–tool loop with schema validation, bounded read concurrency, serialized workspace writes, explicit approvals, cancellation, deadlines, and shared turn budgets.
- SQLite WAL sessions, ordered events, transactional tool results, artifact storage, crash recovery, and reconciliation for uncertain external effects.
- Dynamic child agents and durable sequential/parallel/conditional workflows with joins, cached step results, and shared budgets.
- Durable session history, session working notes, long-term agent memory with SQLite full-text search, bounded automatic recall, and context compaction that retains the original transcript.
- File, patch, search, process, and memory tools; trusted-host and Docker execution; MCP stdio and Streamable HTTP clients.
- Responsive Mission Control TUI with slash commands, model selection, environment status, memory/context/tool panels, sessions, Markdown/code rendering, approvals, and local/remote operation.

## Terminal controls

| Key | Action |
| --- | --- |
| `/` / Ctrl+K | Slash suggestions / full command palette |
| Tab / Shift+Tab | Switch panes |
| Ctrl+N | New mission |
| Enter / Alt+Enter | Submit / newline |
| Ctrl+F | Search flight log |
| PgUp / PgDn | Scroll |
| Ctrl+C | Cancellation controls |
| Ctrl+Q | Exit controls |

| Slash command | Action |
| --- | --- |
| `/new` | New mission, preserving saved sessions |
| `/agent [profile]` | Choose the provider/agent profile |
| `/model [id]` | Change the actual model for the current provider |
| `/providers` | Credential presence and configured models; no secret values |
| `/context` | Prepared context, byte budget, recall size, and output limit |
| `/memory` | Session working notes and long-term agent memories |
| `/tools` | Available tools, effects, and approval policy |
| `/approve` | Review an exact action; Y allows once, N denies |
| `/resume` / `/cancel` | Resume a saved run / open cancellation controls |
| `/inspect` / `/expand` | Toggle inspector / expand tool results |
| `/search [text]` | Filter the flight log |
| `/motion` / `/help` / `/quit` | Motion preference / help / exit controls |

At 140+ columns, all three panes are visible. At 100–139 columns, the inspector becomes a drawer. Smaller terminals show one pane at a time. `NO_COLOR` disables color; terminals without true-color support use a 256-color palette.

## Memory and context management

The default data directory is `.rocketry` in the current working directory; use `--data-dir` to choose another location. Memory is scoped within that database.

- **Conversation history:** every session saves user, assistant, tool, and provider continuation messages. Continuing a session loads its history; `/new` starts a separate conversation.
- **Session working notes:** `session_memory_put/search/list/delete` maintain short-term notes scoped by session and agent. They survive restart and transcript compaction, and are recalled only in that same session.
- **Long-term memory:** `memory_put/search/list/delete` maintain durable facts by agent name across sessions. The existing agent memory namespace is preserved. Different agents have separate memory.

Ask the agent to remember or forget a fact, or manage memory directly from the CLI:

```sh
rocketry memory put response-style "Prefer concise answers with evidence"
rocketry memory list
rocketry memory search concise
rocketry memory forget response-style
rocketry memory --session SESSION_ID put next-step "Inspect the cache tests"
rocketry context --session SESSION_ID
```

Use `--agent` consistently when managing an agent's memory. CLI memory editing is local; remote sessions expose memory through the agent tools and the read-only `/memory` panel. Lists show the 64 most recently written entries per scope, with values previewed up to 4,096 characters. Writes accept keys up to 256 bytes and values up to 64 KiB. Forgetting removes both the record and its search index entry. Model-initiated memory writes and deletions follow the configured approval policy; direct CLI memory commands express the operator's intent.

Before each model turn, the harness reserves space for instructions, tool/output schemas, and a wire-format allowance, recalls permitted session notes and recent agent memories within a bounded allowance (at most 8 KiB), then compacts history to fit `limits.context_bytes`. Memory is marked as untrusted reference data; automatic recall requires an allowed memory read tool. Compaction preserves the initial request and recent assistant/tool groups, with excerpts from older messages. The complete transcript remains on disk. If essential recent context cannot fit, the run reports the budget error instead of dropping tool boundaries. Byte budgets are conservative limits, not provider-specific tokenization; `limits.output_tokens` controls the separate output token cap.

## Filesystem, shell, and tool controls

Discovered real-provider agents expose the built-in tool set and delegation. Custom TOML agents use their explicit tool lists.

| Tools | Behavior |
| --- | --- |
| `read_file`, `list_dir`, `search` | Bounded reads, directory listings, recursive literal search |
| `write_file`, `patch_file` | Write files or replace exactly one matching text block |
| `create_dir`, `move_file`, `remove_file` | Create directories, move a file without overwriting (same filesystem), delete one file |
| `execute` | Cancellable shell process with bounded output |
| `memory_*`, `session_memory_*` | Persistent facts and session working notes |
| `delegate` | Child agents with separate histories and shared execution budgets |
| `mcp_<server>_<tool>` | Tools discovered from configured MCP servers |

Workspace execution starts disabled. Use `rocketry --backend host` to enable trusted host execution, or configure Docker isolation. Host file paths must stay within the workspace; shell execution is trusted host access, not a security sandbox. Reads follow `allow_reads`; writes, process execution, and external effects request approval unless explicitly allowed. Cancellation propagates to child work. Uncertain external effects require reconciliation before resuming.

## Server and remote TUI

```sh
# Set ROCKETRY_TOKEN to a randomly generated token of at least 16 characters.
ROCKETRY_TOKEN="$(openssl rand -hex 32)" ./target/release/rocketry serve
# In another terminal, set the same ROCKETRY_TOKEN, then:
./target/release/rocketry --connect http://127.0.0.1:8787
```

The server binds to localhost by default. Every operational endpoint requires bearer authentication; `/health` is public. Server workspace tools always use Docker with isolated per-root-run directories. Reverse-proxy TLS is required if exposing the service beyond the local host. Only one process opens a database; other clients attach to its server.

The API offers sessions, messages, runs, events, SSE streams, approvals, reconciliation, workflows, and artifacts under `/v1`. `GET /v1/agents/{id}/context?session=SESSION_ID` provides the same memory, tool, and context inspection used by the remote TUI; `POST /v1/runs` accepts an optional `model` override. Fetch `/v1/openapi.json` with authentication or run `rocketry openapi`. SSE supports `Last-Event-ID`; the TUI's remote reader uses the equivalent durable event cursor endpoint.

```sh
rocketry --demo workflow examples/workflow.json --input '"a reliable agent"'
rocketry sessions
rocketry sessions --export SESSION_ID
rocketry resume RUN_ID
rocketry --connect http://127.0.0.1:8787 cancel RUN_ID
rocketry reconcile RUN_ID --call CALL_ID --result '{"operator_verified":true}'
```

A noninteractive local run stops when approval is required; resume it interactively, or configure specific tool allowances using the policy or `--allow-tool TOOL`. There is no blanket auto-approve switch.

## Embed and extend

See [`crates/runtime/examples/sdk.rs`](crates/runtime/examples/sdk.rs) for a runnable example. Implement `ModelProvider`, `Tool`, `ExecutionBackend`, or `ApprovalPolicy` from `rocketry-core`. `SessionStore` defines the persistence contract; this release's `Harness` uses the bundled SQLite `Store` for its transactional recovery operations.

Native tool schemas are validated at startup and arguments before execution. MCP names are `mcp_<server>_<tool>`. MCP tools are external effects and require explicit policy decisions; their permissions do not come from Docker isolation.

## Verify

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo run --release -p rocketry-runtime --example runtime_benchmark
cargo run --release -p rocketry-tui --example tui_benchmark
python3 scripts/pty_smoke.py target/debug/rocketry
python3 scripts/http_smoke.py target/debug/rocketry
python3 scripts/provider_smoke.py target/debug/rocketry
# Requires Docker:
cargo test -p rocketry-tools --test transports -- --ignored
```

See [architecture and operating limits](docs/architecture.md) and [verification evidence](docs/verification.md). Local fixture coverage does not establish live provider compatibility with every model or production load behavior.
