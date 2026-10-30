## 4. Native modules

Each is `import std.<name>` then `name.func(...)`. They are implemented in Rust (`src/native/*.rs`).

### Blocking calls cannot be interrupted

The `std.io` file seams, every `std.fs` syscall, every `std.process` runner (`cmd`/`run`/`run_args`/
`run_bytes`/`run_args_bytes`) and every `std.request` call are **offloaded** to a dirty-pool thread so
they never pin an OS-thread worker. The price: a thread inside one has **no cancellation checkpoint
until the call returns**. A sibling's fault (scope cancel), a `cancel.Token`, `os.exit` and
`chezzi test --timeout` therefore do **not** abort a call already in flight; the halt takes effect
when the call returns. Size the operation itself (a subprocess `timeout 5 …`, a request timeout) if you
need a bound. A fault of a child of a nursery that the caller owns arrives the same way: at the return,
and the call's result is dropped. The `std.io` stdin readers obey the same rule (Go's `os.Stdin.Read`
and CPython's `input()` are not cancelled either).

**Planned:** `std.process` calls will become killable: a cancelled task will signal the child's
process group instead of waiting it out (design: [`killable-subprocess.md`](../../design/killable-subprocess.md)).
`std.fs`/`std.io` will stay uninterruptible, as in Go, which does not cancel file I/O either.

`chezzi test --timeout` therefore reports such a test `TIMED-OUT` **after** it finishes; its message
says `… could not be aborted (a blocking … call has no checkpoint until it returns)`, which tells it
apart from a real abort.

### `std.math`
Functions: `abs`, `floor`, `ceil`, `round`, `pow(base, exp)`, `sqrt`, `sin`, `cos`, `tan`,
`asin`, `acos`, `atan`, `atan2(y, x)`, `exp`, `ln`, `log2`, `log10`, `log(value, base)`.

- `abs` is `abs[T: Num](x: T) -> T` (`int`→`int`, `float`→`float`; `Num` is the builtin protocol
  satisfied by `int` and `float` only); the rest take/return `float`. `abs` and `sign` are values like
  any generic fn: `[-1, 2].map(math.abs)` is `[1, 2]`, `math.abs[int]` is a `fn(int) -> int`, and an
  unpinned `g := math.abs` is rejected with the instantiate hint (`math.abs[<T>]`), as Go rejects
  `g := Abs`.
- A `float` parameter never accepts an `int` (rule D3, `syntax.md §3`): `sqrt(16)` / `floor(2)` are type
  errors naming `write 1.0 (or float(x))`. Write `sqrt(16.0)` / `floor(2.0)`, or `sqrt(float(i))` for a
  typed `int` value.
- `math.round` rounds **half away from zero** (`round(2.5)` → `3.0`, `round(-2.5)` → `-3.0`). The `:.0f`
  format spec uses **banker's rounding** (`"{2.5:.0f}"` → `2`, matching Python). Use `math.round` for
  arithmetic and the format spec for display.
- Math is **total IEEE-754**: out-of-domain inputs return `NaN`/`inf` instead of faulting.
  `sqrt(-1.0)`, `ln(-1.0)`, `asin(2.0)` are all `NaN`; `ln(0.0)` is `-inf`. (`abs` on `int` `MIN`
  still overflow-faults: that is integer arithmetic.)
- Predicates (`float -> bool`, IEEE-754 classification): `is_nan(x)`, `is_inf(x)` (±infinity),
  `is_finite(x)` (neither `NaN` nor infinite).
- There is no `math.min`/`math.max`: use `std.cmp`.

Number / integer functions (Python `math` semantics):
- `gcd(a, b) -> int`, `lcm(a, b) -> int`: greatest common divisor / least common multiple.
  `gcd(0, 0)` is `0`; negatives use absolute value (`gcd(-12, 8)` → `4`). `lcm` involving `0` is `0`.
  `lcm` is computed as `|a|/gcd * |b|`; a result that overflows i64 faults (integer, like `abs`).
