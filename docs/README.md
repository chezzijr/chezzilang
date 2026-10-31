# Chezzi docs

Start with the [syntax reference](reference/syntax/01-lexical-and-literals.md). The project status is in [PROGRESS.md](../PROGRESS.md).

`chezzi docs <topic>` prints a reference topic (`spec`, `syntax`, `stdlib`, `concurrency`). `chezzi docs` with no topic prints all of them. The binary embeds the chapter files below, in file-name order.

## Reference (for people who write Chezzi)

| Topic | Chapters |
|---|---|
| Spec | [overview](reference/spec/01-overview.md) · [feature set](reference/spec/02-feature-set.md) · [imports and modules](reference/spec/03-imports-and-modules.md) · [semantics](reference/spec/04-semantics.md) · [conversions](reference/spec/05-conversions.md) · [architecture](reference/spec/06-architecture.md) |
| Syntax | [lexical and literals](reference/syntax/01-lexical-and-literals.md) · [variables and types](reference/syntax/02-variables-and-types.md) · [operators](reference/syntax/03-operators.md) · [functions](reference/syntax/04-functions.md) · [function values](reference/syntax/05-function-values.md) · [control flow](reference/syntax/06-control-flow.md) · [structs](reference/syntax/07-structs.md) · [generics](reference/syntax/08-generics.md) · [protocols](reference/syntax/09-protocols.md) · [protocol hooks](reference/syntax/10-protocol-hooks.md) · [enums and match](reference/syntax/11-enums-and-match.md) · [errors](reference/syntax/12-errors.md) · [testing and entry](reference/syntax/13-testing-and-entry.md) · [strings](reference/syntax/14-strings.md) · [pipe and concurrency](reference/syntax/15-pipe-and-concurrency.md) · [imports](reference/syntax/16-imports.md) · [FFI](reference/syntax/17-ffi.md) · [native declarations](reference/syntax/18-native-decls-and-example.md) |
| Stdlib | [builtins](reference/stdlib/01-builtins.md) · [methods](reference/stdlib/02-methods.md) · [runtime types](reference/stdlib/03-runtime-types.md) · [native core](reference/stdlib/04-native-core.md) · [native system](reference/stdlib/05-native-system.md) · [network and FFI](reference/stdlib/06-native-network-ffi.md) · [text and data](reference/stdlib/07-pure-text-data.md) · [concurrency utilities](reference/stdlib/08-pure-concurrency-utils.md) · [CLI and JSON](reference/stdlib/09-pure-cli-json.md) |
| Concurrency | [model](reference/concurrency/01-model.md) · [channel](reference/concurrency/02-channel.md) · [shared state](reference/concurrency/03-shared.md) · [timer and wait](reference/concurrency/04-timer-wait.md) · [cancellation](reference/concurrency/05-cancel.md) · [blocking](reference/concurrency/06-blocking.md) · [task boundary](reference/concurrency/07-sendability.md) · [executor and daemons](reference/concurrency/08-executor.md) |
| Grammar | [grammar.bnf](reference/grammar.bnf): the canonical grammar; `cargo test conformance` runs it against the parser |

## Contributing (for people who work on the compiler)

| File | Contents |
|---|---|
| [owners.md](contributing/owners.md) | The one owner of each fact, and the file for each topic. Read it before you search. |
| [code-conventions.md](contributing/code-conventions.md) | Code rules in full (CLAUDE.md has the one-line versions) |
| [dev-commands.md](contributing/dev-commands.md) | Build, test, lint and bench commands |
| [lessons.md](contributing/lessons.md) | Engineering rules learned from past defects |
| [special-cases.md](contributing/special-cases.md) | Special-case rules in the language and the owner decision for each |
| [gaps.md](contributing/gaps.md) | Open bug ledger |
| [backlog.md](contributing/backlog.md) | Open design and ergonomics items |
| [bug-discovery.md](contributing/bug-discovery.md) | Bug-hunt strategy and oracles |
| [bug-discovery-harnesses.md](contributing/bug-discovery-harnesses.md) | Fuzz, difftest, judge and schedfuzz harnesses |
| [benchmarks.md](contributing/benchmarks.md) | Current performance numbers and how to measure |
| [build-times.md](contributing/build-times.md) | Build-time measurements for the crate split |

## Design (open proposals and accepted decisions)

| File | Status |
|---|---|
| [roadmap.md](design/roadmap.md) | Open ideas, not scheduled |
| [concurrency-open.md](design/concurrency-open.md) | Open concurrency items |
| [concurrency-runtime.md](design/concurrency-runtime.md) | Scheduler and runtime internals |
| [killable-subprocess.md](design/killable-subprocess.md) | Proposal (M25), not implemented |
| [value-structs.md](design/value-structs.md) | Proposal, not started; §9 explicit value/ref structs is the owner-preferred route |
| [packed-storage.md](design/packed-storage.md) | Proposal, not started |
| [ffi-and-packaging.md](design/ffi-and-packaging.md) | Proposal, not started |
| [adr-d4-airlock.md](design/adr-d4-airlock.md) | Superseded 2026-10-29 by the shared heap (merged to `main` 2026-10-31) |
| [generics-and-carriers.md](design/generics-and-carriers.md) | Accepted and implemented |
| [go-shared-heap.md](design/go-shared-heap.md) | Done: M1 to M5, merged to `main` 2026-10-31 |

## History

[history/](history/README.md) keeps records of finished work: the dated progress log, bug-hunt waves, benchmark entries and shipped designs. These files are not maintained.
