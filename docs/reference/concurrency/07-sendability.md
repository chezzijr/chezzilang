## 7. Sendability

**The model — a task shares the parent's values (the Go rule).** A `spawn`ed task, a `spawn:`
block and an `Executor` job hold the values they get by handle, as a goroutine does: a captured
local is one cell the task and the parent share (a loop variable gets a cell per iteration), and a
`spawn f(xs)` argument, a `spawn obj.m()` receiver and a job's captures are the parent's objects
(M3.4). A module global is the run's one copy (M3.1). A value sent over a `Channel`, stored in an
`Atomic` or returned through a `Task` is the sender's object (M3.2), and a `Shared` / `RwShared`
box holds one value behind its guard (M3.3). Nothing that a task gets is a copy.

> **One-sentence mental model:** a task sees the parent's objects; order every write another task
> reads with a join, a `Channel` or a box (`Shared[T]`, `RwShared[T]`), or the two tasks race, and a
> race is the program's bug (Go).

```chezzi
fn fill(m: Map[int, int]):
    m[2] = 2

fn main():
    xs: Map[int, int] = {}
    n := 0
    parallel:
        spawn:
            xs[1] = 1
            n = 5
        spawn fill(xs)
    print(xs.len(), n)       # 2 5, as in Go
main()
```

`tests/chz/spec/spawn_handle_test.chz` pins the rule against the equivalent Go program.

### What is sendable

Every value is sendable. A **module namespace** is not a value: `spawn take(math)` is the same
compile error as `take(math)` (`module 'math' is not a value`), as Go rejects `go take(math)`
(`use of package math not in selector`). The concurrency handles (`Channel`, `Atomic[T]`,
`AtomicInt`, `Shared[T]`, `RwShared[T]`, `Executor`) and the I/O handles (`Writer`, `Reader`,
`Socket`, `Listener`) carry one core that lives outside every heap; every other value is an object
of the run's one heap.

### Writes from a task

A task's write to a value it shares with the parent lands on that value: a reassigned captured
local, a `push` on a captured list, a field store on a spawned receiver, a write a callee makes to
its argument. The parent reads it after the join:

```chezzi
c := Counter(0)              # struct Counter: n: int, fn bump(self): self.n += 1
parallel:
    spawn c.bump()
print(c.n)                   # 1
```

Two tasks that write one value without a join, a `Channel` or a box between them race. The race is
memory-safe ([§2](01-model.md), "Data races"): a wrong value, a missed map hit or an
out-of-range task fault, never a VM panic. M3.4 deleted the D4 copy rule (the checker's layer A and
the "this task's copy" fault on a spawn's values); its record is
[`adr-d4-airlock.md`](../../design/adr-d4-airlock.md).

### Generators

A live generator is an object like any other: a task that gets one drives the parent's generator,
so the parent's `next()` after the join continues where the task left it. Two tasks that drive one
generator at once race; the one that calls `next()` while the other runs the body faults
`generator already running` (CPython: `ValueError: generator already executing`), and `recover:`
catches it.