- `divmod(a, b) -> (int, int)`: the pair `(a / b, a % b)` using Chezzi's own C-style `/` and `%`:
  int `/` **truncates toward zero** and `%` carries the **dividend's** sign, NOT Python's floor
  `divmod`. `divmod(17, 5)` → `(3, 2)`; `divmod(-7, 2)` → `(-3, -1)` (Python gives `(-4, 1)`). `b == 0`
  faults like `a / b`. (A bodied Chezzi fn beside the native decls: the hybrid module form.)
- `sign[T: Num](x: T) -> T`: like `abs` (`int`→`int`, `float`→`float`); returns `-1`/`0`/`1`
  (numpy/Go convention). `sign(0.0)` is `0.0`, `sign(NaN)` is `NaN`.
- `trunc(x: float) -> int`: truncate toward zero. Equivalent to the `int(x)` builtin; faults on a
  non-finite or out-of-i64-range input (same as `int()`).
- `hypot(x, y) -> float`: `sqrt(x*x + y*y)`. `cbrt(x) -> float`: real cube root (total; `cbrt(-8.0)` → `-2.0`).
- `factorial(n) -> int!`, `comb(n, k) -> int!`, `perm(n, k) -> int!`: return a clean `!e` (never a
  fault) on a bad domain (negative `n`/`k`) or i64 overflow. `factorial` tops out at `20!` (`21!`
  exceeds i64, so it errs). `comb`/`perm` yield `0` when `k > n` (Python), compute in i128 internally,
  and err only when the true result exceeds i64.
- `parse_int_base(s: str, base: int) -> int!`: parse `s` in `base` (`0` or `2..=36`); malformed input
  errs (never faults).
  - `base 0` auto-detects a `0x`/`0o`/`0b` prefix (else decimal; then a leading zero is an error unless
    the value is zero, as CPython `int(s, 0)`: `"017"` errs, `"00"` → `0`). Bases `2`/`8`/`16` also
    accept the matching prefix.
  - A leading `+`/`-` sign is allowed (`parse_int_base("-2a", 16)` → `-42`).
  - Trims surrounding whitespace and accepts PEP-515 single underscores between digits at every base,
    as `str.to_int` does (`parse_int_base("1_0", 10)` → `10`; `parse_int_base("ff_ff", 16)` → `65535`),
    plus ONE underscore right after a base prefix (`parse_int_base("0x_ff", 0)` → `255`,
    `("-0x_ff", 0)` → `-255`; `0x__ff`, `0x_` and a base-16 `_ff` are an `!e`, like CPython `int()`).
  - A well-formed numeral outside i64 fails with `overflows i64 (range
    -9223372036854775808..=9223372036854775807)`.

Constants (all `const`: reassigning `math.pi`, or `import pi from std.math; pi = x`, is a type error
naming them const): `math.pi`, `math.e`, `math.inf` (positive infinity), `math.nan` (NaN;
`math.nan != math.nan`). They are `NAME: const float = value` lets in `std/math.chz`.

