# Architecture navigation

`scip_architecture` answers module and package questions from SCIP evidence.
It returns compact text from an existing local index.
It does not write reports or run an indexer.

## Routing guidance

Call `scip_architecture` once for these questions:

- What modules or packages depend on this scope?
- What does this scope depend on?
- Which dependency cycles include this scope?
- Which files can define the scope of a multi-file change?
- What is the broad indexed architecture?

Pass a known project-relative file or directory when possible.
Reuse a relevant result until the index changes.

Use `scip_map` for named-symbol references and callers.
Use `scip_outline` for symbols in one file.
Use `scip_find` for fragments or unreferenced audits.
Use `rg` for simple text or definition search.
Do not require an architecture query before each edit or symbol query.
After an empty result, use native search and stop equivalent retries.

## Query contract

The tool accepts `project_root`, `scope`, `direction`, `limit`, and `offset`.
The `direction` value is `incoming`, `outgoing`, or `both`.
Incoming edges show direct reference evidence and possible impact.
The tool does not compute transitive impact.

The scope matcher uses complete path components.
For example, `src/foo` matches `src/foo/mod.rs`.
It does not match `src/foobar/mod.rs`.

The result reports direct dependencies, boundaries, cycles, coverage, and diagnostics.
Scoped package boundaries aggregate only the selected direct edges.
The result marks section, line, and total output truncation.
The total text is at most 12,000 bytes.

## Design evidence

Graphify recommends a scoped graph query for codebase questions when a graph exists.
Its path and explain commands serve focused questions.
It reserves its report for broad questions or inadequate query results.
See [Graphify agent guidance](https://github.com/Graphify-Labs/graphify/blob/d6eaa8aae8df155874ebb1044302c055c286342a/graphify/always_on/agents-md.md).

Graphify installs persistent guidance and platform hooks.
Its Codex hook check is a no-op.
Its Claude integration can block reads in strict mode.
See [Graphify installation code](https://github.com/Graphify-Labs/graphify/blob/d6eaa8aae8df155874ebb1044302c055c286342a/graphify/install.py).

Crux uses persistent guidance without mandatory read blocking.
These sources support the routing design.
They do not prove performance or agent adoption.

## Reproducible evaluation

Run this command:

```sh
cargo test --locked --test architecture_navigation_eval -- --nocapture
```

The fixture is a hand-built SCIP index with four documents.
The documents are `src/api.rs`, `src/service.rs`, `src/database.rs`, and `src/isolated.rs`.
Each document defines one function and has at most three lines.
The fixture has four modules and three directed dependencies.
It has one two-module cycle and one isolated module.
The test checks output against this fixture truth.

The test prints one line that starts with `ARCHITECTURE_NAVIGATION_EVAL=`.
This line holds every measurement as JSON.
The test does not store the line. Copy it from the test output when you need it.

This table records one local run on 2026-10-20:

| Path | Calls | Output bytes | Correctness result |
| --- | ---: | ---: | --- |
| Architecture overview | 1 | 1,215 | Reported all four fixture counts |
| Scoped architecture | 1 | 901 | Reported three direct edges and the relevant cycle |
| Scripted four-outline and one-map baseline | 5 | 709 | Supplied evidence for manual dependency and cycle inference |

The test asserts the correctness results. It does not assert the exact byte counts.
The byte counts were the same in the runs of 2026-09-28 and 2026-10-20.
The baseline call count describes one script.
It does not establish the minimum call count for existing tools.

The scoped architecture payload was 192 bytes larger than the scripted baseline.
This run does not demonstrate a payload saving.
The symbol map can support a manual cycle inference without a cycle heading.

Output bytes measure returned UTF-8 text.
They are not token measurements.
The evaluation does not claim agent savings from payload size.
It excludes `scip_architecture` from the existing savings ledger.

### Routing controls

The test checks server guidance for three negative routing controls:

| Question | Expected tool |
| --- | --- |
| Who calls `service`? | `scip_map` |
| Which symbols are in `src/service.rs`? | `scip_outline` |
| Where does the literal `database` occur? | `rg` |

The test asserts the initialize guidance and relevant tool descriptions.
These checks verify the server routing contract.
They do not execute a model or measure live model choices.
A paired model evaluation must test adoption and task cost separately.

### Latency

The test measures latency but does not assert it.
Latency depends on the machine, the toolchain, and the file cache.
The first overview call loads the index and builds the architecture projection.
Later calls reuse the cached projection.

This run of 2026-10-20 used this environment:

- Machine: Apple M4 Pro, macOS 26.1 (arm64).
- Toolchain: rustc 1.97.1 and cargo 1.97.1.
- Build profile: the default `cargo test` profile, unoptimized with debug information.
- Fixture: the four-document fixture in this section.

| Call | Latency |
| --- | ---: |
| First architecture overview | 1,719 us |
| First scoped architecture | 340 us |
| Median of nine repeated scoped calls | 184 us |

An earlier run of 2026-09-28 recorded 407,861 us for the first overview and 561 us for the first scoped call.
The ratio between these two calls was about 700 in that run and about 5 in the run above.
Thus a single latency value does not support a comparison.
Do not use these values as a performance claim.

## Limits

The result includes only definitions and references in the loaded SCIP index.
Missing runtime, generated, or reflective references remain absent.
Malformed, unknown, and ambiguous references do not create dependency edges.
The tool reports those cases as coverage diagnostics.
An index refresh invalidates the cached architecture projection.
