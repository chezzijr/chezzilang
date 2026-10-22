# Chezzi method-surface audit (2026-10-22)


The main loop re-ran these claims on main `5b7b9358`: `Shared.read` keeps a mutation (`[1, 2]`) and `RwShared.read` loses it (`[1]`); `min_by` takes a key; `Map.has` is `has no method`; `Some(1)` is `unknown name` (the user spelling is `?1`, so D7 is a stale prelude comment); F1 prints both errors. The probe programs were in the session scratchpad.

Binary: a copy of `target/release/chezzi` at HEAD `5b7b9358`. Sources: `std/*.chz`, `std/concurrency/*.chz`,
`docs/reference/stdlib/`, `docs/reference/concurrency/`. The builtin method set has one owner, the bodyless
`native fn` decls (`docs/contributing/owners.md:82`). The only undeclared builtin method is `.iter()` on
`List`/`Map`/`str`/`Channel` (`vm/method_call.rs:446-614`, checker `method_call.rs:91`). `int`, `float`,
`tuple`, `Option` and `Result` have no methods (probed: `(5).str()`, `(1).hash()`, `(1,2).len()`,
`(1.5).is_nan()` are all `has no method`). Paths below are relative to the repo root.

Key: **P** = Python, **R** = Rust, **G** = Go.

## 1. Kind B groups: one function split by argument form or error convention

