### `std.request`
`get(url, timeout_ms?: int) -> Response!` · `get_bytes(url, timeout_ms?: int) -> bytes!` ·
`post(url, body, timeout_ms?: int) -> Response!` · `put(url, body)` · `patch(url, body)` ·
`delete(url)` · `head(url)` ·
`request(method, url, body, headers: Map[str, str], timeout_ms?: int) -> Response!` (method in UPPERCASE).

Results use `struct Response { status: int, body: str, headers: Map[str, str] }` (header names
lowercased). A ≥400 status is **not** an error: the code is in `Response.status`; only
transport/DNS/TLS failures become `!e`. Blocking (offloaded) and
[uninterruptible while in flight](04-native-core.md#blocking-calls-cannot-be-interrupted).

- **Timeout.** The optional trailing `timeout_ms` sets a **per-request total deadline** that overrides
  the default caps (connect 10s / read 30s / write 30s) for that one call; `timeout_ms <= 0` or omitted
  uses the defaults. A timeout (like any transport failure) is a recoverable `!e`, never a panic.
- **Query strings.** Build one with `std.encoding.query_encode` and compose
  `url + "?" + query_encode(params)`.
- **Response headers.** A header sent more than once is **joined with `, `** (Python `requests`;
  `Set-Cookie: a=1` + `Set-Cookie: b=2` → `a=1, b=2`). Each value is decoded **latin-1** (byte → code
  point, never fails), so every header is present; a UTF-8 `café` reads back `cafÃ©`, as CPython
  `urllib` does.
- **Request headers.** Custom REQUEST header names go on the wire **lowercased** (RFC 9110 field names
  are case-insensitive).
- **Redirects** follow Go's rules (`net/http` `Client.Do`), one hop at a time:
  - A `301`/`302`/`303` turns any method but `GET`/`HEAD` into a `GET` without a body, and drops
    `Content-Type`, `Content-Encoding`, `Content-Language` and `Content-Location` for every later hop.
    A `307`/`308` keeps the method and the body. A redirect status without `Location` is the response
    itself.
  - Every hop carries the caller's headers, except that `Authorization`, `Cookie`, `Cookie2`,
    `Proxy-Authorization`, `WWW-Authenticate` and `Proxy-Authenticate` are dropped for every hop after
    one whose host is not the first URL's host or a subdomain of it.
  - Each hop sends `Referer` naming the previous hop's URL (none from `https` to `http`; a caller's own
    `Referer` wins).
  - Up to **ten** redirects are followed; the eleventh is `!"<url>: too many redirects"` (CPython and
    Go both stop near ten).
- **Errors.** An `!e` message starts with the URL. `get_bytes`' non-2xx `!e` names the canonical reason
  (`HTTP 404 Not Found`), not the server's wire phrase.
