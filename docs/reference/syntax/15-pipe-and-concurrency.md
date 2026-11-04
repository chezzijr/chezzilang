## 11. Pipe operator `|>`

`|>` passes the left value as the first argument of the right call: `a |> f(x)` is `f(a, x)`. The right
side must be a call.

**Line continuation:** a line whose **first** token is `|>` continues the previous line. A line that
**ends** in `|>` is a parse error. Only `|>` continues (`|`, `||`, `|=` do not).

The offside rule holds: the `|>` line must be indented at least as deep as the block it continues. A
shallower `|>` line closes the block and is then a parse error:

```chezzi
fn f() -> int:
    r := 1
    |> dbl()       # OK — same indent as `r := 1`, and deeper is fine too
    return r
|> dbl()           # error: unexpected '|>' — column 0 is outside f's body
```

```chezzi
import std.iter

total := [1, 2, 3, 4]
    |> iter.filter(fn(x: int) -> bool: x % 2 == 0)   # → iter.filter([1,2,3,4], ...)
    |> iter.map(fn(x: int) -> int: x * 10)
    |> iter.sum()
print(total)                                         # 60

# equivalent without pipe:
# iter.sum(iter.map(iter.filter([1,2,3,4], fn(x: int) -> bool: x % 2 == 0), fn(x: int) -> int: x * 10))
```

- The right side must be a free call, so a method like `xs.sum()` cannot follow `|>`; `std.iter` has
  free-function forms for this.
- Named arguments are not accepted on the right side: `1 |> cnt(c=5)` is
  `named arguments are not supported on the right side of '|>'`.

## 11b. Concurrency — `spawn` / `parallel:`

This is a syntax summary. The full reference is [`concurrency.md`](../concurrency/01-model.md). `chezzi run`
uses the OS-thread M:N scheduler; set its worker count with `--threads=N` / `CHEZZI_THREADS`
(`0` = all cores).

```chezzi
fn worker(id: int, prefix: str, out: Channel[str]):
    out.send("{prefix}-{id}")

fn main():
    ch := Channel[str]()
    label := "task"
    parallel:                          # nursery — joins all children at the dedent
        spawn worker(1, label, ch)     # form 1: a named call (Go's `go f(x)`); args by handle
        spawn worker(2, label, ch)
        spawn:                         # form 2: an anonymous indented block
            out := heavy()
            ch.send(out)
    # reaching here ⇒ all children finished. No WaitGroup, no leaks.
    for _ in 0..3:
        print(ch.recv())               # each recv hands over one result

main()
```