### `std.io`
The file seams (`read_file`/`write_file`/`read_bytes`/`write_bytes`) are
[uninterruptible while in flight](#blocking-calls-cannot-be-interrupted).

| Function | Signature | Notes |
|----------|-----------|-------|
| `print` | `(s: str) -> None` | stdout + newline. |
| `eprint` | `(s: str) -> None` | stderr + newline. |
| `read_line` | `() -> str?` | Blocking stdin line, newline stripped (`None` at EOF). |
| `read_all` | `() -> str` | Drain **all** remaining stdin to EOF as one `str` (Python `sys.stdin.read()`); `""` at a clean EOF. Shares the one stdin source with `read_line` (a later read then sees EOF). Non-UTF-8 stdin is a **fault**; there is no stdin `read_bytes`. |
| `read_char` | `() -> str?` | Read one Unicode scalar as a 1-char `str` (Chezzi has no `char` scalar); `None` at a clean EOF, a **fault** on a partial/invalid UTF-8 sequence. |
| `flush` | `() -> None` | Flush this process's stdout. In effect a **no-op**: the CLI's stdout is unbuffered (every write, partial line included, is flushed as it is produced) and captured output has nothing to flush. It is the portable idiom, and it never waits on stdout's consumer, so it cannot stall a task. (For real buffering, wrap `stdout()` in `buffered(...)` and call the *Writer*'s `flush()`.) |
| `input` | `(prompt: str) -> str?` | Print the prompt (no newline), flush, read one line. Exactly `print(prompt, end="") + flush + read_line` (`None` at EOF). |
| `isatty` | `() -> bool` | `true` when **stdout** is a real terminal, `false` when piped/redirected (via `std::io::IsTerminal`). Python `sys.stdout.isatty()` / Go `isatty`. Lets a CLI colorize only when not piped. |
| `isatty_stdin` | `() -> bool` | Same, over **stdin**. |
| `isatty_stderr` | `() -> bool` | Same, over **stderr**. |
| `read_file` | `(p: PathLike) -> str!` | Whole file as text (≤ 64 MB; for larger files stream with `open(...)` → `Reader`). **Decodes UTF-8**: a binary file is an `!e` pointing at `read_bytes`. |
| `write_file` | `(p: PathLike, contents: str) -> None!` | Write / overwrite. |
| `read_bytes` | `(p: PathLike) -> bytes!` | Whole file as raw bytes (≤ 64 MB): binary files. |
| `write_bytes` | `(p: PathLike, data: bytes) -> None!` | Write / overwrite raw bytes; no size cap, like `write_file`. |
| `create` | `(p: PathLike) -> Writer!` | Open a **truncating** write handle (create-or-truncate). |
| `append` | `(p: PathLike) -> Writer!` | Open an **append** write handle (create-if-absent, never truncates). |
| `stdout` | `() -> Writer` | A fresh write handle over the process stdout sink (same sink as `print`). |
| `stderr` | `() -> Writer` | A fresh write handle over the process stderr sink (same sink as `eprint`). |
| `buffered` | `(w: Writer, size: int = 8192) -> Writer` | Wrap a writer so writes accumulate in-VM and reach the host in **one** call per `flush` / buffer-full / `close` (Go `bufio.NewWriter`; 8 KiB default). |
| `open` | `(p: PathLike) -> Reader!` | Open a **read-only** file handle for line/chunk streaming (past the 64 MB whole-file `read_file` cap). A directory is an `!e` **at the call** (Python `IsADirectoryError`), same message as `read_file`. |

**`Writer`: write-only file / stream handle.** A native handle (like `Socket`) for buffered
output. Two openers, not a mode string (`create` = truncate, `append` = append). Text vs binary is
**per call**. `stdout()`/`stderr()` route through the *same* sink as `print`/`eprint` (never a raw fd),
so a `Writer` over stdout captures and streams identically. `buffered(...)` batches host/fd writes.

| Method | Signature | Notes |
|--------|-----------|-------|
| `write` | `(data: str) -> int!` | UTF-8-encode + write; returns bytes written. |
| `write_bytes` | `(data: bytes) -> int!` | Write raw bytes; returns bytes written. Byte-exact on **every** backing (a file, `stdout()`/`stderr()`, or a `buffered` chain over either), so `stdout().write_bytes(b"\xff\xfe")` puts `ff fe` on fd 1, matching Python's `sys.stdout.buffer.write` and Go's `os.Stdout.Write`. |
| `flush` | `() -> None!` | Drain a `buffered` writer's in-VM buffer **and** flush every core beneath it (one host/fd write per level). On a **file**-backed chain (`buffered(create(p))`, nested `buffered` included) an `?v` means the bytes are on the fd: visible to an in-process `io.read_file`, a `process.run` child, a sibling process. Like `fs.atomic_write` this is **not** `fsync`'d: observer visibility, **not** crash/power-loss durability. A no-op on unbuffered `stdout`/`stderr`; on a `buffered(stdout())` writer the drained bytes go to the same background stdout queue as `print`, so `?v` there means *queued*, not *written*. |
| `close` | `() -> None!` | Flush (same full-chain guarantee as `flush`) + close the handle. Use-after-close is a clean `!e`, never a fault. |

