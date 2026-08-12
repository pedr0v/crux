# crux

## What crux is

crux is an MCP server for AI coding agents. It reads a [SCIP](https://github.com/sourcegraph/scip) index of your code. It answers these questions in compact text:

- Where is this symbol?
- Who uses this symbol?
- Who calls this function?
- Which exports are not used?

The agent does not read full files to find these answers. Thus the agent uses fewer tokens.

We measured the results on large codebases (600+ files). Navigation tasks used 33% fewer tokens. Caller-graph queries used 64% fewer tokens. On small repositories (fewer than approximately 200 files), grep is sufficient. crux is made for the codebases where grep output is too large. The section [Measured results](#measured-results) shows the full data.

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

## Updates

Run `crux self-update` to install the latest standalone version.
Run `crux self-update --check` to check for an update without an installation.
Halv updates its bundled crux through the app updater.

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

## How agents discover crux

The server sends tool descriptions and instructions through the MCP protocol. Agents such as Claude Code and Codex read this information automatically. The user does not write a prompt.

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

## Measured results

We ran eight controlled A/B experiments. Each experiment used two agents with the same task on the same codebase. One agent used crux. One agent used grep and file reads. We measured the total agent tokens. Lower is better.

| Task | Codebase | Tokens with crux | Tokens with grep | Difference |
| --- | --- | --- | --- | --- |
| Fix two planted bugs | 136 files, 112k LOC | 43,613 | 46,503 | −6% |
| Reference map | 251 files, 65k LOC | 25,967 | 29,824 | −13% |
| Reference map | 77 files, 193k LOC | 33,551 | 39,872 | −16% |
| Reference map | 601 files, 1.5M LOC | 25,975 | 38,886 | **−33%** |
| Caller graph, depth 2 | 251 files | 29,522 | 82,089 | **−64%** |
| Dead-export audit | 230 exports | 42,179 | 53,388 | −21% |

Notes on the data:

- The large codebase is the TypeScript compiler. The 136-file codebase is a production React app.
- Each cell is one run. Treat differences below 10% as noise.
- Answer quality was equal in all runs. Independent graders verified each answer against precomputed ground truth.

## crux and Halv

crux has the MIT license. crux is fully local. It operates on one repository in one session. All operations occur on your machine. crux sends no telemetry.

[Halv](https://halv.ai) includes crux. Halv adds these functions: cache between sessions, measurement of saved tokens, multi-repository indexes, and hosted indexes for teams.