| # | Function | Members (file:line) | Python | Rust | Go | Proposed single spelling | Needs feature? |
|---|---|---|---|---|---|---|---|
| B1 | Sort a list | `List.sort()` prelude.chz:72; `List.sort_by(cmp)` :80; `List.sort_by_key(key)` :81 | `list.sort(*, key=None, reverse=False)` (keyword-only, measured) | `sort`, `sort_by(cmp)`, `sort_by_key(key)` | `slices.Sort`, `slices.SortFunc(cmp)` | `sort(key=..., reverse=false)`; a comparator goes through a `cmp_to_key` helper | YES: a default for a type param (`K = T`) or a generic default value. Today `key: fn(T) -> K = identity` is rejected (§5, F1/F5). Native methods also reject named args (§5, F4). |
| B2 | Smallest / largest element | `List.min()` :82, `max()` :83, `min_by(key)` :84, `max_by(key)` :85; `cmp.min(a,b)` cmp.chz:32, `cmp.max(a,b)` cmp.chz:25 | `min(it, key=, default=)` | `min()`, `min_by(cmp)`, `min_by_key(key)` | `slices.Min`, `slices.MinFunc(cmp)`, builtin `min(a,b)` | `min(key=...)` / `max(key=...)`. If the Rust trio stays: rename `min_by` -> `min_by_key` (Chezzi `min_by` takes a KEY, Rust `min_by` takes a COMPARATOR, and Chezzi `sort_by` takes a comparator) | Same as B1 |
| B3 | Parse an int / float from text | `str.to_int()` prelude.chz:212 (`int?`); `to_float()` :213; `parse_int()` :216 (`int!str`); `parse_float()` :217; `int(x)` ctor :29 (faults: `int(): cannot parse '12x'`); `math.parse_int_base(s, base)` math.chz:72 (`int!`); `json.as_int` json.chz:450 | `int(s)` raises `ValueError`; `int(s, base)` | `s.parse::<i64>() -> Result`; `i64::from_str_radix` | `strconv.Atoi`, `strconv.ParseInt(s, base, 64)` -> `(int, error)` | `s.parse_int(base: int = 10) -> int!str`, `s.parse_float() -> float!str`. Drop `to_int`/`to_float`: `s.parse_int() ?? 0` works today (measured: prints `12`). Fold `math.parse_int_base` into `base` | An `int!str -> int?` conversion (Rust `.ok()`) if a caller needs the carrier: `r.ok()` is `has no method 'ok'` today |
| B4 | Find the index of an element / substring | `List.index_of(v)` prelude.chz:69 (`-1`); `List.position(pred)` :102 (`int?`); `str.index_of(sub)` :204 (`-1`); `string.index_of(s, sub)` string.chz:66 (`-1`); `string.find(s, sub, from_index)` string.chz:162 (`-1`); `iter.find(xs, pred)` iter.chz:139 (`T?`, returns the element) | `list.index(x)` raises; `str.find(sub, start)` -1; `str.index` raises | `iter().position(pred) -> Option`; `str::find(pat) -> Option<usize>` | `slices.Index` -1; `slices.IndexFunc` -1; `strings.Index` -1 | `index_of(x, start: int = 0) -> int?` on `List`, `str`, `bytes`; `position(pred) -> int?` stays. Retire the `-1` sentinel and the `string.find`/`string.index_of` free fns | No (native default args exist: `Socket.read(timeout_ms = 0)`). Breaking change |
| B5 | Strip whitespace | `str.trim()` prelude.chz:177; `str.strip()` :210 ("a trim alias") | `strip()` | `trim()` | `strings.TrimSpace` | One name. `strip` (Python, the syntax owner) | No |
| B6 | Split a string | `str.split(sep)` prelude.chz:180; `str.split_lines()` :208; `string.split(s, sep, maxsplit=-1)` string.chz:184; `string.rsplit` :203; `string.split_whitespace` :218; `regex.split` regex.chz:29 | `split(sep=None, maxsplit=-1)`, `rsplit`, `splitlines` | `split`, `splitn`, `rsplitn`, `split_whitespace`, `lines` | `strings.Split`, `SplitN`, `Fields` | `str.split(sep: str? = None, maxsplit: int = -1)`, `str.rsplit(...)`, `str.split_lines()`. Remove the free-fn twins | No |
| B7 | Count up a tally | `Counter.add(x)` collections.chz:203 (+1); `Counter.add_n(x, n)` :207; `ConcurrentCounter.increment(k)` collection.chz:211 (+1); `ConcurrentCounter.add(k, n)` :215 (+n) | `Counter.update`, `c[x] += n` | `*m.entry(k).or_insert(0) += n` | `m[k] += n` | `add(x, n: int = 1)` on both counters. Today `add` means +1 on `Counter` and +n on `ConcurrentCounter` | No (user-fn defaults work, §5 F6) |
| B8 | Submit work to an executor | `Executor.submit(f) -> None` concurrency.chz:90; `Executor.submit_result(f) -> Channel[T!]` :103; `task.submit_task(ex, f) -> Task[T]` task.chz:69 (a free fn) | `executor.submit(fn) -> Future` | tokio `spawn(fut) -> JoinHandle` (droppable) | `go f()`; `errgroup.Group.Go(f)` | One `ex.submit(f) -> Task[T]`; a dropped handle is fire-and-forget | YES: an owner decision on fault routing. `submit` faults end the run, a handle job never faults (08-executor.md:117). Also a method on a native struct that returns a type of another std module |
| B9 | Stop an executor | `Executor.shutdown()` concurrency.chz:91; `shutdown_now()` :92 | `shutdown(wait=True, *, cancel_futures=False)` | tokio `Runtime` drop / `shutdown_timeout` / `shutdown_background` | `cancel()` then `wg.Wait()` | `shutdown(cancel: bool = false)` (Python), or keep the Java pair `shutdown`/`shutdownNow` (owner decides) | No |
| B10 | Read-modify-write under the exclusive guard | `Shared.update(f)` concurrency.chz:49; `RwShared.write(f)` :61 | `with lock:` | `Mutex::lock`, `RwLock::write` | `mu.Lock()` | One name on both boxes: `update` | No |
| B11 | Copy the whole value out of a shared holder | `Shared.get()` :47; `RwShared.get()` :59; `Atomic.load()` :65; `AtomicInt.load()` :82; `ConcurrentList/Map/Set.snapshot()` collection.chz:146/98/192; `Task.get()` task.chz:50 | n/a | `lock().clone()`; `Atomic*::load` | `atomic.Value.Load` | `load`/`store` for atomics (R and G agree); `get`/`set` for boxes; `copy()` for the concurrent collections (matches `List/Map/Set.copy()`) | No |
| B12 | Hash / encode / read / write a str or a bytes value | `crypto.sha256`/`sha256_bytes` crypto.chz:12/15 (also sha1, sha512); `encoding.base64_encode`/`base64_encode_bytes` encoding.chz:15/22; `io.read_file`/`read_bytes` io.chz:38/48; `Writer.write`/`write_bytes` io.chz:70/71; `Socket.read`/`read_bytes`, `write`/`write_bytes` net.chz:44-47; `process.run`/`run_bytes` process.chz:23/31; `request.get`/`get_bytes` request.chz:28/29 | `hashlib` takes bytes only; `open(mode="rb")` | generic `AsRef<[u8]>` input | `[]byte` input, `string(b)` | One fn per verb with a byte-like input (`str | bytes`), and a `_bytes` twin only where the RESULT type differs (read) | YES: a byte-like protocol for a native param (precedent: `PathLike`, untyped `bytearray.extend`) |
| B13 | Read / write memory at a pointer | `ffi.load_int`/`load_int_at` ffi.chz:50/51 ... 14 pairs; `store_*`/`store_*_at` :82-107 | `ctypes` | `ptr.add(off).read()` | `unsafe.Add` | `load_int(p, off: int = 0)`: 28 decls become 14 | No |
| B14 | Run a subprocess | `process.cmd` :22; `run` :23; `run_args` :24; `run_bytes` :31; `run_args_bytes` :32 | `subprocess.run(args, capture_output=, text=)` | `Command::new(c).args(a)` | `exec.Command(name, args...)` | `run(command, args: List[str] = [])` plus one bytes twin (B12) | No |
| B15 | Same verb as a free fn and as a method | `cancel.derive(parent)` cancel.chz:208 and `Token.derive()` :144; `string.replace/repeat/pad_left/...` string.chz:19-101 and the `str` methods; `string.reverse(s)` string.chz:23 and `str.reversed()`; `iter.map/filter/fold/sum/count/find` iter.chz:59-139 and `List.map/filter/fold/sum/count/position` | Python 3 removed the `string` module fns | method only | package fn only | Methods only. Move `capitalize`/`title`/`swapcase`/`rsplit`/`split_whitespace` onto `str`. `iter.sum(List[int])` is narrower than `List.sum` | No |

