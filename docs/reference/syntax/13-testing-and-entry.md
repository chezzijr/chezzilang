## 9c. Testing — `assert`, `test fn`, `chezzi test`

Chezzi has a built-in test facility in three pieces.

**`assert`** is a statement that faults with its source line when its condition is false:

```chezzi
assert x == 1                       # bare form
assert even_sum(10) == 20, "0..10"  # with a custom message
```

- `cond` must be `bool` and the optional `msg` must be `str` (checker-enforced). `msg` is evaluated
  only on failure.
- A passing assert does nothing. A failing one faults like any runtime error, with the line shown in
  `chezzi run` and in the test report.
- For a **comparison** (`<`, `<=`, `>`, `>=`, `==`, `!=`) the message shows both operand values:
  `assertion failed: 3 == 4`, or `assertion failed: <msg> (3 == 4)` with a `msg`. `in` is excluded
  (its right operand can be a large collection).
- Any other condition gives `assertion failed: <msg>`, or `assertion failed` without a `msg`.
- `assert` works anywhere, not only in tests.

**`test fn`** marks a test. A free `test fn` is an independent test (no parameters, returns nothing);
a `test fn name(self)` method makes its struct a **suite**:

```chezzi
# math_test.chz — independent tests
test fn parses_int():
    assert to_int("42") == 42

# a suite — a struct with test methods, optional lifecycle hooks, and a shared fixture
struct DbTests:
    db: Db = connect()              # built ONCE for the suite (a default field expr)
    fn before_each(self): self.db.begin()
    fn after_each(self): self.db.rollback()
    fn after_all(self): self.db.close()

    test fn empty(self):
        assert self.db.count() == 0
    test fn insert(self):
        self.db.insert("x")
        assert self.db.count() == 1
```

The lifecycle hooks `before_all`, `after_all`, `before_each`, `after_each` are found by name and are
optional. Each must be `fn name(self)` returning nothing. The shared fixture is a default-initialized
field, mutated through `self`.

**`chezzi test [path]`** runs every `test fn` in `*_test.chz` files.

- `path` defaults to the current directory. A `*_test.chz` file runs that file; a directory is walked
  recursively.
- Each test runs in isolation (one failure does not stop the rest). The report is
  `PASS/FAIL name (file:line:col) msg` plus a summary; the exit code is non-zero if a test failed.
- A suite runs `before_all? → [before_each? → test → after_each? (always, even on failure)]* →
  after_all?` and builds the suite instance once.
- Tests run on the M:N scheduler like `chezzi run`; `CHEZZI_THREADS` sets the worker count.

The repo's own Chezzi test suite is in `tests/chz/` (`spec/` for language behaviour, `stdlib/` for
modules, `suites/` for lifecycle suites), separate from `examples/` (golden-output demos). `cargo test`
runs it through `tests/chz_suite.rs`, and `tests/chezzi_threads_cli.rs` runs it again at
`CHEZZI_THREADS=2`. You can test a fault's message in Chezzi: `r := recover: <expr>` then
`match r: !e: assert e.message().contains(...)`. Only checker diagnostics and engine internals stay in Rust.

## 9b. Program entry — there is no automatic `main`

A program runs **top-to-bottom**. `main` is an ordinary function. Run a file directly and only its
top level runs, so call `main` yourself:

```chezzi
fn main():
    print("hello")

main()        # running this FILE directly needs the call; nothing runs main for you
```

For a **project**, `chezzi.toml`'s `[project] entrypoint` sets what a bare `chezzi run` executes: a
dotted module path, with an optional **`:function`** suffix:

```toml
[project]
entrypoint = "src.main:main"   # run src/main.chz's top-level, then call its `main`
# entrypoint = "src.main"      # (no :function) run src/main.chz's top-level only — call main yourself
```

- With `:function`, a bare `chezzi run` runs the module's top level and then calls that function, so
  the source needs no trailing call. Change the manifest to run another function.
- The function part is one name; a second `:` or a `.` is a manifest error naming `chezzi.toml`.
- A function name the entry module never binds (no top-level `fn`, `let` or import) is rejected by
  `chezzi check` and bare `chezzi run` before any code runs, with an error naming `chezzi.toml`. A name
  bound to a non-function is a runtime error.
- Without the suffix, the top level runs and nothing is called.
- `chezzi run src/main.chz` (an explicit file) always runs the top level only.

**`chezzi init [dir]`** creates a new project (`dir` defaults to the current directory and is created
if missing): a `chezzi.toml`, `src/main.chz` (a `fn main():` with no trailing call, since the
entrypoint is `"src.main:main"`), and an example `src/main_test.chz` with `test fn`s.
It never overwrites a file that is already there.

- An existing `chezzi.toml` stops it: nothing is written, exit 1.
- An existing `src/main.chz` or `src/main_test.chz` is kept and reported as `kept`; only missing files
  are written (exit 0), as `cargo init` and `go mod init` do.
- The manifest is a root marker and a parsed file: the toolchain reads its `[project]` keys (`name`,
  `version`, `entrypoint`). The reader takes `[section]` headers, `key = "value"` string pairs and `#`
  comments. An empty `chezzi.toml` is a valid root marker with no entrypoint.

> **Program arguments follow a `--` terminator** (as `cargo run -- --dir x`; `go run` forwards the
> `--` itself):
>
> ```
> $ chezzi run -- --dir logs   # entrypoint sees std.os.args() == ['--dir', 'logs']
> ```
>
> The `--` is consumed. The file form takes it too (`chezzi run src/main.chz -- --dir logs`). Only the
> first `--` in terminator position (after `run`/its flags or the path) is consumed; a later `--`
> reaches `std.os.args()` (`chezzi run f.chz a -- b` → `['a', '--', 'b']`, as `go run`, `python` and
> `cargo run -- a -- b` do).
