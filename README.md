# crux

## What it is

crux is an MCP server that answers “where is X?”, “who uses X?”, “who calls X?”, and “what’s dead?” from a [SCIP](https://github.com/sourcegraph/scip) index in compact plain text, so AI coding agents stop burning tokens reading whole files.

On large codebases (600+ files), measured results are −33% tokens on navigation tasks and −64% on caller-graph queries versus grep-driven agents. On small repositories (under roughly 200 files), plain grep is already efficient—crux is built for the codebases where grep drowns.

## Install

Download the prebuilt binary for your platform from [GitHub Releases](https://github.com/pedr0v/crux/releases):

| Platform | Binary |
| --- | --- |
| macOS (Apple Silicon) | `crux-aarch64-apple-darwin` |
| macOS (Intel) | `crux-x86_64-apple-darwin` |
| Linux (x86_64) | `crux-x86_64-unknown-linux-gnu` |
| Linux (ARM64) | `crux-aarch64-unknown-linux-gnu` |
| Windows (x86_64) | `crux-x86_64-pc-windows-msvc.exe` |

Or install from source:

```sh
cargo install --git https://github.com/pedr0v/crux
```

## Setup for Claude Code

```sh
claude mcp add crux -- /path/to/crux
```

Then ask Claude to run `scip_index` once per project.

## Setup for Codex CLI

Add this to `~/.codex/config.toml`:

```toml
[mcp_servers.crux]
command = "/path/to/crux"
```

## Tools

| Tool | What it does |
| --- | --- |
| `scip_index` | Detect the project language and create or refresh its SCIP index. |
| `scip_map` | Bundle definitions, signatures, and contextual reference sites for up to eight symbols. |
| `scip_search` | Search symbol names and return compact, disambiguated matches. |
| `scip_def` | Find definitions with signatures, documentation, and source lines. |
| `scip_refs` | Group a symbol’s reference lines by file. |
| `scip_outline` | Show a file’s definition skeleton. |
| `scip_callers` | Resolve direct or transitive callers using enclosing definitions. |
| `scip_dead` | Find exports with no cross-file references before you delete them. |

## Language support

| Language | Indexer | Requirement |
| --- | --- | --- |
| TypeScript / JavaScript | `scip-typescript` via `npx` | Node.js and `npx` |
| Python | `scip-python` via `npx` | Needs Node.js ≤22 today; big vendored trees need a `pyrightconfig.json` exclude |
| Rust | `rust-analyzer scip` | `rustup component add rust-analyzer` |
| Dart / Flutter | `scip_dart` via `dart pub global` | `dart pub global activate scip_dart` |
| Java / Kotlin | `scip-java` via Coursier | Install [Coursier](https://get-coursier.io/) |
| C / C++ | `scip-clang` | A `compile_commands.json` compilation database |

The index lives at `<project>/.scip-nav/index.scip`. Put `.scip-nav/` in your global gitignore.

## crux and Halv

crux is MIT and fully local—one repo, one session, everything runs on your machine, with no telemetry. Cross-session caching, real savings metering, multi-repo, and team/hosted indexes ship inside [Halv](https://halv.ai), which bundles crux.
