# Architecture and operating contract

## Crates

`core` defines the SDK contracts and versioned events. `providers` translates streaming HTTP protocols. `store` owns SQLite transactions and artifacts. `tools` owns workspace backends and MCP clients. `runtime` owns policies, budgets, recovery, delegation, and workflows. `server` exposes REST/SSE. `tui` projects events into Mission Control; `cli` loads configuration and starts the appropriate interface.

Provider and tool implementations never depend on terminal code. The runtime emits bounded, coalesced text events; consumers read durable cursors without creating unbounded in-memory subscriber queues. Slow SSE clients cannot block agent progress. Completed transcript cards cache their wrapped layout; only visible rows are submitted to the renderer.

## Execution and recovery

A model response is streamed for display, but its tool calls are only executable after successful completion and schema validation. Assistant messages persist before tool execution. Before invoking a tool, the store commits an execution record. The result and corresponding tool message commit together. Run status and its event also commit together, preventing an SSE client from observing completion before the terminal event exists.

A crash during an external effect can leave its outcome unknown. Such a run becomes `needs_reconciliation`; Rocketry does not claim exactly-once external execution. The operator supplies a verified result using the reconciliation API or CLI, then resumes. Approved argument objects remain bound to their original call IDs and survive an interrupted approval wait.

Safe resumption uses persisted assistant/tool boundaries, not a regenerated tool call. Partial provider streams are not added as complete assistant messages. Transient HTTP 429/5xx responses retry before streaming begins; partially consumed streams are not automatically replayed.

The original transcript remains in SQLite. Context compaction retains the initial user mission, recent complete assistant/tool groups, and short excerpts from older messages. Provider continuation blocks in retained groups are preserved. This is deterministic excerpt compaction, not model-generated semantic summarization.

## Delegation and workflows

Dynamic child agents have separate sessions, explicit inputs/results, inherited tool ceilings, shared turn accounting, and cascading cancellation. Empty tool lists grant no tools; `*` explicitly grants the registered tool set and delegation. Policy remains authoritative even when a tool is registered or inherited.

Workflow coordinators persist their definitions and leaf execution records. Sequential outputs feed the next step through `{{input}}`; parallel joins preserve branch order. A conditional step chooses a branch using a literal substring of its serialized input. Completed step outputs are reused when a coordinator resumes. Uncertain child effects must be reconciled first. Graph nesting is limited to 32 levels; sequences to 100 steps; parallel width uses the configured child limit.

All workflow leaves and dynamically delegated agents share the root turn budget. Each active coordinator or agent consumes a global run slot. Admission fails explicitly when capacity is full; there is no distributed queue.

## Boundaries and limits

This is a single-operator/team installation, not a multi-tenant security service. Host mode is trusted execution and is not an OS sandbox. Docker is required for server workspace tools: non-root user, read-only root filesystem, no network, dropped capabilities, no-new-privileges, 1 CPU, 256 MiB memory, 64 PIDs, and a 32 MiB temporary directory. Only the run workspace is mounted; provider credentials and the Docker socket are not passed through.

MCP servers are explicitly configured integrations. A stdio server runs on the host with a restricted environment; remote MCP actions execute wherever that server runs. Neither is placed inside the workspace sandbox. Tools default to external, serial actions and require policy approval. A failed external request is not retried automatically. Connection establishment and discovery have bounded deadlines. Interactive OAuth is deferred.

Default limits: 100 active runs, 4 parallel read tools per run, 8 child agents per parent, delegation depth 3, 50 shared model turns, 900 seconds per execution attempt, 4,096 output tokens per model call, 128 KiB context, and 32 KiB tool-result previews. Process tools have a 120-second timeout and retain at most 2 MiB per output stream while draining excess output. File operations accept at most 2 MiB. Search visits at most 10,000 entries and returns at most 200 matches; directory listing returns at most 1,000 entries. Large retained tool results become artifacts.

Usage reports are provider-reported counts. Price estimates require explicitly configured input/output prices; missing usage or pricing remains unavailable. No automatic model pricing lookup or dollar billing guarantee is provided.

## Provider compatibility

OpenAI uses `/responses`, Anthropic `/messages`, Gemini `/models/<model>:streamGenerateContent`, and compatible endpoints `/chat/completions`. Base URLs include the API version prefix. No provider is substituted on failure. Changing provider profiles requires a new session.

Native structured-output requests are passed through and final JSON is validated locally. A model or compatible endpoint may reject a feature it does not implement; Rocketry reports the provider error. Native server-executed tools, audio/video, computer use, provider-specific realtime protocols, and interactive OAuth are outside this release.

Protocol references: [OpenAI Responses](https://developers.openai.com/api/docs/guides/migrate-to-responses), [Anthropic streaming](https://platform.claude.com/docs/en/build-with-claude/streaming), [Anthropic structured output](https://platform.claude.com/docs/en/build-with-claude/structured-outputs), [Gemini function calling](https://ai.google.dev/gemini-api/docs/function-calling), and the [official MCP Rust SDK](https://github.com/modelcontextprotocol/rust-sdk). Dependency versions are pinned in `Cargo.lock`.

## Operations

One process holds an exclusive lock for each data directory. Operational endpoints require a bearer token; TLS is delegated to a reverse proxy for remote deployment. Shutdown cancels active trees and persists their recoverable state. The server provides health, readiness, active-run/capacity metrics, and structured runtime logs that omit request bodies and credentials.

SQLite and artifacts are local files and should live on local storage. Back up the complete data directory while the process is stopped. The schema migrates transactionally and rejects databases from newer versions. No distributed workers, account system, browser dashboard, vector index, automatic deployment, or public package publication is included.
