## 6. Control flow

```chezzi
if x > 0:
    print("pos")
elif x == 0:           # 'elif' (Python-style single keyword), chainable
    print("zero")
else:
    print("neg")

for i in 0..10:        # range: 0..10 is 0 through 9 (end-exclusive, ascending only)
    print(i)
for i in 0..=10:       # inclusive: 0 through 10

r := 0..3              # a value: bind it, pass it, return it, store it, send it across a spawn
print(r)               # 0..3       — display is the source form (`0..=3`, `..3`, `3..`, `Range(0, 10, 3)`)
print(3 in r, r.len()) # false 3    — `Contains[int]`; `len` needs a start and an end
print(List(r), Set(r)) # [0, 1, 2] {0, 1, 2}  — `Iterable[int]` (NOT an `Iterator`: iterate it twice)
print(r == Range(0, 3))  # true      — Eq and Hashable on the four fields; a `Range` is a map key
for i in Range(10, 0, -1):   # 10, 9, 8, … , 1
    print(i)

for item in items:     # iterate a list
    print(item)

for kv in counts:      # iterate a map → (key, value) pairs (insertion order)
    print(kv)

for k in counts.keys():  # the keys are `counts.keys()`
    print(k)

for k, v in counts:    # two names unpack each (key, value) pair like any tuple element
    print("{k}={v}")

for a, b in pairs:     # destructure a tuple element (see "One unpacking grammar" in §2)
    print("{a}:{b}")

while cond:
    cond = step()

# `break` exits the innermost loop; `continue` skips to the next iteration.
```

### Ranges

A range is a value of type `Range` (the struct of `std/prelude.chz`, in scope in every module).

| Form | Meaning |
|---|---|
| `a..b` `a..=b` `..b` `a..` `..` | the literal forms; `..` binds below `\|` and above `==`/`in` |
| `Range(a, b)` `Range(a, b, step)` | the constructor, the only place for a step. A negative step counts down (`Range(3, 0, -1)` yields 3, 2, 1); a step of 0 faults |

- Readable fields: `start: int?`, `end: int?`, `step: int = 1`, `inclusive: bool = false`.
- `for i in 5..:` runs until a `break` (at the top of `int` it faults with `integer overflow in Add`).
  `3 in 5..` and `xs[5..]` work. `List(5..)` runs until memory ends.
- A range with no start (`..5`, `..`) cannot be iterated: `a range with no start cannot be iterated`
  (a check error for a literal, a fault for a value).
- `a()..b()` runs `a()` first, in every position.
- `for i in a..b` over a literal range is a counter loop (no allocation). Iterating a range value
  (`for i in r`) costs about 3.3x as much ([`benchmarks.md`](../../contributing/benchmarks.md)).

### Iteration over a collection is a snapshot

`for` over a List/Map/Set iterates a shallow snapshot of the spine, taken when the loop starts.
Pushing, popping or inserting in the body does not change how many steps run, and new entries are
not visited. Python raises instead:

```chezzi
m := {"a": 1}
for k in m.keys():         # runs ONCE. Chezzi: 1 visit, m.len() ends at 2, rc=0.
    m["b"] = 2             # CPython: RuntimeError: dictionary changed size during iteration
```

- The snapshot holds element values, not slots: overwriting a slot in the body (`xs[2] = 30`,
  `m["b"] = 99`) is invisible to the loop (CPython and Go read `xs[i]` live). Mutating a struct element
  in place (`ps[1].v = 20`) is visible.
- Collect additions and apply them after the loop if you need them.
- The same rule holds for a `.iter()` cursor and for the callback methods listed under
  "snapshotting" in [`stdlib.md`](../stdlib/01-builtins.md).

### Iterators, `Iterable[T]` and `Iterator[T]`

A user struct with `next(self) -> T?` is iterable: `for` calls `next()` each step until it returns
`None` (an infinite iterator plus `break` terminates).

```chezzi
struct Counter:
    n: int
    limit: int
    fn next(self) -> int?:
        if self.n >= self.limit:
            return None
        v := self.n
        self.n = self.n + 1
        return ?v

for x in Counter(0, 5):    # x binds the element type (int); a next() yielding a tuple destructures (for a, b in …)
    print(x)
```

`Iterable[T]` is a protocol bound: a generic fn can take any iterable (list/set/map/str/bytes, a
`.iter()` cursor, a generator, a user struct with `next`/`iter`) and recover its element type `T`. It
also works as a plain parameter type:

```chezzi
import Iterable from std.iter

fn first_or[S: Iterable[T], T](xs: S, default: T) -> T:
    for x in xs:           # x is typed `T`
        return x
    return default
first_or([10, 20], 0)      # 10   (T = int, recovered from the list element)
first_or("hi", "?")        # "h"  (T = str)
fn total(xs: Iterable[int]) -> int:
    n := 0
    for v in xs:           # v is typed `int`
        n += v
    return n
total([1, 2, 3])           # 6   (a set, a map's keys, a cursor or a generator work the same)
```