- **`parallel:` is a nursery:** every task spawned inside joins at the dedent. `spawn` returns at once
  and the task starts there, beside the rest of the body (Go's `go f()`). The barrier guarantees
  completion, not start.
- **Every function body and the module top level is an implicit nursery.** A bare `spawn` is legal
  anywhere and joins at the body's `return`/end (the top level joins at program exit). `return`,
  fall-through and `?` are join points; `defer`s run after the join. A task cannot outlive the
  function that spawned it.
- **`return`, `break` and `continue` inside a `spawn:` block are compile errors**
  (`'return' is not allowed inside a spawn block: a spawned task has no caller to return to`;
  `'break' is not allowed inside a spawn block: a spawned task runs outside the loop`), also in an `if`/`for`/`match`/`wait:` arm in the block. Send the value on a
  `Channel`/`Shared`. The error names the innermost block the construct is in. A loop inside the
  task owns its own `break`. A `return` in a
  `parallel:` body (parent frame) or in a nested `fn` declared in the block is fine.
- **`?` (and `?.`) inside a `spawn:` block is a compile error** (`'?' is not allowed inside a spawn
  block: a spawned task has no caller to propagate to`), whatever the enclosing function returns and
  even under `recover:` or `defer:`. Send the error on a channel or `match` it in the task. Legal: a
  `?` in a `parallel:` body, in a nested `fn`/closure declared in the task, in a `recover:`
  inside the task, and in an argument of the call form `spawn f(g()?)` (arguments run in the parent). A `?` in a `defer:` inside the task is the `defer:` error.

```chezzi
fn fetch_all(urls: List[str]):
    for u in urls:
        spawn fetch(u)        # no `parallel:` needed — joins when fetch_all returns
    print("dispatched")       # the tasks are already running (Go's `go f()`); they JOIN at
                              # end-of-function, so print order against them is undefined
```

**Primitives** (details in [`concurrency.md`](../concurrency/01-model.md)):

| Name | From | Use |
|---|---|---|
| `Channel[T]` | global | buffered FIFO: `send`, `recv() -> T!RecvError` (`!RecvError` once closed and drained), `try_recv() -> T?`, `try_send(v) -> bool`, `len`, `close`, `trip()`; `for v in ch:` drains until closed |
| `Shared[T]` | `std.concurrency` | cross-task mutable box: `get`, `set`, `update(fn)` |
| `RwShared[T]` | `std.concurrency` | reader-writer sibling of `Shared` |
| `Atomic[T]` | `std.concurrency` | atomic box: `load`, `store`, `exchange`, `cas`, numeric `add`/`sub` |
| `AtomicInt` | `std.concurrency` | lock-free int atomic, same methods, about 2.7× faster than `Atomic` on a contended counter |
| `timer(ms) -> Channel[bool]` | `std.time` | one-shot timeout channel: `time.timer(500).recv()` yields `?true` after about 500 ms; use it in a `wait:` arm |
| `Token` | `std.cancel` | cancellation token from `cancel.manual()` / `cancel.timeout(ms)`: `cancelled()`, `done() -> Channel[bool]`, `cancel()`, `reason()`, `deadline_at()`, `derive()` (a child; cancelling a parent cancels its children, never the reverse). Cooperative: poll it |

- After `close()`: `send` faults, `recv` drains then returns `!RecvError` on every call (no fault),
  `try_send` returns `false`.
- A channel hands over the value itself (Go `chan T`): the sender and the receiver share one object.
- `std.concurrency` members import like any other: `import Shared from std.concurrency` (also with
  `as`), or `import std.concurrency` then `concurrency.Shared(0)` / `std.concurrency.Shared(0)`. A
  whole-module import binds no bare name. A generic over an `Atomic` payload needs the sealed bound
  `StructuralEq`: `fn mk[T: StructuralEq](v: T) -> Atomic[T]`.
- `net.Socket` / `net.Listener` and `std.ffi`'s `ptr` / `owned_str` / widths are type-only:
  `net.Socket(...)` is rejected. `time.timer` in type position is `module 'time' has no type 'timer'`.
- The ladder: a value one task writes → a value tasks share and order by a join or a `Channel` →
  `Shared[T]` (writes under a guard). See [`03-shared.md`](../concurrency/03-shared.md).

**`wait:` (select)** races channel `recv` and `send` arms; the first ready arm wins, in source order.

- A recv arm: `v := ch.recv():` (or `result = ch.recv():`, `_ := ch.recv():`, a tuple target
  `a, (b, c) := ch.recv():` or `a, (x.f, ys[0]) = ch.recv():`).
- A send arm: `ch.send(v):` binds nothing and is ready when the channel can take the value.
- A guard switches an arm off: `j := jobs.recv() if not paused:`, `ch.send(v) if ok:`. It is a `bool`,
  checked once on entry before the arm's operands, and cannot see the arm's binding. Every arm off
  with no `else` faults `"wait: every arm is off ..."`.
- An optional non-blocking `else:` (last), and `timer` arms for timeouts (there is no `recv_timeout`).
- A closed and empty recv arm is skipped. A closed send arm faults `"send on a closed channel"`. All
  arms closed with no ready send and no `else` faults.
- See [`concurrency.md`](../concurrency/01-model.md) §6d,
  [`examples/wait_select.chz`](../../../examples/wait_select.chz) and
  [`examples/wait_send.chz`](../../../examples/wait_send.chz).

**Task boundary.** A task gets the parent's values by handle (M3.4), as a goroutine does: a captured
local, an argument and a spawned receiver are the parent's own, so a write in the task is seen after
the join (§3, "Closure capture"). A module global is the run's one copy (M3.1). Every value can
cross; a module is a qualifier, not a value. Two tasks that write one value with no join, `Channel`
or box between them race (see [§7](../concurrency/07-sendability.md)).