Groups I checked and kept (one ancestor uses the same split for a type reason):
`FlagSet.str_flag/bool_flag/int_flag` + `get_str/get_bool/get_int` (Go `flag.String/Bool/Int`), `math.ln/log/log2/log10`
(Rust), `Logger.log_at` + `debug/info/warn/error` (Python `logging`), `bytes.decode`/`decode_lossy` (Rust
`from_utf8`/`from_utf8_lossy`).

## 2. Kind A axes: different functions on one axis

| Axis | Types and spellings | Consistent? |
|---|---|---|
| In place vs new copy: reverse | `List.reverse`/`reversed` prelude.chz:60/88; `bytearray.reverse`/`reversed` :240/241; `str.reversed` :197; `bytes.reversed` :227; `string.reverse(s)` string.chz:23 **returns a new string** | NO: `string.reverse` uses the in-place name for a copy |
| In place vs new copy: sort | `List.sort`/`sort_by`/`sort_by_key` in place only; no `sorted` | NO: `reverse` has a copy twin, `sort` has none (P has `sorted`) |
| In place vs new copy: dedup | `List.unique`, `List.dedup` return a new list prelude.chz:93/94 | NO vs Rust: `Vec::dedup` mutates in place. Same name, other axis value |
| Mutate vs return new: combine | `List.extend` / `concat` :71/70; `Map.update` / `merge` :116/115; `Set.union/intersection/difference` new only :127-129; `bytearray.extend` only :236 | NO: two word pairs for one axis (`extend/concat`, `update/merge`). `Set` has no in-place form (P `set.update`, R `HashSet::extend`). R uses `extend` on Vec and HashMap |
| Blocking vs non-blocking | `Channel.send`/`try_send` :143/148; `recv`/`try_recv` :152/153 | YES (only `Channel` has the axis) |
| Graceful vs cancel | `Executor.shutdown`/`shutdown_now` | YES within one type (see B9) |
| Locked generic vs lock-free int | `Atomic[T]` / `AtomicInt`: identical 6 method names concurrency.chz:64-87 | YES |
| Strict vs lossy decode | `bytes.decode`/`decode_lossy` :222/225; `bytearray.decode` only :238 | NO: `bytearray` has no `decode_lossy` |
| Shallow copy | `List/Map/Set/bytearray.copy()`; `Concurrent*.snapshot()`; `Deque`/`Heap`/`Counter`: none | NO: `snapshot` vs `copy`; std containers lack `copy` |
| Positional vs at offset | `ffi.load_X`/`load_X_at`, `store_X`/`store_X_at` | YES inside ffi, but see B13 |

