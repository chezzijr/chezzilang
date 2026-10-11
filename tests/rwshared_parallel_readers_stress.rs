//! TICKET-246 — many readers copy out of a LIVE `RwShared` heap while one writer changes it in
//! place. Sixteen reader tasks copy a nested value holding every container and struct kind
//! (`List`, `Map`, `Set`, tuple, struct, enum, `str`, `bytes`, a closure) with `get`, `read` and
//! each collection read view; the writer keeps one invariant under the exclusive lock (`a` and
//! `b` move together, and `log` holds exactly `a` items). Every reader asserts the invariant on
//! its own copy, so a copy taken without the read lock, or a reader that wrote into the heap it
//! reads, shows as a torn value.
//!
//! Each worker count runs in its own child process (DEC-230: the pool is one process-wide
//! `OnceLock`), under a timeout.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const SRC: &str = r#"import std.concurrency
import ConcurrentList, ConcurrentMap, ConcurrentSet from std.concurrency.collection

enum Tag:
    Even(int)
    Odd(str)

struct Inner:
    name: str
    raw: bytes
    pair: (int, str)

struct State:
    a: int
    b: int
    log: List[int]
    by_name: Map[str, Inner]
    seen: Set[int]
    tag: Tag
    f: fn(int) -> int
    inner: Inner

fn inc(x: int) -> int:
    return x + 1

fn fresh() -> State:
    i := Inner("n", b"xy", (0, "z"))
    return State(0, 0, [], {"n": i}, Set[int](), Tag.Even(0), inc, i)

fn step(s: State) -> State:
    s.a += 1
    s.log.push(s.a)
    s.seen.add(s.a)
    s.by_name["k" + str(s.a % 8)] = Inner("k", b"ab", (s.a, "v"))
    if s.a % 2 == 0:
        s.tag = Tag.Even(s.a)
    else:
        s.tag = Tag.Odd(str(s.a))
    s.inner.pair = (s.a, "p")
    s.b += 1
    return s

fn tag_n(t: Tag) -> int:
    match t:
        Tag.Even(n): return n
        Tag.Odd(s): return int(s)

fn torn(s: State) -> int:
    bad := 0
    if s.a != s.b:
        bad += 1
    if s.log.len() != s.a or s.seen.len() != s.a:
        bad += 1
    if tag_n(s.tag) != s.a or s.inner.pair.0 != s.a:
        bad += 1
    if s.f(1) != 2 or s.inner.raw != b"xy":
        bad += 1
    return bad

fn writer(w: RwShared[State], xs: ConcurrentList[int], m: ConcurrentMap[int, int], st: ConcurrentSet[int], rounds: int):
    for i in 0..rounds:
        w.write(step)
        xs.push(i)
        m.set(i, i)
        st.add(i)

fn some_or(o: int?, d: int) -> int:
    match o:
        ?x: return x
        None: return d

fn reader(w: RwShared[State], xs: ConcurrentList[int], m: ConcurrentMap[int, int], st: ConcurrentSet[int], bad: AtomicInt, rounds: int):
    for _ in 0..rounds:
        bad.add(torn(w.get()))
        bad.add(w.read(torn))
        n := xs.len()
        if n > 0:
            # The list only grows and item i is i, at every moment.
            if some_or(xs.at(n - 1), -1) != n - 1:
                bad.add(1)
            sl := xs.slice(0, 2)
            if sl.len() > 0 and sl[0] != 0:
                bad.add(1)
            if some_or(m.get(0), -1) != 0 or not m.contains(0) or not st.contains(0):
                bad.add(1)
        if m.len() < 0 or st.len() < 0:
            bad.add(1)

w := RwShared(fresh())
xs := ConcurrentList[int].new()
m := ConcurrentMap[int, int].new()
st := ConcurrentSet[int].new()
bad := AtomicInt(0)
parallel:
    spawn writer(w, xs, m, st, 300)
    for _ in 0..16:
        spawn reader(w, xs, m, st, bad, 40)
end := w.get()
print(bad.load(), end.a, end.b, end.log.len(), xs.len(), m.len(), st.len())
"#;

/// Run the stress program at `threads` workers (`None` = default); `Err` on a timeout.
fn run(threads: Option<&str>) -> Result<(String, String, Option<i32>), String> {
    let dir = std::env::temp_dir().join(format!(
        "chezzi-t246-stress-{}-{}",
        std::process::id(),
        threads.unwrap_or("default")
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("main.chz");
    std::fs::write(&path, SRC).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.arg("run")
        .arg(&path)
        .env_remove("CHEZZI_THREADS")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(t) = threads {
        cmd.env("CHEZZI_THREADS", t);
    }
    let mut child = cmd.spawn().expect("spawn chezzi");
    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let out_t = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = out.read_to_string(&mut s);
        s
    });
    let err_t = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = err.read_to_string(&mut s);
        s
    });
    let start = Instant::now();
    let code = loop {
        if let Some(st) = child.try_wait().expect("try_wait") {
            break st.code();
        }
        if start.elapsed() > Duration::from_secs(120) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("timeout at CHEZZI_THREADS={threads:?}"));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    Ok((out_t.join().unwrap(), err_t.join().unwrap(), code))
}

fn check(threads: Option<&str>) {
    let (out, err, code) = run(threads).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(code, Some(0), "CHEZZI_THREADS={threads:?} stderr: {err}");
    // No torn copy, and the writer's 300 steps all landed.
    assert_eq!(
        out, "0 300 300 300 300 300 300\n",
        "CHEZZI_THREADS={threads:?} stderr: {err}"
    );
}

#[test]
fn sixteen_readers_see_no_torn_value_at_one_worker() {
    check(Some("1"));
}

#[test]
fn sixteen_readers_see_no_torn_value_at_two_workers() {
    check(Some("2"));
}

#[test]
fn sixteen_readers_see_no_torn_value_at_the_default_worker_count() {
    check(None);
}
