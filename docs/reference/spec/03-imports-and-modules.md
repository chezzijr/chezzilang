## Imports & module resolution

Grammar: dot paths, `import..from`, alias at both levels, **no** `from..import`:

```chezzi
import std.io                       # whole module → io.read()
import std.io as fs                 # module alias → fs.read()
import read, write from std.io      # named (no braces — indentation lang)
import read as r from std.io        # named + alias
import math from std                # a std module by name, like `import std.math`
```

**Bound name and full path.** An un-aliased `import a.b` binds the last segment `b` (Go reading) AND
the full path `a.b` (Python reading), so `b.X` and `a.b.X` name one module, which initializes once.

1. **No implicit children or parents.** `a.b.X` resolves only after `import a.b` in this file.
   `import a` never binds `a.b`, and a bare `import std` is an error (`std` is a namespace).
2. **Longest imported prefix wins**, statically at check time: `a.b.c.X` is module `a.b.c` when
   both `a.b` and `a.b.c` are imported. Packages, types and methods never become runtime values.
3. **`as` binds only the alias:** `import a.b as c` does not bind `a.b`.
4. **Shadowing:** a local, parameter, type parameter or module-level name spelled like the head
   hides the full path.
5. **Same last segment:** `import a.math` plus `import b.math` keeps both full paths; a bare
   `math` use is an ambiguity error that suggests `import a.math as am`.
6. **`import X from std`** binds std's module `X` (Python `from os import path`); it does not bind
   `std.X`. `import std.math` plus `import math from std` bind `math` once. `import X from` a user
   package directory is not supported: write `import pkg.X`.

Struct patterns stay two-level (`deep.Point(x)`, never `pkg.deep.Point(x)`).

Resolution uses an **optional root marker**, which removes Python's run-relative trap:

1. Pick the run's **origin**. For `chezzi run <file>` it is that file. For a bare `chezzi run` (no file
   → the manifest entrypoint) it is the *cwd*, the directory you launched from.
2. Walk *up* from the origin for the **nearest** `chezzi.toml`. Found → that dir is root. **Not found
   → the script's own dir is root** (`run <file>`) / an error (bare `run` needs a manifest).
3. `std.*` is reserved and resolves to the **stdlib**. Its source comes from `$CHEZZI_STD` if that env
   var is set (a dev override: "use *this* tree", exclusive; a module missing from it is an error, never
   a fall-back), else from the **stdlib baked into the binary** (`std/*.chz` is `include_str!`'d, and parsed into an encoded AST, at
   compile time, like the `docs/*.md` topics). An installed `chezzi` is therefore self-contained: it
   needs no source checkout. A **pre-built** binary plus an edited `std/*.chz` is stale until you
   rebuild it (`cargo run`/`cargo test` rebuild automatically; otherwise use `$CHEZZI_STD`).
4. `a.b.c` → `<root>/a/b/c.chz`. **No `./` relative imports.**

Single-file scripts need zero config (the Deno/Bun/Go model); `chezzi.toml` matters only once a project
spans multiple files.

**Malformed graphs fail with a clean diagnostic, never a host crash.** Import **cycles** (`A↔B`,
self-import) are a clean error (Go-style). A transitive import chain deeper than **256** modules is
rejected with `import chain too deep (exceeds 256)` at the offending import. Diamond re-imports dedupe
and do **not** count toward depth.

**One root governs the whole graph.** A run computes its module-graph root **once** (the nearest
`chezzi.toml` above the origin, steps 1–2), and that root resolves **every** import in the program
(step 4), whatever module the `import` is in. The root that locates the entry file also resolves its
imports.

- `chezzi run <file>` → origin is the file; root = nearest `chezzi.toml` **above the file**.
- bare `chezzi run` (manifest entrypoint) → origin is the **cwd**; root = nearest `chezzi.toml`
  **above the cwd** (the manifest that declares the `[project] entrypoint`). The entry file and its
  imports resolve against *that* manifest's dir, so a bare run gives the same result from any
  subdirectory that shares the same nearest manifest.