- **An explicit `flush()`/`close()` on a file-backed buffered writer always persists** (Python
  `open(p,'wb',buffering=n)` / Go `bufio` semantics), also after a write larger than the buffer,
  which drains mid-write.
- **Flushing/writing *through* a handle whose inner writer is closed is a clean `!e`** naming the inner
  (`the inner writer this buffer drains into is closed`). A flush that persisted nothing never reports
  `?v`, and `close()` does not mask it either. (`w0 := io.create(p)?; w := io.buffered(w0, 8); w0.close()`
  ⇒ `w.flush()` is that `!e`.)
- **If you forget `flush`/`close` on a `buffered` writer, you can lose the tail** (as in Go). A
  **file**-backed buffered writer flushes its tail when the handle is dropped (program exit / GC), a
  nested `buffered(buffered(create(p)))` chain included. A **stdout/stderr**-backed buffered writer's
  tail is *not* recovered on drop: call `flush()`/`close()`. A plain `create`/`append` writer never
  loses data (its `BufWriter` flushes on drop). A writer that becomes garbage flushes when a
  collection frees it, so its tail reaches the fd at a collection, not when the last reference
  goes (call `flush()`/`close()` when a reader waits for it). One reaper thread runs these drops,
  in the order the collections freed the writers, and it runs no task: while an fd is a full pipe,
  no task waits for the flush, but a second freed writer's flush waits behind the first (Go runs
  its finalizers on one goroutine the same way). A run waits for its pending flushes before it
  returns. Python's `BufferedWriter` and Rust's `BufWriter` flush on drop too; Go's `bufio.Writer`
  loses the tail.
- **Cross-task write order to one shared `Writer` is unspecified** (Go's `bufio`-not-goroutine-safe
  rule). Each single `write`/`write_bytes` is one atomic critical section, but the *order* of separate
  writes from different tasks is not guaranteed. Join and write from one task if you need order.
- Not supported: seek / random access.

**`Reader`: read-only file handle.** The read twin of `Writer` (same kind of native handle, opened by
`open(path)`): stream a large file line by line or chunk by chunk (the 64 MB `read_file`/`read_bytes`
cap does not apply).

| Method | Signature | Notes |
|--------|-----------|-------|
| `read_line` | `() -> str?` | **Three** outcomes: one line (ONE trailing `\n`, then ONE `\r`, **stripped**, as Go `bufio.Scanner` and the module-level `read_line()`), `None` at EOF, or a clean **fault** on a mid-read I/O error / non-UTF-8 line, pointing at `read_bytes` (a `T?` cannot carry the error, like `read_file`). The non-UTF-8 fault is **non-destructive**: see the carry rule below. |
| `read_bytes` | `(n: int) -> bytes!` | At most `n` bytes (exactly `n` until a short final chunk); **empty bytes = EOF**; `!e` on closed / I/O. The binary and error-reporting form. `n <= 0` → `?(b"")`. Drains a pending **carry** first, without touching the fd. |
| `close` | `() -> None!` | Release the fd, and discard any carry. Idempotent; a read after `close` is a clean `!e` (`read_bytes`) / fault (`read_line`), never a panic. |
| `lines` | `() -> Iterator[str]` | **Lazy** line stream: `for ln in r.lines():` (Python `for l in f` / Go `bufio.Scanner` / Rust `BufRead::lines`). A generator over `read_line()`: each line is fetched on demand (the file is **not** snapshotted; an early `break` stops reading), one trailing `\n`, then one `\r`, stripped, ends at EOF. A mid-read non-UTF-8 fault surfaces as in `read_line`, carry included. |

