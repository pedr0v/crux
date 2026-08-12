# crux

## What crux is

crux is an MCP server for AI coding agents. It reads a [SCIP](https://github.com/sourcegraph/scip) index of your code. It answers these questions in compact text:

- Where is this symbol?
- Who uses this symbol?
- Who calls this function?
- Which exports are not used?

The agent does not read full files to find these answers. Thus the agent uses fewer tokens.

We measured the results on large codebases (600+ files). Navigation tasks used 33% fewer tokens. Caller-graph queries used 64% fewer tokens. On small repositories (fewer than approximately 200 files), grep is sufficient. crux is made for the codebases where grep output is too large.

## Installation

Download the binary for your platform from [GitHub Releases](https://github.com/pedr0v/crux/releases):

| Platform | Binary |
| --- | --- |
| macOS (Apple Silicon) | `crux-aarch64-apple-darwin` |
| macOS (Intel) | `crux-x86_64-apple-darwin` |
| Linux (x86_64) | `crux-x86_64-unknown-linux-gnu` |
| Linux (ARM64) | `crux-aarch64-unknown-linux-gnu` |
| Windows (x86_64) | `crux-x86_64-pc-windows-msvc.exe` |

As an alternative, build crux from source:

```sh
cargo install --git https://github.com/pedr0v/crux
```

## Configuration for Claude Code

1. Run this command:

```sh
claude mcp add crux -- /path/to/crux
```

2. Tell Claude to run `scip_index` one time in each project.

## Configuration for Codex CLI

1. Add these lines to `~/.codex/config.toml`:

```toml
[mcp_servers.crux]
command = "/path/to/crux"
```

2. Tell Codex to run `scip_index` one time in each project.

## Tools

| Tool | Function |
| --- | --- |
| `scip_index` | Finds the project language. Creates or refreshes the SCIP index. |
| `scip_map` | Shows definitions, signatures, and reference sites for a maximum of eight symbols in one call. |
| `scip_search` | Finds symbols by name. Shows compact matches. |
| `scip_def` | Shows definitions with signatures, documentation, and source lines. |
| `scip_refs` | Shows the reference lines for one symbol, in groups by file. |
| `scip_outline` | Shows the definition structure of one file. |
| `scip_callers` | Shows the direct or transitive callers of one function. |
| `scip_dead` | Shows the exports that have no references in other files. Use it before you delete code. |

## Language support

| Language | Indexer | Requirement |
| --- | --- | --- |
| TypeScript / JavaScript | `scip-typescript` through `npx` | Install Node.js. |
| Python | `scip-python` through `npx` | Install Node.js version 22 or lower. For large vendored directories, add an exclude list to `pyrightconfig.json`. |
| Rust | `rust-analyzer scip` | Run `rustup component add rust-analyzer`. |
| Dart / Flutter | `scip_dart` | Run `dart pub global activate scip_dart`. |
| Java / Kotlin | `scip-java` | Install [Coursier](https://get-coursier.io/). |
| C / C++ | `scip-clang` | Supply a `compile_commands.json` compilation database. |

crux writes the index to `<project>/.scip-nav/index.scip`. Add `.scip-nav/` to your global gitignore file.

## crux and Halv

crux has the MIT license. crux is fully local. It operates on one repository in one session. All operations occur on your machine. crux sends no telemetry.

[Halv](https://halv.ai) includes crux. Halv adds these functions: cache between sessions, measurement of saved tokens, multi-repository indexes, and hosted indexes for teams.