## 3. Convention drift

| Convention | Types and spellings | Drift |
|---|---|---|
| Miss on a lookup / search | `-1`: `List.index_of`, `str.index_of`, `string.find`, `string.index_of`. `T?`: `List.position`, `List.first/last/pop/min/max`, `Map.get`, `Map.remove`, `ConcurrentList.at`, `Heap.peek/pop`, `Deque.peek_*/pop_*`, `iter.find`, `json.get/at`, `Channel.try_recv`, `os.env`. `bool`: `Set.remove`. `0`: `Counter.count`, `ConcurrentCounter.count`. Fault: `xs[i]`, `m[k]` (`key not found`), `List.remove(i)`, `Channel.recv` on closed, `iter.reduce([])`, `int("x")`. `T?!`: `regex.find` | YES: 5 shapes. `-1` is the only shape that a later index reads as a valid element |
| `len` vs `size` | `len` on every container, `str`, `bytes`, `Range`, `Channel`, `Heap`, `Deque`, concurrent collections; `fs.size(path)` is a file size | No drift |
| `is_empty` | `Heap.is_empty`, `Deque.is_empty` collections.chz:85/173; `string.is_empty(s)` free fn string.chz:13; `List/Map/Set/str/Counter`: none (probed `[1].is_empty()`: no method) | YES |
| `contains` | `List/Map/Set/str/Range/ConcurrentMap/ConcurrentSet.contains`; `bytes`/`bytearray`: none (probed); docs say `has` (§4 D1); box primitives `_has` (Map) vs `_contains` (Set) base/builtin_method.rs:21-22; `ConcurrentMap.contains(item: K)` collection.chz:71 names a key `item` | YES |
| `remove` | `List.remove(i) -> T` by INDEX, faults :92; `Map.remove(k) -> V?` :112; `Set.remove(v) -> bool` :126; same on `ConcurrentMap/Set`; `bytearray`/`Deque`/`Heap`: none | YES vs Python: P `list.remove(x)` removes by VALUE and P `list.pop(i)` is the by-index form. Matches Rust `Vec::remove` (documented choice 02-methods.md:150) |
| `insert` | `List.insert(i, x) -> None` clamps, never faults :89; `Map.insert -> V?` :114; `Set.insert -> bool` :125; `bytearray`: none | YES: `insert(99, x)` appends, `remove(99)` faults (measured). R `Vec::insert` panics past `len` |
| `get` | `Map.get(k) -> V?`; `ConcurrentMap.get(k)`; `Shared/RwShared.get()` copies the whole value; `Task.get() -> T!`; `FlagSet.get_str(name)`; `json.get(j, k)`; `List`: no `get`, `ConcurrentList.at(i) -> E?` | YES: `get` means keyed lookup on maps and whole-value copy on boxes; `at` vs a missing `List.get` |
| `_by` suffix | `sort_by(cmp)`, `sort_by_key(key)`, `min_by(key)`, `max_by(key)` | YES: `_by` takes a comparator on `sort`, a key on `min/max`. R: `_by` is always a comparator |
| first / last / peek | `List.first/last` :86/87; `Deque.peek_front/peek_back`; `Heap.peek` | YES vs R `VecDeque::front/back`, `BinaryHeap::peek` |
| `count` | `List.count(pred)` :99; `str.count(sub)` :205; `Counter.count(x)`; `ConcurrentCounter.count(k)`; `iter.count(start, step)` is an infinite iterator iter.chz:159 | YES: 3 argument kinds under one name. P `list.count(x)` takes a value |
| `add` | `Counter.add(x)` (+1); `ConcurrentCounter.add(k, n)` (+n); `Atomic.add(v)`; `Duration.add(other)`; `ops.Add.add`; `ConcurrentSet.insert` calls `_add` | YES: see B7 |
| `str` vs `bytes` vs `bytearray` | `str`: 27 methods. `bytes`: `decode`, `decode_lossy`, `len`, `reversed`. `bytearray`: `len`, `push`, `extend`, `pop`, `decode`, `copy`, `reverse`, `reversed` | YES: no `contains`/`index_of`/`split`/`starts_with`/`count`/`join`/`replace` on bytes (P bytes has all of them); no `insert`/`remove`/`clear` on `bytearray` (P has them) |
| Box family | `Shared`: get/set/update/read. `RwShared`: get/set/write/read. `Atomic`: load/store/exchange/cas/add/sub. Concurrent*: snapshot. `Task`: get/done | YES: `update` vs `write` (B10). `read` has two meanings: a mutation inside `Shared.read` persists, inside `RwShared.read` it is lost (measured: `[1, 2]` vs `[1]`) |
| `done` | `Task.done() -> bool` task.chz:62; `Token.done() -> Channel[bool]` cancel.chz:132; `Token.cancelled() -> bool` :68 | YES: same name, different return kind (G `ctx.Done()` is a channel) |
| Constructors | static `T.new()`: `ConcurrentList/Map/Set/Counter.new`, `Path.new`; module fn: `log.new`, `flag.new`; factories: `collections.min_heap/max_heap/from_list`, `cancel.manual/timeout`, `duration.millis/seconds/...`; none for `Deque`/`Counter` | YES (R `Type::new`, G `pkg.NewX`) |
| Display hook | `Range.str`, `Path.str` (Stringable hook); `Duration.to_string()` duration.chz:49; `datetime.to_string(dt)` free fn | YES: `print(duration.seconds(2))` prints `Duration(ms=2000)`, `to_string()` prints `2s` (measured) |
| `close` result | `Channel.close -> None`; `Socket.close`/`Listener.close -> None`; `Writer.close`/`Reader.close -> None!` | YES |
| `timeout_ms` param | present: `Socket.read/write/*_bytes`, `Listener.accept`, `request.get/get_bytes/post/request`; absent: `request.put/patch/delete/head` request.chz:32-35 | YES |
| Walk names | `ConcurrentList/Set.for_each/fold`; `ConcurrentMap.for_each_entry/fold_entries`; `List.fold` but no `List.for_each` | YES |
| `strip_prefix` result | returns `str` unchanged on a miss (P `removeprefix` semantics, R name) | YES vs R: `strip_prefix -> Option<&str>` |
| Named args at a call | user fns/methods: accepted (F2/F3/F6); native methods: `'sort_by_key' takes no named arguments` (F4); native fn `print(sep=)`: accepted | YES |
| `clear` | none on `List`, `Map`, `Set`, `bytearray` (probed) | Gap vs P/R/G |

