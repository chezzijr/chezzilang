### `std.fs`

Every syscall here is [uninterruptible while in flight](04-native-core.md#blocking-calls-cannot-be-interrupted).

**Every path argument is a [`PathLike`](04-native-core.md#pathpath-input) and every path RESULT is a
[`path.Path`](07-pure-text-data.md#pathpath).** A bare `str` literal works with no annotation and no
turbofish; `bytes`, `bytearray` and `path.Path` work too. The returned `Path` carries the **raw OS
bytes**, so a filename that is not valid UTF-8 round-trips (`fs.exists(fs.list_dir(d)?[0])` is
`true`).

> **Internal byte seam.** Each path-taking native is declared once, `_`-prefixed and typed `bytes`
> (`_exists`, `_list_dir`, `_read_file`, `_getcwd`, …); the public name is a pure-Chezzi wrapper that
> calls `_native(p.as_path())` and wraps a returned path into `path.Path`. A `_` name that a std module
> declares is **internal to std**: the checker hides it from user modules, so
> `import _exists from std.fs` is `module 'std.fs' has no member '_exists'`. The rule covers every std
> module's `_` members, `_` types and `_` methods; a user module's own `_` names are untouched. Call the
> public name.

**Queries:**

- `list_dir(p) -> List[Path]!`: entry names, sorted by raw bytes.
- `exists(p) -> bool` · `is_file(p) -> bool` · `is_dir(p) -> bool` · `size(p) -> int!`.
- `glob(pattern) -> List[Path]!`: the Go `filepath.Match` dialect.
  - `*` (any run), `?` (single char), and POSIX character classes `[abc]`/`[a-z]`/`[^abc]`, all in the
    **final** path component only. No recursive `**`, no brace expansion, no escape character.
  - `[` is a **metacharacter**: it opens a class.
  - A malformed `[...]` class is an `!e` carrying `"bad pattern"`, checked **before** the directory is
    read, so the result never depends on whether the directory exists.
  - A directory that does not exist, cannot be read, or is not a directory (after the pattern passed
    its check) yields `?([])`, not `!e`. Go 1.26.6 `filepath.Glob` returns `[] err=<None>` for all
    three ("Glob ignores file system errors such as I/O errors reading directories"), and CPython 3.14.7
    `glob.glob` returns `[]`. `fs.walk` is the opposite: it aborts at the first unreadable directory.
  - Matched over **raw bytes**, so an ASCII pattern still matches a non-UTF-8 filename. `?` and a
    class's single character count one **Unicode scalar** where the name is valid UTF-8 (like Go
    `filepath.Match` / Python `fnmatch`), and one byte only for a byte that begins no valid sequence.
  - `*` **matches dotfiles** (Go `filepath.Glob`): `gt/*` over a dir holding `.hidden` + `visible.txt`
    returns both. Python's `glob.glob` excludes dotfiles from a bare `*` unless the pattern starts with
    a dot.
- `canonicalize(p) -> Path!`: resolve symlinks + `.`/`..` against the **real filesystem** to an absolute
  real path. Unlike the lexical `path.normalize` (no I/O), it **requires the path to exist** (`!e` on a
  nonexistent path).
- `stat(path) -> FileInfo!`: read filesystem metadata into a
  `struct FileInfo { size: int, mtime: int, mode: int, is_dir: bool, is_file: bool, is_symlink: bool }`.
  `size` is bytes; `mtime` is Unix-epoch **seconds** (`0` if pre-epoch/unsupported); `mode` is the raw
  unix `st_mode` (permission + type bits; `0` on non-unix). `stat` **follows symlinks** for
  size/mtime/mode/is_dir/is_file (as `stat`/Python `os.stat`); `is_symlink` is separate (so a
  symlink-to-file has `is_file == true` **and** `is_symlink == true`). `!e` on a missing/unreadable path
  (a broken symlink included). `std.fs` **owns** `FileInfo`: read its fields off a returned value with no
  import, but to name the type `import std.fs` (or `import FileInfo from std.fs`).
- `walk(p) -> List[Path]!`: recursively list **every** entry (files + dirs) strictly under `p` as full
  paths, in a **deterministic** order: each directory's entries sorted by name, a directory before its
  children (pre-order). A **symlinked directory is listed but not descended** (cycle guard). `!e` on
  the first unreadable directory, the root or any descendant; the message names that directory, and
  the walk stops there with no partial listing.

**Mutations** (all `None!`: a permission-denied / missing-parent failure is a catchable `!e`, never a
panic):

- `mkdir(path) -> None!`: create a directory **recursively** (like `mkdir -p`: missing parents are
  created, an existing dir is a no-op). An **empty path is an `!e`** (`No such file or directory`).
- `remove_file(path) -> None!`: delete a file (`!e` if missing or a directory).
- `remove_dir(path) -> None!`: delete an **empty** directory; **non-recursive** (`!e` on a non-empty
  dir).
- `rename(from, to) -> None!`: move/rename a path.
- `copy(from, to) -> None!`: copy a file's contents (file-only; the byte count is dropped). It
  **`!e`s, leaving the file untouched, when `from` and `to` are the SAME FILE**: the same path, or two
  names that reach one inode via a symlink or a hardlink (identity is `dev`+`ino`). Without that check
  the truncating open would wipe the file. Python `shutil.copyfile` raises `SameFileError` and
  coreutils `cp a a` errors the same way.
- `append(path, contents) -> None!`: append a string to a file, creating it if absent and **never
  truncating** (complements `std.io.write_file`, which overwrites).
- `chmod(path, mode: int) -> None!`: set unix permission bits (e.g. `0o755`). **Unix-only** (on a
  non-unix target it `!e`s `"chmod is unix-only"`); `mode` goes to the OS unmasked.
- `atomic_write(path, contents) -> None!`: write `contents` to a temp file in the **same directory** as
  `path`, then `rename` it over `path` (atomic within one filesystem). A concurrent reader sees either
  the old contents or the new, never a half-written file, and an existing target's permission bits are
  kept across the swap. It is **not** `fsync`'d: concurrent-observer atomicity, **not**
  crash/power-loss durability (same as `write_file`).

**Limit:** there is no recursive directory removal (`remove_dir_all` / `rm -rf`): `remove_dir` is
empty-only to prevent an accidental recursive wipe. Use `fs.walk` (reverse the list) +
`remove_file`/`remove_dir` if you need it.

### `std.time`
`now() -> int` (Unix epoch seconds, UTC) · `now_ms() -> int` (Unix epoch **milliseconds**, UTC; a wall
clock that can step backwards and is never clamped, so never subtract a `monotonic()` reading from it)
· `monotonic() -> float` (seconds, immune to clock changes) · `sleep_ms(ms: int) -> None` ·
`format(epoch: int) -> str` (`"YYYY-MM-DD HH:MM:SS"`, UTC).
`timer(ms) -> Channel[bool]` (one-shot timeout channel; see `concurrency.md §6c`): `time.timer(ms)`, or
`import timer from std.time` (an alias works, and `f := time.timer` is a function value).

Both `sleep_ms` and a `timer(ms)` `recv` are **continuous cancellation checkpoints**: the runtime owns
the deadline, so a scope cancel or an `Executor.shutdown_now()` ends the wait within ~5 ms, and the task
still runs its `defer`s. `chezzi test --timeout` uses the same checkpoint and reaches every timer wait,
also one parked in a `parallel:` nursery with no runnable sibling (`concurrency.md` §cancellation
points). **The same ~5 ms bound covers a nested `Executor` JOIN**: a job parked in an inner
`shutdown()`/`shutdown_now()` sees an outer cancel and `--timeout` on the join's own poll (measured:
cancel 6 011 ms → **213 ms**, `--timeout=500` 9 010 ms → **509 ms** over a job that runs `process.run`).
The bound cannot cross the [uninterruptible in-flight call](04-native-core.md#blocking-calls-cannot-be-interrupted)
itself: the join unwinds within ~5 ms, and the child process it waited on runs to completion.

### `std.process`
A running child is [uninterruptible while in flight](04-native-core.md#blocking-calls-cannot-be-interrupted):
nothing in the language kills it. Bound it with `timeout N …` in the command line itself.

| Function | Result |
|---|---|
| `cmd(line: str) -> str!` | Run `sh -c <line>` and capture stdout; `!stderr` on non-zero exit (on failure stdout is discarded; use `run` for the full result). |
| `run(line: str) -> ProcResult!` | Run `sh -c <line>` and return `struct ProcResult { stdout: str, stderr: str, code: int }`. A non-zero exit is a normal `?ProcResult` with `code != 0` (both streams kept); **only a spawn failure** (no such program, permission denied) is `!e`. A signal-killed process reports `code = -1`. |
| `run_args(prog: str, args: List[str]) -> ProcResult!` | Run `prog` directly with `args` as the argv vector, **NO shell**, so metacharacters in `args` (`$(...)`, `;`, `&&`, …) pass literally and are **injection-safe**. Same `?v`/`!e` contract as `run`. |
| `run_bytes(line: str) -> bytes!` / `run_args_bytes(prog: str, args: List[str]) -> bytes!` | Return the child's stdout **byte-exactly**. |

**Text vs binary: the `str` forms are a LOSSY VIEW, the bytes twins are exact.** `ProcResult`'s fields
(and `cmd`'s return) are `str`, so `cmd`/`run`/`run_args` decode the child's output as UTF-8 *lossily*
(an undecodable byte becomes `U+FFFD`). Use `run_bytes`/`run_args_bytes` for binary output. Their
`?v`/`!e` split is **`cmd`'s, not `run`'s**: `bytes!` has no status channel, so **any failed child is
`!e`**: a non-zero exit (message = the child's stderr, or `command exited with status N` if it wrote
none) as well as a spawn failure. `?bytes` therefore means "the command succeeded and these are its
bytes" (the rule `request.get_bytes` uses for a non-2xx). A command that exits non-zero **and** has
meaningful stdout (`grep`, `diff`) belongs on `run`/`run_args`, which carry `code` + both streams (or
use `run_bytes("cmd; exit 0")`). The text call does not fail on bad UTF-8, because a finished child
has no carry: an error would destroy the captured stdout, stderr and exit code. The bytes path carries
**stdout only**; neither form gives byte-exact stderr.

All five are blocking subprocess I/O (offloaded). `std.process` **owns** `ProcResult`: read its fields
(`.stdout`/`.stderr`/`.code`) off a returned value with no import, but to name or construct the type
(`p: ProcResult` / `ProcResult(...)`) import the module (`import std.process`, then `ProcResult(...)`
or `process.ProcResult(...)`; or `import ProcResult from std.process`). A user `struct ProcResult`
(without the import) is your own type.

**Security:** `cmd`/`run` hand `line` to the shell. Never interpolate untrusted input into it
(shell injection); use `run_args`.

**Not yet:** stdin piping, output streaming, per-process env/cwd overrides, a bytes-carrying structured
result (binary stdout *plus* stderr *plus* the code in one value).

### `std.rand`
Pseudo-random scalars (SplitMix64 PRNG). `seed(n: int) -> None` (reseed deterministically) ·
`float() -> float` (uniform in `[0, 1)`) · `int(lo: int, hi: int) -> int` (uniform in the half-open
`[lo, hi)`; **faults** `rand.int(lo, hi): hi must be > lo` if `hi <= lo`) · `bool() -> bool`.
The stream seeds itself on first use; call `seed(n)` to make it reproducible. Draws are inline CPU
(not I/O). Generic collection helpers (`shuffle`/`choice`/`sample`) live in `std.iter`: the native
seam carries only scalars, so it cannot return a generic `List[T]`.

**Platform (same source as `crypto`):** the auto-seed reads OS entropy on **every unix**: Linux
`getrandom(2)`, `/dev/urandom` elsewhere (macOS, the BSDs, Android, iOS). Only where that read fails
(a non-unix target, or `/dev/urandom` unreadable) does it fall back to a mix of wall-clock nanos, a
static's address and a process counter, which is guessable. The **auto-seed** never faults (like
CPython's `random`). The draws can: `rand.int(lo, hi)` faults on `hi <= lo`.

**Security:** `std.rand` is **not cryptographically secure** on any platform. The generator is a 64-bit
SplitMix64 whose output function is a bijection, so one observed raw draw recovers the state *and* the
seed and predicts the whole stream in both directions. This carries to `iter.shuffle` / `iter.choice` /
`iter.sample`, which call `rand.int`, and to `std.uuid`'s **seeded** stream (unseeded `uuid.v4()` reads
the OS CSPRNG per call). For a secret (a token, a session id, a shuffle an adversary must not predict)
use `crypto.token_hex(n)` / `crypto.secure_bytes(n)`.

**Limit:** the PRNG state is one process-global, so *concurrent* draws from several tasks interleave
nondeterministically. *Sequential* draws are deterministic once seeded. Draw in one task, or guard
with a `Shared`/lock, when you need reproducibility under concurrency.

### `std.regex`
Results use `struct Match { text: str, start: int, end: int, groups: List[str] }`: **codepoint**
offsets, like Python's `re`, so `subject[m.start..m.end] == m.text` holds on non-ASCII input; `groups`
are capture groups 1..n; a non-participating optional group is `""`.

`is_match(pattern, subject) -> bool!` · `find(pattern, subject) -> Match?!` ·
`find_all(pattern, subject) -> List[Match]!` · `find_all_text(pattern, subject) -> List[str]!` ·
`replace_all(pattern, subject, repl) -> str!` · `split(pattern, subject) -> List[str]!`. A bad pattern is
`!e`. Each call takes the pattern as a string; an internal cache holds the compiled form. There is no
compiled `Regex` handle value yet.

`find_all_text` returns the matched text of every `find_all` match (Go's `FindAllString`, Rust's
`find_iter`), about 5x faster than `find_all` plus a `.text` loop over 1M tokens (0.31s vs 1.64s,
release). Unlike Python's `re.findall` it ignores capture groups (`find_all_text(r"(\d+)-(\d+)",
"1-2")` is `?(['1-2'])`, where `re.findall` gives `[('1', '2')]`).

**Write patterns as RAW strings: `r"\d{4}"`, not `"\\d{4}"`.** A normal string always interpolates, but
a hole whose whole text is digits (like `{4}`) renders literally, so `"\\d{4}-\\d{2}"` keeps its
quantifiers: `\d{4}-\d{2}`. A raw string is still the recommended spelling, because it also keeps every
backslash literal.

**The dialect is RE2 (the Rust `regex` crate), not Python's `re`.** The differences, most common first:

- **Replacement is `$1` / `${name}`, Rust-style. Python's `\1` is REJECTED.**
  `replace_all(r"(\d+)-(\d+)", "10-20", r"$2/$1")` → `?'20/10'`; the same call with `r"\2/\1"` → an
  `!e` naming the RE2 form (`'$2'`). A backslash not followed by a digit or `g<` stays literal, so
  `r"\n"` as a replacement produces backslash-n. That is the only way to put a literal backslash in a
  replacement.
- **Argument order is `(pattern, subject, repl)`**: the *replacement is last*, the reverse of
  `re.sub(pattern, repl, subject)`. Both are `str`, so swapping them type-checks.
- **No lookaround and no backreferences** (RE2 drops them for its linear-time guarantee):
  `r"(?<=a)b"` and `r"(a)\1"` each come back `!"regex parse error: … not supported"`. Rewrite with an
  explicit group + `Match.groups`. **The payoff is no ReDoS**: `"a" * 30 + "b"` against `r"(a+)+$"`
  takes **0.000077 s** here and **86.42 s** in CPython `re`. (Test ReDoS with a non-matching tail:
  against `"a" * 30` alone both are instant.)
- **`split` drops capture groups, Python keeps them**: `split(r"(,)", "a,b")` → `?(['a', 'b'])` where
  `re.split` gives `['a', ',', 'b']`. You may *write* named groups `(?<name>…)`, but there is no
  read-by-name accessor: index `Match.groups` positionally.
- **An empty match next to the previous match is dropped** (RE2/Go/Rust rule): `find_all(r"a*", "baaa")`
  → `['', 'aaa']` where Python gives `['', 'aaa', '']`; `replace_all(r"a*", "baaab", "-")` → `-b-b-` vs
  Python's `-b--b-`; and `a$` does not match before a trailing `\n` (Python's `$` does).
- **`$` takes the LONGEST `[0-9A-Za-z_]` run, so `$1px` names a group called `1px`**, not group 1
  followed by `px`. That group does not exist, so the call errs naming `${1}`. `${1}px` gives
  `?'12px 34px'`, and `$$` is a literal `$`. Go's `regexp.ReplaceAllString` expands an unknown name to
  the empty string; Chezzi errs, as for the backslash form above. An empty `${}` is malformed and stays
  literal text, as in Go (`replace_all(r"(\d+)", "12 34", r"${}")` → `?'${} ${}'`).