Lazy sequences are adapter structs over this protocol (Rust-style; see
[`examples/iter_adapters.chz`](../../../examples/iter_adapters.chz) for `Take`/`Mapped`).

Every collection has `.iter()`, which returns a cursor of type `Iterator[T]` with `.next() -> T?` (a
value per step, then `None` on every later call). It lets a plain collection enter an adapter pipeline:

```chezzi
pipe := Mapped(Take([10, 20, 30, 40].iter(), 2), fn(x): x * 2)   # a list → Take → Mapped
for v in pipe:             # 20, 40
    print(v)
it := {1: "a", 2: "b"}.iter()   # map iterates (key, value) pairs; str → 1-char str; bytes/bytearray → int 0..=255
print(it.next())           # 1
```

`Iterable` only promises `iter()`; `Iterator` also has `next`. Every `Iterator` is `Iterable`:
`iter()` on a cursor or generator returns itself, and `s.iter()` in an `[S: Iterable[T]]` body returns
the value itself when its type has `next` and no `iter`. A user struct with only
`iter(self) -> Iterator[E]` is for-iterable too (one `.iter()` call drives it).

```chezzi
import Iterable from std.iter

fn total[S: Iterable[int]](src: S) -> int:
    sum := 0
    for x in src.iter():
        sum = sum + x
    return sum
total([1, 2, 3])           # 6   (a list)  — also accepts a generator or a `next`-struct
List([5, 6, 7].iter())     # [5, 6, 7]   (a cursor IS an Iterator[T], so List()/Set() drain it)
```

- A cursor snapshots the collection at `.iter()`.
- Each `.iter()` is a fresh cursor, but an exhausted cursor yields nothing on a second pass (there is
  no compile-time multi-pass check).
- `for`, `List(it)` or `Set(it)` on a named cursor consumes it in place, so a partial `for … break`
  leaves the rest for a later `next()`/`List()`.
- A cursor crosses `spawn` as a deep copy, like a `list`. `Iterable` and `Iterator` are members of `std.iter`.

### Generators (`yield`)

A fn that uses `yield` is a generator: a call returns a suspended iterator. It runs lazily, suspends
at each `yield` and resumes on the next `.next()`. The `-> Iterator[T]` annotation is required, and
every `yield` must be assignable to `T`.

```chezzi
import Iterator from std.iter

fn count_up(n: int) -> Iterator[int]:
    i := 0
    while i < n:
        yield i            # produce a value, suspend until the next .next()
        i = i + 1
for x in count_up(3):      # drives the generator: prints 0, 1, 2
    print(x)
g := count_up(2)
match g.next():            # 0
    ?v: print(v)
    None: print(-1)
```

- A generator can be a struct method (`fn m(self)`). A generator value is an `Iterator[T]`: drive it by
  `for`, pass it to an `[S: Iterator[T], T]` bound, or call `.next()` (`v` per yield, then `None`).
- A bare `return` stops a generator early. `defer`/`spawn`/`parallel:`/`wait:` are not allowed inside one.
- An un-inferable element (`yield []` alone, or an int-then-float mix; no int->float coercion at a
  `yield`) is rejected.
- A generator crosses a task boundary by value, as a deep copy of its execution state (also when it is
  suspended in `recover:`, and also when it is a module global). A value in its frame is a task copy
  only when the parent can still reach it: a frame-local list the parent never saw stays writable in
  the task; a list the generator yielded and the parent holds faults on write.
- Examples: [`examples/generators.chz`](../../../examples/generators.chz),
  [`examples/generators_inferred.chz`](../../../examples/generators_inferred.chz),
  [`examples/generators_basic.chz`](../../../examples/generators_basic.chz).

### The `pass` keyword

`pass` is a reserved keyword with two roles.

**(1) A no-op statement.** `pass` is valid anywhere a statement is (a fn/method body, an `if`/`else`
branch, a `for`/`while` body, a statement `match` arm, a concurrency block) and is the idiomatic empty
body. A function whose body is a lone `pass` returns `None`, like a lone `return`:

```chezzi
fn todo():
    pass                # empty body; returns None (same as `return`)

for x in xs:
    pass                # a deliberately-empty loop body
```

`pass` is a statement, so it is not valid in an expression-position `match` arm. The exception is a
closure body after `:` (`fn(): pass`), the no-op closure.

**(2) An empty-body marker for `protocol` and `struct`.** These bodies hold declarations, so a sole
`pass` line means "empty body":

```chezzi
protocol Top:          # zero methods → an accept-all top type (like `Any`)
    pass

struct Unit:           # zero fields → `Unit()` takes no args
    pass
```

