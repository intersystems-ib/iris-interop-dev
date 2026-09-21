# Working on iris-interop-dev

Rules for anyone — human or agent — changing this repo. ObjectScript-specific guidance lives in
[`light-skills/AGENTS.md`](light-skills/AGENTS.md); this file is about the Rust server itself and,
mostly, about one bug.

## The rule: a failure must never be answered with a negative fact

This is the defect class this codebase produces more than any other. It is not a crash and it has no
symptom. It is a call that **could not do its job** and returned something shaped exactly like an
answer:

- a dead connection → an empty list, so the caller concludes "there are none"
- an unparseable response → `success: true` with zero rows
- a 401 → "not found"
- a poisoned lock → the entry dropped, but the id already handed back
- an expired token → "never existed"
- a filter the server rejected → no filter at all, and the query still reports success

Every one of those is a *statement about the world* invented to stand in for a failure. Nothing logs,
nothing reddens, and the caller acts on a fact that was never established. Because this is an MCP
server, the blast radius is a model: it reads `count: 0` as ground truth and tells the user their
class has no methods.

### Why this repo specifically

An empty result is a **legitimate answer** to almost every tool here — no matching classes, no
messages in the log, no items in the production. So the failure mode is camouflaged by design. In a
codebase where "nothing found" were impossible, these bugs would be obvious.

### The shapes, and how to find them

These are the constructs that turn a failure into a fact. None is wrong everywhere; each is wrong
when the `Err`/`None` it swallows means *we could not tell*.

```bash
rg 'unwrap_or_default\(\)'      crates/*/src --type rust   # Err → "", 0, empty Vec
rg '\.ok\(\)\?'                 crates/*/src --type rust   # Err → None → "not found"
rg 'if let Ok\('                crates/*/src --type rust   # Err → branch silently skipped
rg 'unwrap_or\((0|false|"")\)'  crates/*/src --type rust
rg '\.is_empty\(\)'             crates/*/src --type rust   # "no output" vs "output was blank"
```

Run these instead of trusting any list of known cases — the list below is a starting set, not a
census, and it was already incomplete when it was written.

### The repair is always a third case

Not success, not absence — **unavailable**, as its own variant, carrying the reason:

```rust
enum Listing {
    Found(Vec<String>),
    Empty,                      // the server answered, and the answer is "none"
    Unavailable { status: Option<u16>, detail: String },   // we could not tell
}
```

Two arms cannot express the difference, so no amount of care at the call site helps. Three arms make
the compiler enumerate the call sites for you. `ActionMsg` in `tools/scm.rs` and `ExpandedTargets` /
`ListingUnavailable` in `tools/wildcard.rs` are the worked examples.

**Fix the sibling too.** This defect travels in pairs — report vs enforce, assert vs analyse, the
reader that recovers vs the writer that drops. When you find one, grep for the same call shape in the
neighbouring function before closing the issue: fixing only the one you found leaves the siblings
looking *more* trustworthy than they are.

### Known instances

Each of these was this bug. None had a reported symptom; all were found by reading.

| | |
|---|---|
| #15 | `iris_execute` ignored `IRIS_NAMESPACE` and silently ran in `USER` |
| #17 | `iris_search` with no `files=` scope searched nothing, successfully |
| #78 | a wrong parameter name returned success with an empty index |
| #83 | `iris_get_log` advertised `limit`/`offset` and silently ignored them |
| #105 | a 200 with a non-JSON body → `success: true`, zero rows |
| #106 | a 401 → a confident false negative, which `iris_generate` then acted on |
| #116 | a document could not be read back in the session that wrote it — reported absent |
| #119 | `production=` ignored; `set_settings` silently edited the *running* production |
| #143 | `body_select` silently dropped — joined, filtered, then never projected |
| #164 | one quoted `FormalSpec` zeroed the whole result set, `success: true, count: 0` |
| #166 | `NO_TESTS_FOUND` carried `failed: 0`, which reads as a passing run |
| #202 | a rejected `search_table=` degraded into NO filter, and still reported success |
| #255 | 8 non-ignored tests self-skipped on `IRIS_HOST` and printed `ok` |
| #301 | a poisoned lock dropped the log entry after handing out its id |
| #302 | unparseable IRIS output → `success: true`, error text discarded |
| #305 | any lookup failure → `ELICITATION_EXPIRED`, including never-existed |
| #312 | absent / unreadable / unparseable config all collapsed to `None` |
| #313 | a wildcard reached the CLI with none of the tool's guards |
| #320 | an unrecognised `SystemMode` let writes through on a live instance |