## 4. Doc contradictions

| # | Doc / comment | Says | Code / measured | True |
|---|---|---|---|---|
| D1 | docs/reference/stdlib/02-methods.md:107, :124 (and :134, syntax/09-protocols.md:206) | `Map.has(key)`, `Set.has(x)` | prelude.chz:108/123 declare `contains`; `m.has("a")` is `type Map[str, int] has no method 'has'` | Code. The doc tables are stale |
| D2 | docs/reference/concurrency/03-shared.md:353 | `ConcurrentList.fold`: "no per-worker copy of the inner" | collection.chz:139-143 `fold` loops `for x in self.inner.get()`: one whole-list copy per call, so one per worker. 03-shared.md:343-347 (walk rule) says the same | Walk rule and code. Line 353 is false |
| D3 | docs/reference/stdlib/08-pure-concurrency-utils.md:49-50 | `ConcurrentMap`: "No method copies the whole map except `snapshot`" | `for_each_entry`/`fold_entries` collection.chz:85-94 copy via `inner.get()`; the same doc's row for `for_each_entry` says "copies the whole map once" | Code |
| D4 | 08-pure-concurrency-utils.md:34-35 | "the walk is not one atomic snapshot" | collection.chz:47-49 and 03-shared.md:343-345: a walk copies once and "visits exactly the entries present when it starts" | Code (it is one snapshot) |
| D5 | 08-pure-concurrency-utils.md:45 | `ConcurrentList.for_each`: "one element in the caller's heap at a time" | collection.chz:134-136 copies the whole list into the caller's heap first | Code |
| D6 | 08-pure-concurrency-utils.md:51 | "`set`/`remove`/`get_or_insert` are single-entry writers" | `ConcurrentMap` has no `set`; the writer is `insert` collection.chz:63 | Code |
| D7 | std/prelude.chz:257-261 | "The four bare variant names ... are ordinary variant imports" (`Some`, `None`, `Ok`, `Err`) | `x: int? = Some(1)` is `unknown name 'Some'`; `Ok(2)` is `unknown name 'Ok'` (measured). Only `None` resolves in an expression | Measured. Out of the method surface; flag to the owner |
| D8 | std/prelude.chz:219-220 | `bytes` comment lists `decode` and `len` | `decode_lossy` and `reversed` also exist :225/227 | Code (comment incomplete) |
| D9 | 03-shared.md:332-334 (collection table) | lists methods without `snapshot` | `snapshot` exists on List/Map/Set collection.chz:98/146/192; the same doc names it at :385 | Code (table incomplete) |