**Nearest marker wins (sub-packages).** A **nested** `chezzi.toml` in a subdirectory is a real
boundary: it becomes the root for any file **run from beneath it**, like Go's `go.mod`, Cargo's
`Cargo.toml` and npm's `package.json`. So `chezzi run vendor/lib/tool.chz` treats `vendor/lib/` as its
root (if it holds a `chezzi.toml`), and its `import util` resolves to `vendor/lib/util.chz`. A bare
`chezzi run` from the *outer* project is one run rooted at the outer manifest: it does **not** consult
a nested marker. When you vendor code, a subtree is a sub-package only for runs that start inside it.

**Types are module-scoped (Python-style).** A `struct` / `enum` / `type` alias belongs to its declaring
module. Every top-level type is exported by default (like functions; no `pub`), and other modules reach
it **only via import**, by the same bound last-segment name a function uses:

```chezzi
import core.geo                      # binds `geo`
p: geo.Point = geo.Point(1, 2)       # qualified construction + annotation
c := geo.Color.Red                   # qualified enum variant
xs: List[geo.Point] = []             # qualified type inside a generic

import Point from core.geo           # named import → bare use
q := Point(3, 4)                     # bare construction
import Point as Pt from core.geo     # rename on import (user types only)
```

- A bare use of a type whose module was imported whole (`import geo`) but not named-imported is a
  **check-time error** (`unknown type 'Point'; import it from geo`).
- Two modules MAY declare the same type name with no collision.
- This gate applies only to *naming/constructing* the type. **Reading a field or calling a method on a
  VALUE of that type always works, import-free**: member resolution uses the value's own module-scoped
  identity. So after `import make from geo`, `w := make()` still resolves `w.x` / `w.bump()`, although
  bare `Point` is an unknown type (the module-owned `Match`/`Response`/`ProcResult` rule is the same).

**Identity is separate from display.** Every user `struct` / `enum` / variant / `type` alias has ONE
canonical **identity key**, always module-qualified: `<module-key>::Name` (the module key is the
declaring module's dotted path, or the entry file's stem).
- The key is the runtime type tag and the key into every layout table (construction, field/method
  resolution, `match`-pattern variant ids, `json.decode` targets, the task wire/snapshot format). The
  checker, compiler, and VM derive it identically. Two modules' `Point` are `a::Point` and `b::Point`,
  with no collision special case.
- The **display name** is the bare `Name`. All user-facing output (print/`str`, error messages,
  `json.decode` errors, `repr`) renders the bare name, so two colliding `Point`s **both** print
  `Point(...)`. JSON *encode* also emits bare names.
- One compile-time exception: a type diagnostic that names two DIFFERENT types with the same bare name
  qualifies both by module (`expected a.Col, found b.Col`; the full dotted path when the last segments
  also match, and bare again if that is still ambiguous). Runtime output never changes.
- Reserved/native types (`T!E`/`T?`/…, `Iterator`, the std library type surface on `import std.*`, and
  the FFI width names like `int32`) are **not** module-keyed; they keep their bare name globally.
- An imported `type` alias is **transparent**: its body resolves in the *defining* module's scope. An
  alias body never holds an FFI width type: `type Len = int32` is a compile error (a width describes
  the C boundary only).

`chezzi init [dir]` scaffolds a new project (`chezzi.toml` + `src/main.chz` + an example `*_test.chz`).
The generated `chezzi.toml` is **both a root marker and a parsed manifest**: the resolver checks for its
*presence* to fix the root, and the toolchain parses its `[project]` keys.
- `name`/`version` are metadata.
- **`entrypoint`** (a dotted module path + optional `:function`, scaffolded as `"src.main:main"`) is what
  a bare `chezzi run` executes; the `:main` suffix calls `main`, so the scaffolded `src/main.chz` needs no
  trailing call.
- The parser is a small fixed-schema reader (`[section]` headers, `key = "value"` string pairs, `#`
  comments). It ignores unknown keys/sections, and an empty `chezzi.toml` is a valid root marker (all
  fields unset; `entrypoint` is required only for the no-file `chezzi run`).