- **Lenient and strict parsing.** A response with an `HTTP/1.2` status line or an obs-fold continuation
  is `?v` (the folded value joins with one space, Go's rule). A control byte or NUL in a header value is
  an `!e` whose message names the line (`malformed MIME header line: "X-Bad: a\x01b"`), as in Go;
  CPython accepts it.
- **Proxies.** `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY` and the lowercase twins are **honoured**, except
  that a loopback target (`127.0.0.0/8`, `::1`, `localhost`) goes direct, as in Go.
- **Retry on a reused keep-alive connection, by Go's rule** (`net/http` `shouldRetryRequest`). A server
  may close an idle keep-alive connection as the next request goes out (RFC 9112 allows it). When a
  request fails on a pooled connection before any response byte arrives, and the request is
  replayable, it runs once more on fresh connections. Replayable is Go's `isReplayable`: `GET`, `HEAD`,
  `OPTIONS`, `TRACE`, or any method with an `Idempotency-Key` or `X-Idempotency-Key` header. `PUT`,
  `DELETE`, `PATCH` and `POST` without that header are not retried, and neither is a failure on a fresh
  connection or after a response byte. A `timeout_ms` covers both attempts. Each redirect hop decides
  its own retry, and only the failed hop is re-sent: `POST /r` answered `303` to `/a`, then `GET /a`
  dropped on the reused connection, re-sends `GET /a` alone. The `!e` message keeps the first URL as its
  prefix.
- **Binary download:** `get_bytes` fetches the body as raw `bytes` (byte-exact, no UTF-8 decode), so an
  image/zip/pdf survives where `get`'s `Response.body: str` would mangle it. It is GET-only and
  body-only: a non-2xx status is an `!e` (a 404/500 error page cannot pose as a successful download),
  and headers are dropped. It caps a download at 64MB (a larger body is an `!e`).

**Module-owned struct types.** `Match`, `Response` and `ProcResult` belong to `std.regex`,
`std.request` and `std.process`; they are **not** reserved program-global names. Field access on a
returned value (`.text`/`.status`/`.code`, …) works with **no import**. Naming or constructing the type
(`m: Match` / `Match(...)`) requires importing the owning module (whole-module `import std.regex`
exposes `Match` bare and as `regex.Match(...)`; or `import Match from std.regex`). A user
`struct Response` without `import std.request` is the user's own type. Importing the type **and**
declaring a same-named `struct` in the same module is rejected at check (`type 'Response' is already
defined`).

### `std.net`
Non-blocking TCP (scheduler-aware). `connect(addr: "host:port") -> Socket!` ·
`listen(addr: "host:port") -> Listener!`. **Both return `T!E`** (bind/DNS/refused failures are the
`!e`); match or `?` them. Socket/Listener methods are in §3. See `concurrency.md`.
The `Socket`/`Listener` TYPE names require `import std.net` to use bare in an annotation (whole-module,
or `import Socket from std.net`); they are not global builtins.

**Text and binary:** `Socket.read -> str!` decodes UTF-8 and never lossily (see §3): a split codepoint
is carried across reads and reassembled exactly, while a **binary** payload is a clear `!e`. For
binary, use `Socket.read_bytes` / `write_bytes` (§3).

**`connect` inside an `Executor` job** returns the error `connect would block: an Executor job doesn't
own its thread — blocking here would starve every other job and \`parallel:\` nursery sharing the pool.
Do this socket op inside \`spawn:\` or a \`parallel:\` nursery instead, where it parks rather than
blocking a shared thread.` A job runs on the bounded, process-wide pool with no scheduler under it,
so blocking there takes width from every other job and every `parallel:` nursery.
**Everywhere else `connect` blocks and succeeds**: a `spawn`/`parallel:` fiber parks on the
netpoller, and top-level `main` blocks its thread (as CPython `socket.connect` and Go `net.Dial` do from
the main thread). `connect` is allowed where `accept`/`read`/`read_bytes`/`write` are not, because those
wait on a **Chezzi peer fiber** that can run only on the thread they would block, while the **kernel**
completes a `connect` handshake. The wait honours `--timeout`, cancellation and `os.exit`.

### `std.ffi`
C-ABI vocabulary for `extern "lib":` blocks (see the FFI section of `syntax.md`).
`null() -> ptr` · `is_null(p: ptr) -> bool`. It also exports the marshalling **type names**: the opaque
pointer handle `ptr` plus nine width names: the eight fixed-width integers `int8`, `int16`, `int32`,
`int64`, `uint8`, `uint16`, `uint32`, `uint64` (each resolves to `int`), and `float32`, a C `float`
(it resolves to `float`; a param rounds to the nearest C `float`, a return widens exactly):

```chezzi
import float32 from std.ffi
```

None of these are global builtins: a module that uses `ptr`, `owned_str` or a width type imports it
from `std.ffi` (`import int32 from std.ffi`, or `ffi.int32` after `import std.ffi`). An alias marshals
as the type it names (`import int8 as Byte from std.ffi`). A width type is legal only in an **`extern`
signature or a struct field's own type**; `owned_str` only as an extern return; `ptr` has no such
limit. (Sync scalar **callbacks** need no `std.ffi` surface: a callback extern param is a
function-typed param spelled `fn(scalars) -> scalar`; see the FFI section of `syntax.md`.)

**Memory deref (`load_*` / `store_*`).** Read/write the C-owned memory *behind* an opaque `ptr`: struct
fields, return buffers, event payloads, and C output-params. Each has a base form (byte offset `0`) and
an `_at(p, off)` form that takes a **byte** offset (the `_at` store puts the offset *before* the value).
Loads:

- `load_int(p)` / `load_int_at(p, off) -> int`: C `long`.
- `load_int8`/`load_int16`/`load_int32`/`load_int64` (+ `_at`) `-> int`: sign-extended to `int`.
- `load_uint8`/`load_uint16`/`load_uint32`/`load_uint64` (+ `_at`) `-> int`: zero-extended (a `uint64`
  with the top bit set wraps negative, a known limit).
- `load_float(p)` / `_at -> float` (C `double`); `load_float32(p)` / `_at -> float` (C `float`, widened).
- `load_bool(p)` / `_at -> bool` (1 byte, nonzero = true).
- `load_ptr(p)` / `_at -> ptr`: deref a `void**` (the result may itself be NULL).
- `load_str(p)` / `_at -> str`: copy a NUL-terminated C string (the buffer is **not** freed; the
  precondition is a well-formed, NUL-terminated string, with no max-length cap). The bytes are
  **validated**: a non-UTF-8 buffer is a clean **fault** naming the bad offset, never a mangled string
  (same contract as `Socket.read`). Read raw/binary bytes with `load_uint8_at`. The `str`/`owned_str`/
  `str?` **extern return** paths validate alike.

Stores mirror every width except `str` (there is no `store_str`: an unbounded write into a caller
buffer is unsafe). Each returns `None`: `store_int`/`store_int8`..`store_int64`/`store_uint8`..
`store_uint64`/`store_float`/`store_float32`/`store_bool`/`store_ptr`, base form `(p, v)`, `_at` form
`(p, off, v)`. Stores write at the value's **natural C width** (`store_int8` writes one byte only). A
width store's value must fit the width: a constant outside it is a compile error
(`constant 300 does not fit int8 (-128..127)`), a runtime value a recoverable fault
(`value 300 does not fit int8 (-128..127)`); `float32` passes NaN and inf. Wrap first with
`ffi.cast_<w>`.

**Explicit wrap and range constants.** `cast_int8(v)` … `cast_uint64(v)` wrap an `int` like a C cast
(Go's `int8(v)`: `cast_int8(300) == 44`); `cast_float32(v)` rounds to the nearest C `float`. The
constants `INT8_MIN`/`INT8_MAX`, `INT16_MIN`/`INT16_MAX`, `INT32_MIN`/`INT32_MAX`,
`INT64_MIN`/`INT64_MAX`, `UINT8_MAX`, `UINT16_MAX`, `UINT32_MAX`, `UINT64_MAX` (`-1`, C's value as its
i64 bit pattern) and `FLT_MAX` are declared in `std/ffi.chz`, with the `native type` decls of every
exported type name.

> **Unsafe surface.** `load_*`/`store_*` read/write *arbitrary* memory through a C-sourced address:
> like Python `ctypes` (`POINTER(c_int)` + `a[0]`), a bad pointer **segfaults**. One mitigation that
> ctypes lacks: a `ptr` is **opaque** and cannot be forged from an `int` (only an `extern` return, a
> callback arg, or `ffi.null()` yields one). The only cheap check is the **NULL** base pointer: a
> `load_*`/`store_*` on address `0` returns a *recoverable* error (`ffi.<fn>: null pointer`). A
> dangling, misaligned, or out-of-bounds *non-null* pointer is undefined behavior (not detectable).
> These builtins are **unix-only** (a non-unix build registers the names but every call errors).

**C-buffer alloc layer (`alloc` / `alloc_zeroed` / `free`).** Allocate raw C-laid-out memory to hand to
a C array/buffer API (`qsort`, `bsearch`, `fread`-into-buffer, …). Fill and read it with the
`store_*`/`load_*` builtins. The **libc allocator** (`malloc`/`calloc`/`free`) backs it, so a C fn may
itself realloc/free the buffer.

- `alloc(nbytes) -> ptr`: `malloc(nbytes)`; the bytes are **garbage** (uninitialized).
- `alloc_zeroed(nbytes) -> ptr`: `calloc`-style; the bytes are **zeroed**.
- `free(p)`: release a buffer; returns `None`. `free(ffi.null())` is a safe **no-op**. Freeing the same
  buffer **twice is undefined behavior and aborts the process** (`free(): double free detected in
  tcache 2`, SIGABRT); `recover:` cannot catch it. CPython `ctypes` aborts the same way. Do not reuse a
  `ptr` after `free`.

> **Manual free.** A `ptr` is **never auto-freed**. The idiom is `p := ffi.alloc(n)` then
> `defer ffi.free(p)`. **Forgetting to free is a leak.** `nbytes < 0` is a recoverable error
> (`ffi.alloc: negative size`); `malloc`/`calloc` returning NULL for `nbytes > 0` is a recoverable
> `ffi.alloc: out of memory`. `nbytes == 0` passes through to `malloc(0)` (impl-defined: NULL or a
> unique ptr). **Double-free, use-after-free, or `store_*`/`load_*` beyond the allocation are undefined
> behavior**, as in ctypes: there is no bounds or lifetime tracking. Unix-only.
>
> **The contract covers stream output too.** A C function that writes stdout writes the file descriptor
> directly, so its bytes do not interleave with `print` and the broken-pipe halt cannot see them: a
> `chezzi run x.chz | head -1` loop of C writes never faults. `ctypes` and `cgo` do the same; the C
> function's own return value is your only error channel. See [`syntax.md` §12b](../syntax/01-lexical-and-literals.md).

Sort a Chezzi list with libc `qsort` (alloc + `store_*` + a callback comparator + `load_*`):

```chezzi
import std.ffi

extern "libc":
    fn qsort(base: ffi.ptr, n: int, size: int, cmp: fn(ffi.ptr, ffi.ptr) -> int)

fn cmp(a: ffi.ptr, b: ffi.ptr) -> int:           # qsort hands two const void* (each an int64 slot)
    x := ffi.load_int64(a)
    y := ffi.load_int64(b)
    if x < y: return -1
    if x > y: return 1
    return 0

data := [5, 2, 9, 1, 7]
buf := ffi.alloc(data.len() * 8)         # one int64 slot per element
defer ffi.free(buf)                      # manual free — never auto-freed
for i in 0..data.len():
    ffi.store_int64_at(buf, i * 8, data[i])
qsort(buf, data.len(), 8, cmp)           # sorts in place, calling back into `cmp`
for i in 0..data.len():
    print(ffi.load_int64_at(buf, i * 8)) # 1 2 5 7 9
```

### `std.encoding`
Reversible text codecs. Every function takes a `str` and works on its **UTF-8 bytes** (like
`s.encode()`). Encoders return `str` (infallible); decoders return `str!` (malformed input, or decoded
bytes that are not valid UTF-8, is a recoverable `!e`, never a panic). All members are pure CPU
transforms (no I/O) and run inline.

- **base64** (RFC 4648): `base64_encode(s) -> str` / `base64_decode(s) -> str!` (std `+/` alphabet,
  `=` padding) · `base64_encode_url(s) -> str` / `base64_decode_url(s) -> str!` (URL-safe `-_`
  alphabet). The std decoder rejects `-_`; the URL decoder rejects `+/`. **Every decoder ignores `\r` and
  `\n` anywhere** in the input (Go's `DecodeString`; CPython `b64decode` too), so a PEM/MIME-wrapped body
  decodes; any other non-alphabet byte, space and tab included, is an `!e`.
- **base64 of raw bytes**: `base64_encode_bytes(b: bytes) -> str` · `base64_decode_bytes(s: str) -> bytes!`
  (std alphabet). These do not UTF-8-validate, so **arbitrary binary round-trips** (an image, a gzip
  body). There are no URL-safe or hex bytes twins.
- **hex**: `hex_encode(s) -> str` (lowercase) · `hex_decode(s) -> str!` (rejects odd length / non-hex
  digits).
- **URL percent-encoding** (RFC 3986 **component** form): `url_encode(s) -> str` keeps the unreserved
  set `A-Za-z0-9-._~` literal and `%XX`-escapes everything else (uppercase hex) ·
  `url_decode(s) -> str!` reverses it. **Strict 3986**: `+` is *not* a space (that is
  `application/x-www-form-urlencoded`, a different scheme).
- **Query string builder**: `query_encode(params: Map[str, str]) -> str` assembles a `k=v&k2=v2` query
  string; key and value are both percent-encoded with `url_encode`. Keys are **sorted by their RAW
  (pre-encoding) value**, so the output is deterministic. An empty map yields `""` (no leading `?`);
  `{"k": ""}` yields `"k="`. Compose `url + "?" + query_encode(params)`.
- **Query parser**: `query_decode(q: str) -> Map[str, str]` reverses `query_encode` for single-valued
  keys. It strips one leading `?`; splits on `&` (empty segments skipped); each segment splits on the
  FIRST `=` (a no-`=` segment maps its key to `""`); key and value are percent-decoded with `+` → space
  (the `x-www-form-urlencoded` rule, looser than `url_decode`, which keeps `+`). DUPLICATE keys are
  **last-wins** (a `Map[str,str]` cannot hold Python `parse_qs` value lists; Go `url.Values.Get`
  analog). A malformed `%`-escape (or non-UTF-8 result) keeps the field's RAW substring; never a fault.
- **URL splitter**: `url_parse(u: str) -> Map[str, str]` LEXICALLY splits a URL into the keys `scheme`,
  `host`, `port`, `path`, `query`, `fragment` (missing components → `""`). Never faults.
  - It does **not** percent-decode the components (as Python `urlsplit` / Go `net/url`): call
    `url_decode` / `query_decode` on the pieces you need.
  - `port` is a **string** (`""` when absent; Go `url.Port()` analog).
  - A **protocol-relative** URL (`//host/path`, no scheme) also splits its authority:
    `url_parse("//h/p")` gives `host="h"`, `path="/p"`; `url_parse("//h:8080/p")` gives `host="h"`,
    `port="8080"`, `path="/p"` (as Go 1.26.6 `url.Parse` and CPython 3.14.7 `urlsplit`).
  - Userinfo (`user:pass@host`) is **dropped**, split on the LAST `@`:
    `url_parse("http://example.com:pw@evil.com/x")` gives `host="evil.com"`, as in Go/CPython. It never
    goes into `host`/`port` (a `host` allowlist must not see attacker-controlled userinfo).
  - IPv6 brackets stay in `host` (Go's `URL.Host` minus the port, not CPython's `.hostname`); only a
    `:port` after the closing `]` splits off: `url_parse("http://[::1]:8080/a")` gives `host="[::1]"`,
    `port="8080"`.
  - The scheme separator (`://`) must be ANCHORED at position 0 (an RFC-3986 scheme is
    `[A-Za-z][A-Za-z0-9+.-]*`), so a later `://` is path text: `url_parse("/p/a://b")` gives
    `path="/p/a://b"`.
  - The scheme is lowercased (`url_parse("HTTPS://EXAMPLE.COM/x")` gives `scheme="https"`); `host`
    keeps its case.
  - Limits: a `//`-less scheme (`mailto:x`) puts the remainder in `path`. `url_parse("///p")` follows
    CPython (`host=""`, `path="/p"`), not Go, which keeps `///p` whole in `path`.

The `str` members UTF-8-validate their decoded output, so a non-UTF-8 result is an `!e`. Arbitrary
binary round-trips through `base64_encode_bytes`/`base64_decode_bytes`. There is no gzip/zlib.

### `std.crypto`
Hand-written digests + HMAC (zero dependencies). Each `str`-taking fn hashes the str's UTF-8 bytes and
returns the lowercase-hex digest as a `str` (infallible, no `T!E`); the `_bytes` twins hash raw `bytes`
(e.g. `io.read_bytes(p)` → hash a file).
`sha256(s) -> str` / `sha256_bytes(b: bytes) -> str` (FIPS 180-4) ·
`sha1(s) -> str` / `sha1_bytes(b: bytes) -> str` (FIPS 180-4) ·
`sha512(s) -> str` / `sha512_bytes(b: bytes) -> str` (FIPS 180-4) ·
`md5(s) -> str` (RFC 1321) ·
`hmac_sha256(key: bytes, msg: bytes) -> str` (HMAC-SHA-256, RFC 2104, keyed message authentication;
pass a `str` key/msg as a `b"..."` literal).

**CSPRNG** (Python `secrets`): `secure_bytes(n: int) -> bytes` returns `n` cryptographically-secure
random bytes; `token_hex(n: int) -> str` returns `n` secure random bytes as a `2n`-char lowercase-hex
`str`. Both draw from the OS entropy source (`getrandom(2)`, else `/dev/urandom`, which must be a
character device, so a planted regular file is refused) and **fail closed**: if the OS cannot supply
entropy they raise a **recoverable fault**, never weak bytes. `n` must be `0..=1048576` (a 1 MiB cap);
a negative or oversized `n` faults. Output is **non-deterministic**. There is no `token_urlsafe`.

**Platform:** entropy is portable across **unix**: Linux `getrandom(2)`, falling back to `/dev/urandom`
(the same kernel CSPRNG) on macOS, the BSDs, Android, iOS, and on Linux if `getrandom` is unavailable.
On a **non-unix** target (Windows, unsupported) both fns **fail closed** with a recoverable fault whose
message ends `no secure entropy source on this platform`.

**Security:** MD5 **and SHA-1** are **cryptographically broken**: use them only for checksums / git
object ids / legacy interop, never for passwords, signatures, or integrity against an adversary. There
is no password hashing (bcrypt/argon2). All members are pure CPU or a fast entropy syscall (no blocking
I/O) and run inline.

### `std.uuid`
RFC 4122 version-4 (random) UUIDs. `v4() -> str` returns a fresh random UUID as the canonical 36-char
`8-4-4-4-12` lowercase-hex string (version nibble `4`, variant in `8/9/a/b`).
`uuid_seed(n: int) -> None` switches the stream to a reproducible one (for golden runs).

**By default `v4()` is secure:** it draws 16 fresh bytes from the OS CSPRNG per call (the source of
`crypto.secure_bytes`, and the shape of CPython's `uuid.uuid4()`) with no PRNG state, so an id is
unpredictable, one leaked id reveals nothing about another, and a `v4()` draw never changes a program's
`rand` sequence. It **fails closed** like `crypto` (a recoverable fault) if the OS cannot supply
entropy; see `std.crypto` for the platform matrix. It runs inline (a fast entropy syscall).

**`uuid_seed(n)` opts OUT of that, permanently**: it switches `v4()` to a reproducible 64-bit SplitMix64
stream, and the switch is **process-global and sticky**: every later `v4()` in the process is seeded.
Use it only for reproducible/golden runs, e.g. `examples/uuid_shape.chz`.

**Security (seeded stream only):** the seeded `v4()` is **not cryptographically secure**. `v4` overwrites
only **6** of the 128 drawn bits (4 version + 2 variant), so **ONE observed UUID** recovers the
SplitMix64 state and the seed, and every later *and* earlier UUID becomes predictable byte-exactly.
Never call `uuid_seed` in a program whose ids are tokens or session ids.

**Limit (as `std.rand`):** the *seeded* stream is one process-global, so *concurrent* `v4()` draws
interleave nondeterministically; an EXACT seeded value is reproducible only for *sequential* draws.
(Unseeded draws are independent.)