## 5. Feature checks (exact outputs)

F1: generic default on a user struct method, with `import Comparable from std.cmp` (g1.chz):

```chezzi
struct Bag[T]:
    xs: List[T]
    fn sort[K: Comparable](self, key: fn(T) -> K = fn(x: T) -> T: x, reverse: bool = false):
        self.xs.sort_by_key(key)
        if reverse:
            self.xs.reverse()
b := Bag([3, 1, 2])
b.sort()
```
```
type error (g1.chz:6:52): default value for parameter 'key': expected fn(T) -> K, found fn(T) -> T
type error (g1.chz:12:1): the default for 'key' is filled by the callee and can only be omitted from the END of a call, but 'reverse' is supplied after it — pass 'key' explicitly
chezzi: 2 type errors
```
Without the import, a third error comes first: `unknown name 'Comparable' in bound on 'K'; import it from std.cmp`.
The second error names `reverse` as supplied, but the call `b.sort()` supplies no argument. This diagnostic looks wrong.

F5: the same with a named generic fn `ident[T]` as the default: the same two errors (`expected fn(T) -> K, found fn(T) -> T`).

F3 (g3.chz): a default whose type names the struct param `T`, no method type param:
`fn sort(self, key: fn(T) -> T = fn(x: T) -> T: x, reverse: bool = false)` then `b.sort()` and `b.sort(reverse=true)`:
```
type error (g3.chz:5:9): type T does not satisfy Comparable
type error (g3.chz:10:1): the default for 'key' is filled by the callee and can only be omitted from the END of a call, but 'reverse' is supplied after it — pass 'key' explicitly
type error (g3.chz:12:1): the default for 'key' is filled by the callee and can only be omitted from the END of a call, but 'reverse' is supplied after it — pass 'key' explicitly
```

F2 (f2.chz): the non-generic form works, with keyword args: `fn sort(self, key: fn(int) -> int = fn(x: int) -> int: x, reverse: bool = false)` on `struct Bag: xs: List[int]`.
`check`: `ok: no type errors`. `run`: `b.sort()` `[1, 2, 3]`; `b.sort(reverse=true)` `[3, 2, 1]`; `b.sort(key=fn(x: int) -> int: 0 - x)` `[3, 2, 1]`; `b.sort(fn(x: int) -> int: x, true)` `[3, 2, 1]`.