## Measurement discipline

The same habit of mind catches both. **A zero, an empty result, or silence is not evidence until the
same path has produced a non-zero.**

- **Assert that the check RAN**, not just that it passed. `FAILED: 0` over an empty log is
  indistinguishable from a pass. Print the denominator: the count of `test result:` lines, the number
  of targets asked for, `N passed; M filtered out`.
- **A test-runner exit code is not a verdict.** `cargo test` exiting non-zero can mean "bad
  arguments" as easily as "a test failed".
- **Pair every negative sweep with one probe you know hits.** A `grep` with a broken pattern and a
  `grep` with nothing to find print the same thing.
- **Never hand-roll a character class to enumerate names.** `[a-z_]+` silently drops `e2e`, `hl7`,
  `x12`, `sha256`. Use `\S+` and let the delimiters do the work; over-matching is visible,
  under-matching is not.
- **Never restate a count in prose.** Every count written into a comment or doc in this repo has gone
  stale, in both directions. Name the command that prints it, or assert it in a test that executes
  the derivation.

## The gate

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
env -u IRIS_HOST -u IRIS_CONTAINER \
  bash -c 'cargo test --workspace --lib --bins $(bash scripts/ci-test-targets.sh) --no-fail-fast'
```

Three things that have each produced a false verdict here:

1. **Run the test step under `bash`.** `scripts/ci-test-targets.sh` emits `--test a --test b …` as a
   string to be word-split. bash splits it into 90 arguments; **zsh does not split it at all**, so
   cargo receives one giant argument, rejects it, and runs zero tests.
2. **`cargo build --workspace` before any e2e run.** The e2e tests spawn
   `target/debug/iris-interop-dev` from the *other* package, and `cargo test` will not rebuild
   another package's binary for them. A stale binary has given a false verdict more than four times.
3. **Unset the IRIS env.** With `IRIS_HOST` set, tests that are supposed to exercise the
   no-connection path acquire a real connection and assert the opposite of their names.

CI's `e2e-tests` job runs only on `push` to master and on `workflow_dispatch` — **a `pull_request`
run skips it**. Validate a branch with `gh workflow run CI --ref <branch>`.

## Changing behaviour

- **Mutation-check every new assertion.** Break the thing it guards, watch it go red, restore it,
  confirm it goes green. Print "applied" when the mutation lands: a mutation that never applied and a
  genuinely surviving mutant print identical output.
- **A mutation must change behaviour, not a string an assertion greps for.** Several tests here
  necessarily assert on the *text* of generated ObjectScript. A mutation that inserts the exact
  literal such an assertion forbids is circular — its red is guaranteed by construction, and it is
  evidence about the assertion, not the code. The way this was found: a loop truncation spelled
  `for i = 1:1:1` went red against a test asserting `!code.contains("1:1:1")`, while the *same
  truncation* spelled `if n > 1 { set n = 1 }` survived. If you cannot think of a second spelling for
  your mutation, the assertion is grepping rather than testing.
- **Prove the harness can kill before you believe a survival.** Apply one mutation you are certain is
  fatal and watch it go red. Only then trust a "survived" result. A survival without that control is
  worth exactly as much as a clean zero without one.
- **A surviving mutant names a missing assertion**, usually in the knob you were most confident of.
- **Check whether two inputs to a conjunction ever vary independently.** If every fixture that sets
  one also sets the other, both halves are jointly unpinned and dropping either survives — and the
  half that survives is usually the false-green direction.
- **Existence is not reachability.** `pub` plus four unit tests does not mean a function runs. Before
  trusting behaviour you read, ask who calls it, and whether anything in production constructs its
  inputs — a constructor check is stronger than a call-site grep. One builder in this crate has four
  tests pinning a vocabulary the shipped path cannot produce.
- **The reason you write beside a fix is a hypothesis.** The fix working is evidence the symptom is
  gone, not that your explanation is right — and the explanation is the part that gets quoted later.
  Either run the experiment that separates it from the nearest rival cause, or write only what you
  changed and what you observed.
