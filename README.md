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

Run this command:

```sh
claude mcp add --scope user crux -- /path/to/crux
```

Use `--scope user`. This scope makes crux available in every directory. Without the flag, Claude Code uses the `local` scope. The `local` scope binds the server to the current directory only. The server then does not appear in sessions that start in other directories.

No other step is necessary. The agent creates the index automatically on first use. The first query in a large project takes longer because it builds the index.

## Configuration for Codex CLI

Add these lines to `~/.codex/config.toml`:

```toml
[mcp_servers.crux]
command = "/path/to/crux"
```

This file is global. Codex reads it in every directory, so no scope flag is necessary.

No other step is necessary. The agent creates the index automatically on first use.

## How agents discover crux

The server sends tool descriptions and instructions through the MCP protocol. Agents such as Claude Code and Codex read this information automatically. The user does not write a prompt.

## Profiles

The `slim` profile is the default. It advertises the four consolidated tools and `scip_expand`.
Call `scip_expand` when an agent needs a finer-grained tool. The call adds five narrow tools and sends an MCP tool-list change notification.

Start crux with the complete tool surface when you need it immediately:

```sh
crux --profile full
```

You can also set `CRUX_PROFILE=full`. The `--profile` flag overrides the environment variable.
The `full` profile still advertises `scip_expand`. The tool returns `already expanded` in this profile.

## Tools

| Tool | Function |
| --- | --- |
| `scip_index` | Finds the project language. Creates or refreshes the SCIP index. |
| `scip_find` | Finds symbol candidates by name. It can find unreferenced symbols. |
| `scip_map` | Shows definitions, signatures, reference sites, and callers for a maximum of eight known symbols. |
| `scip_outline` | Shows the definition structure of one file. |
| `scip_expand` | Adds the narrow tools to a slim server session. |

The `full` profile and `scip_expand` add these narrow tools:

| Tool | Function |
| --- | --- |
| `scip_search` | Finds symbols by name. Shows compact matches. |
| `scip_def` | Shows definitions with signatures, documentation, and source lines. |
| `scip_refs` | Shows reference lines for one symbol, in groups by file. |
| `scip_callers` | Shows the direct or transitive callers of one function. |
| `scip_dead` | Shows exports with no references in other files. Use it before you delete code. |

## Migration to 0.6.0

Version 0.6.0 uses the four consolidated tools by default. The narrow tools remain available through `scip_expand` or the `full` profile.

| Old call | New call |
| --- | --- |
| `scip_index { project_root, language?, max_file_mb? }` | Unchanged. |
| `scip_search { project_root, query, limit? }` | `scip_find { project_root, name: query, limit? }` |
| `scip_def { project_root, name }` | `scip_find { project_root, name }` |
| `scip_refs { project_root, name, limit? }` | Run `scip_find`, then `scip_map { project_root, names: [qualified_name], ref_limit: limit? }`. |
| `scip_callers { project_root, name, depth?, limit? }` | Run `scip_find`, then use the same `scip_map` shape. |
| `scip_dead { project_root, path_prefix?, limit?, exports_only? }` | `scip_find { project_root, name: "*", unreferenced: true, limit? }` |
| `scip_map { project_root, names, refs_limit? }` | `scip_map { project_root, names, ref_limit? }` |
| `scip_outline { project_root, file }` | Unchanged. |

`scip_map` no longer accepts `context` or `include_imports`.
The new map filters import and re-export sites.
It reports direct callers and has no caller depth option.
The unreferenced query has no path or export filter.

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