**The non-UTF-8 fault is NON-DESTRUCTIVE.** The reader **carries** the line that `read_line` could not
decode: it keeps the raw bytes, line terminator included, and `read_bytes` returns them **byte-exactly**
as a carry-only *short* read. The fd is not touched until the carry is empty, so `read_bytes(100)`
after the fault yields exactly the failed line, and the *next* `read_bytes` continues the file. Same
rule as `Socket.read`'s carry.
- **Sticky.** While a carry is pending, `read_line` re-decodes it and re-faults; it never skips ahead.
  So `for ln in r.lines():` cannot step over a bad line: drain it with `read_bytes` (or `close`) to make
  progress.
- **Self-healing.** A *partial* `read_bytes` that drains the invalid prefix leaves the rest carried;
  if that remainder decodes, the next `read_line` returns it as the line.
- `close()` discards the carry, and a carry is never served after `close` or after EOF.
- A **mid-line I/O error** carries too: whatever the read delivered before the error stays, so
  `read_bytes` gets it back. That carry is **not** self-healing: an interrupted line is a *truncated*
  one, so `read_line` re-raises the I/O error until you drain the bytes.

Why sticky: on `b"alpha\nbad\xff\xfebytes\ngamma\n"`, CPython 3.14 `open(p,'r')` raises on the
**first** `readline()` (`"alpha"` is lost too) and then returns `''`, a false EOF over `"gamma"`. Go's
`bufio.Scanner` returns the bad line verbatim, which a Chezzi `str` cannot hold. Chezzi returns
`?"alpha"`, then faults, with every byte still recoverable through `read_bytes`.

```chezzi
# /tmp/bin.dat == b"line1\nA\xffB\nline3\n"
r := io.open("/tmp/bin.dat")?
r.read_line()                     # line1
x := recover: r.read_line()       # !stream did not contain valid UTF-8 — read binary files with Reader.read_bytes
r.read_bytes(100)                 # b'A\xffB\n'   <- the refused line, byte-exact
r.read_bytes(100)                 # b'line3\n'
```