- An empty protocol is satisfied by every type. This is how `Any` (§5) is defined: `fn f(x: Top)`
  accepts an `int`, a `str`, a struct; `xs: List[Top] = [1, "a", true]` checks.
- An empty struct: `Unit()` constructs it, prints as `Unit()`, and two `Unit()` are equal. It is no
  key on its own: a `Set` element or `Map` key needs `hash` and `eq`. It still heap-allocates.
- `pass` must be the only line of such a body; mixed with a field/method or repeated it is a parse error.
- An empty enum is not supported: an enum body with no variant (`pass`, methods only, `test` blocks only) is rejected at the enum name.

`pass` cannot be a name (variable, parameter, field, function, type, module alias): `pass := 5` is an error.

## 6b. Indexing & slicing

```chezzi
xs := [10, 20, 30, 40]
print(xs[1])           # 20    — index
xs[1] = 99             # mutate in place
print(xs[-1])          # 40    — negative index counts from the end
xs[-1] = 0             # negative index works as an assignment target too
sub := xs[1..3]        # slice: a Range value → [99, 30]
print("hello"[0..2])   # he    — strings slice too (→ a new str)
print(xs[1..99])       # [99, 30, 0]   — slice bounds are clamped (no panic)
print(xs[2..])         # [30, 0]       — open end (defaults to len)
print(xs[..2])         # [10, 99]      — open start (defaults to 0)
print(xs[..])          # full copy
print(xs[0..=2])       # [10, 99, 30]  — inclusive end
print(xs[Range(0, 4, 2)])   # [10, 30]  — a step goes through the constructor
print(xs.reversed())   # [0, 30, 99, 10]  — a NEW list
print(xs[-2..])        # [30, 0]       — negative bounds count from the end
```

Slicing takes a `Range` value: `xs[a..b]`, `xs[a..]`, `xs[..b]`, `xs[..]`, or any `Range` (`xs[r]`,
`xs[Range(a, b, step)]`). One function resolves it against a length, for `List`, `str`, `bytes`,
`bytearray` and `ConcurrentList`:

1. A missing start is `0` and a missing end is `len` (for a negative step: `len - 1` and "before
   index 0", as CPython).
2. A negative bound `b` becomes `b + len`.
3. An inclusive end adds one after step 2 (for a negative step it subtracts one).
4. Both bounds clamp to the sequence.
5. A start at or past the end gives an empty result.

With `xs = [10, 20, 30]`: `xs[-2..]` is `[20, 30]`, `xs[0..=-1]` is `[10, 20, 30]`, `xs[2..1]` and
`xs[-1..0]` are `[]`, `xs[1..99]` is `[20, 30]`.

- A plain index is strict: `xs[-4]` faults, and `xs[-100]` on a short list is
  `index -100 out of bounds (len N)`. A slice bound `xs[-100..]` clamps (Python's asymmetry).
  Negative indexes work as assignment targets.
- A `step` of `0` cannot be built (`Range() step cannot be zero`).
- A subscript by a `Range` is a slice, or a key lookup on a `Map[Range, V]`. A type that declares both
  a range `slice` and an `index` whose key accepts a `Range` is the declaration error
  `a type takes a range as a slice or as a key, not both`. A `Map` has no `slice`, so `m[0..3]` is a
  key lookup.
- `xs[a..b] = v` is the check error `cannot assign to a slice`.
- `List[T]` slices to `List[T]`, `str` to `str`. Indexing and slicing are protocols, so custom types
  opt in (`Index`/`IndexSet`/`Slice` in §7b). A user `Slice` impl is `slice(self, r: Range) -> R` (the
  parameter is named `r`); `return self.items[r]` gives the standard result.
- `reversed()` returns a NEW value on `List`, `str`, `bytes` and `bytearray`; `reverse()` changes in
  place and exists on `List` and `bytearray` only.

## 6c. Comprehensions

```chezzi
[x * 2 for x in xs]              # list: map each element
[x for x in xs if x > 0]         # list: with an `if` guard
[i for i in 0..10]               # over a range (any iterable works)
{x % 3 for x in xs}              # set: duplicates collapse
{k: v * 10 for k, v in scores}   # map: `for k, v` binds a map's entries
[x + y for x in xs for y in ys]  # nested: cartesian product (ys inner-most, like nested for-loops)
[y for xs in xss for y in xs]    # nested: a later clause references an earlier clause's variable
[x for x in xs if x > 0 for y in ys]   # a guard may follow ANY clause (filters that clause)
```

- One or more `for` clauses, each binding a name or a tuple target (`for k, v in m`,
  `for a, (b, c) in xs`), each followed by optional `if` guards.
- Clauses nest like `for` loops (first outermost), so a later clause may use an earlier binding, and a
  guard after a clause filters at that level (Python semantics).
- Loop variables are scoped to the comprehension. The iterable is anything `for` accepts. Set elements
  and map keys must be `Hashable`.