G2 (g2.chz): generic key with NO default, keyword args: `fn sort[K: Comparable](self, key: fn(T) -> K, reverse: bool = false)`.
`check`: `ok: no type errors`. `b.sort(key=fn(x: int) -> int: x, reverse=true)` prints `[3, 2, 1]`; `b.sort(fn(x: int) -> int: x)` prints `[1, 2, 3]`.

G4: a non-function default that names `T` (`d: T? = None`) works: `ok: no type errors`, prints `None` then `3`.

F4: named args on native methods:
```
type error (f4.chz:2:1): 'sort_by_key' takes no named arguments; pass its arguments positionally
type error (f4.chz:2:1): 'sort_by_key' expects 1 argument(s), got 0
type error (f4.chz:4:7): 'join' takes no named arguments; pass its arguments positionally
type error (f4.chz:4:7): 'join' expects 1 argument(s), got 0
```

F6: named args on a free user fn: `total([1, 2], start=10)` prints `13`; `total([1, 2])` prints `3`.

Conclusion for B1/B2: `sort(key=..., reverse=...)` needs two features.
1. A default for a method type param, or a default value of type `fn(T) -> T` accepted for `fn(T) -> K` with `K := T`. Today the checker rejects it (F1, F5).
2. Named args on a native method (F4).
Also the F1/F3 "omitted from the END" error fires on a call with zero arguments.

Other probes: `"12".parse_int() ?? 0` prints `12`; `r.ok()` on `int!str` is `has no method 'ok'`; `Shared.read(fn(xs): xs.push(2))` then `get()` prints `[1, 2]`, the same on `RwShared` prints `[1]`; `[1, 1, 2, 1].dedup()` `[1, 2, 1]`, `.unique()` `[1, 2]`; `text.split("a,b,c", ",", 1)` `['a', 'b,c']`.

## 6. Counts

| Item | Count |
|---|---|
| Types with methods audited | 34 (List, Map, Set, str, bytes, bytearray, Channel, Range, Shared, RwShared, Atomic, AtomicInt, Executor, Task, Token, Heap, Deque, Counter, ConcurrentList/Map/Set/Counter, Duration, Path, Logger, FlagSet, Writer, Reader, Socket, Listener, Json Parser, plus FileInfo/ProcResult/Match/Response with fields only) |
| Builtin native methods (prelude + concurrency.chz, public) | 123 (List 33, Map 11, Set 8, Channel 8, str 27, bytes 4, bytearray 8, Shared 4, RwShared 4, Atomic 6, AtomicInt 6, Executor 4) |
| Kind B groups (table 1) | 15 (B1-B15), plus 4 checked and kept |
| Kind A axes (table 2) | 10, of which 6 inconsistent |
| Drift conventions (table 3) | 22, of which 21 drift |
| Doc contradictions (table 4) | 9 |
| Feature checks (section 5) | 10 programs; 2 features missing (generic default, named args on native methods); 1 wrong diagnostic |

## 7. Owner decisions (2026-10-22)

| Rule | Decision | Groups |
|---|---|---|
| Search miss | `T?` on every type: `index_of(x, start: int = 0) -> int?` on `List`, `str`, `bytes`; the `-1` sentinel goes away | B4, convention "miss" |
| Sort shape | Python: one `sort(key=, reverse=)`, one `min(key=)`, one `max(key=)`, and a `sorted()` copy twin. First build a generic default argument and named args on native methods, and fix the wrong "supplied after it" error on a zero-argument call | B1, B2, `_by` suffix |
| Free fn twins | Methods only. Move `capitalize`, `title`, `swapcase`, `rsplit`, `split_whitespace` onto `str`; delete the `string.*`, `iter.*` and `cancel.derive` twins | B5, B6, B15 |
| Box verbs | One write verb, `update`, on `Shared` and `RwShared`. `read(f)` is read-only on both: a write inside it faults (D4 layer C) | B10, box family |
