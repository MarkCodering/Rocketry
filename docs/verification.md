# Verification evidence

Local verification on 2026-09-05 used an Apple M2 (aarch64), 8 GiB RAM, macOS 26.6.2, and Rust 1.98.0. Benchmarks used release builds. These results describe deterministic local workloads, not cloud model throughput.

## Checks

| Check | Result |
| --- | --- |
| `cargo fmt --all --check` | Passed |
| Clippy, workspace and all targets, warnings denied | Passed |
| Workspace tests, all features, locked dependencies | 27 passed; Docker test separately exercised |
| Core and runtime with no default features | Passed |
| Release CLI build | Passed |
| MCP stdio and Streamable HTTP discovery and calls | Passed |
| Docker workspace tools and isolation | Passed with Docker 29.6.2 and `python:3.13-slim` |
| Real server and remote CLI | Authentication, completed run, SSE cursor reconnect, OpenAPI, metrics passed |
| Real PTY: 80×24, 120×40, 180×50 | Unicode bracketed paste, resizing, completion, cancellation, and terminal restoration passed |
| Deterministic TUI screens | Golden snapshots passed at all three sizes |

Runtime fixtures exercise fragmented/interleaved tool streams, malformed arguments, provider failures and throttling, context bounds, approvals, cancellation, crash recovery, uncertain effects, delegation, and workflow joins/resume. They use local mock servers and deterministic providers. Docker checks exercise actual file operations, path escape rejection, disabled networking, and absence of provider credentials and the Docker socket.

The representative [Mission Control image](mission-control.png) is generated from the real Ratatui view with a labeled visual fixture. Its displayed usage values are fixture content, not benchmark measurements. Exact text snapshots are checked into `crates/tui/src/snapshots/`. The PTY script writes raw terminal captures and its report into `target/pty/`.

## Performance

| Measurement | Local result | Initial target |
| --- | ---: | ---: |
| p95 in-memory event dispatch | 0.000084 ms | < 2 ms |
| p95 durable run scheduling | 1.700 ms | < 20 ms |
| p95 TUI frame preparation, 180×50 | 0.455 ms | < 16 ms |
| Incremental RSS, 100 mocked runs | 8.03 MiB | < 256 MiB |

The runtime benchmark starts 100 mock runs with bounded 32 KiB contexts; it completed in 0.110 seconds (910 mock runs/second). In-memory dispatch measures a bounded channel round trip. Durable scheduling measures `Harness::start` including SQLite persistence. RSS is the sampled difference from the process baseline, not a peak-memory guarantee. The TUI benchmark prepares frames using Ratatui's `TestBackend`, cached layout, and 1,004 transcript cards; it excludes terminal I/O. Model/network latency and container startup are excluded from these acceptance measurements.

Reproduce from the workspace root:

```sh
cargo build --release -p rocketry-cli --locked
cargo run --release -p rocketry-runtime --example runtime_benchmark --locked
cargo run --release -p rocketry-tui --example tui_benchmark --locked
python3 scripts/pty_smoke.py target/release/rocketry
python3 scripts/http_smoke.py target/release/rocketry
cargo test -p rocketry-tools --test transports --locked -- --ignored
```

The Docker image used locally had digest `sha256:9d2e5553305c7c7b0097999bb17187c69b921ccd6bc9d40e4bb5ebe652c00285`. The image reference is configurable; pin a digest when reproducible container environments are required.

## Remaining external validation

No live cloud-provider requests were made. Native adapters are covered by protocol fixtures; configure concrete model IDs and credentials to validate the selected live models. Usage is provider-reported when available, while configured prices only produce estimates. Missing usage remains unavailable.

Linux/macOS CI and the Linux Docker job are configured in `.github/workflows/ci.yml`; hosted CI has not been run from this local workspace. The local results above establish macOS execution. Production load, every remote MCP server's behavior, and every model's capabilities remain outside this fixture evidence.
