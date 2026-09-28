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

The fixture has four modules and three directed dependencies.
It has one two-module cycle and one isolated module.
The test checks output against this fixture truth.

This table records one local run on 2026-09-28:

| Path | Calls | Output bytes | Correctness result | Observed latency |
| --- | ---: | ---: | --- | ---: |
| Architecture overview | 1 | 1,215 | Reported all four fixture counts | 407,861 us |
| Scoped architecture | 1 | 901 | Reported three direct edges and the relevant cycle | 561 us |
| Scripted four-outline and one-map baseline | 5 | 709 | Supplied evidence for manual dependency and cycle inference | Not recorded |

Nine extra cached scoped calls had a median latency of 410 us.
The first overview can include server startup, index loading, and projection construction.
Latency depends on the machine and file cache.
The baseline call count describes one script.
It does not establish the minimum call count for existing tools.

The scoped architecture payload was 192 bytes larger than the scripted baseline.
This run does not demonstrate a payload saving.
The symbol map can support a manual cycle inference without a cycle heading.

Output bytes measure returned UTF-8 text.
They are not token measurements.
The evaluation does not claim agent savings from payload size.
It excludes `scip_architecture` from the existing savings ledger.

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

## Limits

The result includes only definitions and references in the loaded SCIP index.
Missing runtime, generated, or reflective references remain absent.
Malformed, unknown, and ambiguous references do not create dependency edges.
The tool reports those cases as coverage diagnostics.
An index refresh invalidates the cached architecture projection.