- **Cross-task read order on one shared `Reader` is unspecified**: two tasks that read one handle race
  for the file offset (Go's `bufio`-not-goroutine-safe rule). Each single read is one atomic critical
  section; read from one task if you need order.
- Not supported: seek / random access.

**Output contract (`chezzi run`).** The CLI **streams**: output appears when it happens (a prompt before
its read; a long-running program prints incrementally; a killed program keeps what it printed; a spawned
task's line is visible before its nursery joins).

- One `print(...)` call is **ONE locked write**, so it is line-atomic: two tasks never garble a single
  `print`'s output. But `print(x, end="")` fragments from two tasks **can** interleave mid-line (as in
  Python).
- Concurrent tasks' prints interleave **nondeterministically**; cross-task order is NOT guaranteed. For
  ordered output from concurrency, **join and print the results yourself** (as in Python/Go/Rust). (The
  per-task buffer + task-order flush belongs to the captured *test-harness* sink, not to user code.)
- stdout and stderr are **locked separately**, so a task's `print` and `eprint` may reorder relative to
  each other (as in Python).
- Output is **unbuffered**: a `print(x, end="")` progress marker appears immediately, without an
  `io.flush()`.
- **Nothing in the program ever waits on stdout's consumer.** A `print` hands the line to a background
  writer thread (one per stream) and returns; `flush` / `read_line` / `input` never wait on that thread
  either. So a slow consumer (`chezzi run x.chz | (sleep 60; cat)`) cannot pin a core worker or starve
  the other tasks.
- **A failed write is not silent.** A closed stdout reader (`chezzi run x.chz | head -1`) makes the
  next `print` raise the ordinary runtime fault `stdout closed (broken pipe)`, so an endless printer
  stops, and the run exits **non-zero** with a trace on stderr (`| head` closes only stdout). Python
  raises `BrokenPipeError` here. The halt fires **only where the program wrote stdout**: the printing
  job faults, and an `Executor` sibling that never printed runs to completion (as CPython's
  `ThreadPoolExecutor` runs every submitted job). A `parallel:`/`spawn` nursery aborts its siblings on
  any fault, broken pipe included (`concurrency.md` §8). A dead stdout does **not** halt via the
  `os.exit` channel, so a *crashing* program under `| head -1` still reports its fault and a non-zero
  code. Any other stdout I/O error (`ENOSPC`, `EIO`) also prints `chezzi run: cannot write stdout: …`.
- A failure on **stderr** is swallowed: it is a diagnostic channel, and a dead `2>` reader is no reason
  to kill a healthy program.
- A stdout fd that is CLOSED at startup (`chezzi run f.chz >&-`) is not an error: the Rust runtime
  reopens fd 0/1/2 on `/dev/null` before `main`, so every `print` succeeds into the void and the run
  exits 0 (CPython does the same).
- **These rules cover the VM's own sink only.** Bytes that an FFI call writes to the descriptor itself
  (`extern "libc": fn puts`) are outside every guarantee here: not line-atomic against `print`, not
  unbuffered, not ordered with it, and invisible to the broken-pipe halt, so a `| head -1` loop of C
  writes never faults. `ctypes` and `cgo` behave the same; see [`syntax.md` §12b](../syntax/17-ffi.md).

**Input contract (`read_line` / `read_all` / `read_char` / `input`).** stdin is **ONE source, shared
by every task**, like Go's `os.Stdin` and Python's `sys.stdin`. Any task may read it (`spawn:`/
nursery, `Executor.submit`, the entry task); no task gets a false EOF. `read_all` drains the whole
remainder (so a later read in any task sees EOF); `read_char` consumes one scalar at a time.

- A line goes to **exactly one** task: never duplicated, never dropped.
- **Which** task gets a given line is **nondeterministic**: concurrent readers race for lines, as in
  Go/Python. For a deterministic distribution, let the entry task read and fan the lines out over a
  `Channel[str]`.
- `None` means stdin is **exhausted** (a real EOF).
- Concurrent `io.input(prompt)` calls may interleave prompt and answer (as in Python): the prompt
  write and the read are not one atomic unit.
- **A blocked read does not starve other tasks.** `read_line`/`read_all`/`read_char`/`input` are not
  offloaded (they touch host stdio), but the engine demotes the worker for the call and hands its slot
  to a replacement thread, so a runnable sibling runs while a task waits on stdin, even at
  `--threads=1` (as in Go at `GOMAXPROCS=1`). K blocked readers cost K parked OS threads, not K core
  workers.

### `std.os`
| Function | Signature | Notes |
|----------|-----------|-------|
| `args` | `() -> List[str]` | Program args (the positionals after the script path). Decoded **lossily**: see the decoding note. |
| `env` | `(key: str) -> str?` | Environment variable (reads the injected env, see note). |
| `environ` | `() -> Map[str, str]` | ALL environment variables, **sorted by key** (deterministic across runs). Same source as `env`. Keys + values are decoded **lossily**: see the decoding note. |
| `setenv` | `(key: str, value: str) -> None` | Set an env var. `env` and `environ` see it, and it is **visible across tasks** (one env map for all M:N workers, process-global like Python `os.environ` / Go `os.Setenv`). Writes the injected env map, **not** a child's real env; `process.cmd` still inherits the real process env. |
| `getpid` | `() -> int` | Current process id. |
| `platform` | `() -> str` | OS name: `"linux"` / `"macos"` / `"windows"` / … (`std::env::consts::OS`). |
| `hostname` | `() -> str?` | System hostname (`None` on the rare `gethostname` syscall failure). |
| `home_dir` | `() -> str?` | User home (`$HOME`; `None` if unset). Unix-focused. **Stays `str`**: unlike `getcwd`/`temp_dir` it reads the injected env map, which is a lossy surface (see the decoding note). |
| `temp_dir` | `() -> path.Path` | System temp directory, as **raw OS bytes** wrapped in a [`path.Path`](07-pure-text-data.md#pathpath): `$TMPDIR` need not be valid UTF-8. |
| `getcwd` | `() -> path.Path!` | Current working directory (real process cwd), as **raw OS bytes** wrapped in a [`path.Path`](07-pure-text-data.md#pathpath), so a non-UTF-8 cwd round-trips. No type argument, no turbofish. `import std.path` to name the type. |
| `chdir` | `(p: PathLike) -> None!` | Change the **real process cwd** (`!e` on failure). **Process-global**: one cwd for all M:N workers, so a task's `chdir` shifts sibling tasks' relative paths (Python/Go have the same limit). |
| `exit` | `(code: int) -> None` | Hard, uncatchable halt (the checker sees an ordinary call: write `panic("unreachable")` after it where a value or a leave is required, like Go's `os.Exit`), unwinding past any `recover:`. **Does NOT run `defer`s.** The process status is the **low 8 bits** of `code` (`code & 0xff`), like POSIX `exit(3)` / bash / Python / Go: `os.exit(-1)` → **255**, `os.exit(300)` → **44**, `os.exit(0)` → `0`. (It is a *mask*, not a clamp: a negative code must never report SUCCESS.) |

**Env source:** `env` / `environ` / `setenv` read and write the engine's injected env config.
`chezzi run` and `chezzi test` fill it from the real process env; a Rust test harness injects its own. One env map is shared across M:N workers (an `Arc<Mutex<…>>`), so a
`setenv` inside a task is visible to the parent and siblings. A child that `process.cmd` spawns does
**not** see a `setenv` (it inherits the real process env). `getpid` / `platform` / `hostname` /
`home_dir` / `temp_dir` are plain queries.

**Non-UTF-8 argv / env (decoding rule):** the OS hands argv and the environment over as raw bytes,
which need not be valid UTF-8. A Chezzi `str` is UTF-8, so the CLI decodes both **lossily** at startup:
an invalid byte becomes `U+FFFD` (`args()` returns `"A�B"` where the shell passed `A\xffB`). Unlike
Python's `surrogateescape`, this is **not reversible**: the original bytes are gone, and two raw env
keys that decode to the same string collide (last one wins). Hostile bytes **never crash the CLI**.

<a id="pathpath-input"></a>
**`PathLike`: the path INPUT position.** A reserved universe protocol with one method,
`as_path(self) -> bytes`. `str` / `bytes` / `bytearray` satisfy it **intrinsically**; `path.Path`
satisfies it structurally. Every path-taking fn in `std.fs` / `std.io` / `std.os` / `std.path` takes
one, so a raw byte path never has to pass through a `str`, which cannot represent it. (`argv`/`env` are
a separate surface and stay lossy.)

**The CLI never takes a path from a lossy decode.** `U+FFFD` substitution is *not* injective: a raw
path `sc\xffipt.chz` and a real file named `sc\u{FFFD}ipt.chz` decode to the same string, and opening
the alias would run a *different* program. So `run` / `check` / `ast` / `tokens` / `test` **refuse** a
path argument that contains `U+FFFD` (`cannot use '…' as a path — it contains U+FFFD …`, rc=1). The
check is on the character, so a file whose name contains a literal `U+FFFD` is refused too.
**Limit:** you cannot run a script whose *path* is not valid UTF-8 (it fails cleanly, never runs the
wrong file). Support needs `OsString` in the resolver and module graph; see
[`gaps.md`](../../contributing/gaps.md).
