//! TICKET-287 (E1): a task whose `parallel:` body faults while a child is parked must report the
//! fault, not run an unrelated spinning sibling on the joiner's own stack forever.

#[path = "support/hang_deadline.rs"]
mod hang_deadline;

const PROGRAM: &str = "dead: Channel[int] = Channel()\nfn owner():\n    parallel:\n        spawn:\n            print(dead.recv())\n        assert false, \"inner boom\"\nparallel:\n    spawn:\n        owner()\n    spawn:\n        i := 0\n        while true:\n            i = i + 1\nprint(\"end\")\n";

#[test]
fn owner_fault_with_parked_child_is_reported_beside_a_spinner_at_one_worker() {
    let dir = std::env::temp_dir().join(format!("chz-t287-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("e1.chz");
    std::fs::write(&path, PROGRAM).expect("write program");
    let out = hang_deadline::run_with_hang_deadline(&path, Some("1"));
    let _ = std::fs::remove_dir_all(&dir);
    let out = out.expect("owner fault never reported: no exit within the hang deadline");
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        all.contains("inner boom"),
        "expected `inner boom` reported: {all:?}"
    );
    assert_eq!(out.status.code(), Some(1), "output: {all:?}");
}

/// The grid's axes. `OWNERS`: who runs the `owner()` call (a spawned task, main's own body, an
/// Executor job, a task spawned by a parent whose body spins: the DEC-103 ancestor shape, a task
/// that calls `owner()` from a `map` callback: an owner under a native frame).
/// `ESCAPES`: how the family's wait starts. `fault`: an uncaught fault in the `parallel:` body.
/// `caught`: a fault a `recover:` catches, then a second `parallel:` with two tasks. `question`: a
/// `?` to a same-frame `recover:`. `cancelled`: a task whose nursery waits is cancelled by its
/// parent's fault. `child_fault`: a child's fault delivered under a `recover:` inside the body.
/// `return_defer` and `question_defer`: a `return` or a recover-scoped `?` whose body `defer`
/// faults (the `defer` fault must reach the `recover:`). Every form ends with `inner boom`.
/// `CHILDREN`: what the cancelled child waits in. `NEIGHBOURS`: the
/// fibers outside the family: none; a spinner (re-run as the kept fiber); a ping-pong pair and a
/// nursery churn loop (picked while other work is queued); a task blocked IN PLACE in a callback,
/// on a dead channel or in a 60 s sleep (no fiber boundary ever comes back); a task that sleeps 150 ms
/// and then blocks in place (it wakes while the owner waits for an outer level). `DEPTHS`: how many
/// nested `parallel:` levels the `NESTED` escapes open (TICKET-294); the other escapes run at depth 1.
const OWNERS: [&str; 5] = ["task", "main", "job", "ancestor", "callback"];
const ESCAPES: [&str; 7] = [
    "fault",
    "caught",
    "question",
    "cancelled",
    "child_fault",
    "return_defer",
    "question_defer",
];
const CHILDREN: [(&str, &str); 3] = [
    ("recv", "print(dead.recv())"),
    ("sleep", "time.sleep_ms(60000)"),
    ("spin", "j := 0\nwhile true:\n    j = j + 1"),
];
const NEIGHBOURS: [(&str, &[&str]); 7] = [
    ("none", &[]),
    ("spinner", &["spin"]),
    ("pingpong", &["ping", "pong"]),
    ("churn", &["churn"]),
    ("inplace_recv", &["inplace_recv"]),
    ("inplace_sleep", &["inplace_sleep"]),
    ("late_inplace", &["late_inplace"]),
];
const THREADS: [Option<&str>; 3] = [Some("1"), Some("2"), None];
const NESTED: [&str; 3] = ["caught", "question", "cancelled"];
const DEPTHS: [usize; 3] = [1, 2, 3];

const PRELUDE: &str = "import std.time\nimport Executor from std.concurrency\ndead: Channel[int] = Channel()\ndead2: Channel[int] = Channel()\na: Channel[int] = Channel()\nb: Channel[int] = Channel()\ngo: Channel[int] = Channel(1)\nfn spin():\n    i := 0\n    while true:\n        i = i + 1\nfn ping():\n    while true:\n        a.send(1)\n        _ := b.recv()\nfn pong():\n    while true:\n        _ := a.recv()\n        b.send(1)\nfn churn():\n    while true:\n        parallel:\n            spawn:\n                pass\nfn on_dead(x: int) -> int:\n    v := dead2.recv() else: panic(\"dead2 closed\")\n    return x + v\nfn on_sleep(x: int) -> int:\n    time.sleep_ms(60000)\n    return x\nfn inplace_recv():\n    print([0].map(on_dead))\nfn inplace_sleep():\n    print([0].map(on_sleep))\nfn late_inplace():\n    time.sleep_ms(150)\n    print([0].map(on_dead))\nfn bad() -> int!str:\n    return !\"q boom\"\nfn boom():\n    assert false, \"defer boom\"\nfn via_cb(x: int) -> int:\n    owner()\n    return x\nfn cb_owner():\n    print([0].map(via_cb))\nfn parent():\n    parallel:\n        spawn:\n            owner()\n        i := 0\n        while true:\n            i = i + 1\n";

/// `depth` nested `parallel:` levels at column `indent`, each with one child that waits in `wait`.
/// Past depth 1 each child sleeps in a `defer` (50 ms at the innermost level, 600 ms above it), so
/// an outer level's family is still open after the inner level is done (TICKET-294).
fn levels(indent: usize, depth: usize, wait: &str) -> String {
    let mut src = String::new();
    for level in 0..depth {
        let ind = " ".repeat(indent + 4 * level);
        src.push_str(&format!("{ind}parallel:\n{ind}    spawn:\n"));
        if depth > 1 {
            let ms = if level + 1 == depth { 50 } else { 600 };
            src.push_str(&format!("{ind}        defer time.sleep_ms({ms})\n"));
        }
        for line in wait.lines() {
            src.push_str(&format!("{ind}        {line}\n"));
        }
    }
    src
}

/// `owner()`: the family's wait starts by `escape` while its child waits in `wait`, under `depth`
/// nested levels.
fn owner_fn(escape: &str, wait: &str, depth: usize) -> String {
    let top = levels(4, depth, wait);
    let inner = levels(8, depth, wait);
    // The body of the innermost level under `top` and under `inner`.
    let tb = " ".repeat(4 + 4 * depth);
    let ib = " ".repeat(8 + 4 * depth);
    match escape {
        "fault" => format!("fn owner():\n{top}        assert false, \"inner boom\"\n"),
        "caught" => format!(
            "fn owner():\n    _r := recover:\n{inner}{ib}assert false, \"first boom\"\n        1\n    out: Channel[int] = Channel(2)\n    parallel:\n        spawn:\n            out.send(1)\n        spawn:\n            out.send(2)\n    print(\"second done\")\n    assert false, \"inner boom\"\n"
        ),
        "question" => format!(
            "fn owner():\n    _r := recover:\n{inner}{ib}x := bad()?\n{ib}print(x)\n        1\n    assert false, \"inner boom\"\n"
        ),
        "cancelled" => format!(
            "fn victim():\n{top}{tb}go.send(1)\n{tb}j := 0\n{tb}while true:\n{tb}    j = j + 1\nfn owner():\n    parallel:\n        spawn:\n            victim()\n        _ := go.recv()\n        assert false, \"inner boom\"\n"
        ),
        "child_fault" => format!(
            "fn owner():\n{top}        spawn:\n            assert false, \"inner boom\"\n        _r := recover:\n            j := 0\n            while true:\n                j = j + 1\n            1\n"
        ),
        "return_defer" => format!(
            "fn escapes():\n{top}        defer boom()\n        return\nfn owner():\n    r := recover:\n        escapes()\n        1\n    print(r)\n    assert false, \"inner boom\"\n"
        ),
        "question_defer" => format!(
            "fn owner():\n    r := recover:\n{inner}            defer boom()\n            x := bad()?\n            print(x)\n        1\n    print(r)\n    assert false, \"inner boom\"\n"
        ),
        other => panic!("unknown escape {other}"),
    }
}

/// One cell's program: `owner()` (see [`owner_fn`]) runs as `owner`, and every fn in
/// `neighbours` runs as a fiber outside its family.
fn program(owner: &str, escape: &str, wait: &str, depth: usize, neighbours: &[&str]) -> String {
    let mut src = String::from(PRELUDE);
    src.push_str(&owner_fn(escape, wait, depth));
    match owner {
        "task" | "ancestor" | "callback" => {
            let call = match owner {
                "task" => "owner",
                "ancestor" => "parent",
                _ => "cb_owner",
            };
            src.push_str(&format!("parallel:\n    spawn:\n        {call}()\n"));
            for f in neighbours {
                src.push_str(&format!("    spawn:\n        {f}()\n"));
            }
        }
        "main" if neighbours.is_empty() => src.push_str("owner()\n"),
        "main" => {
            src.push_str("parallel:\n");
            for f in neighbours {
                src.push_str(&format!("    spawn:\n        {f}()\n"));
            }
            src.push_str("    owner()\n");
        }
        "job" => {
            src.push_str("ex := Executor()\n");
            for f in neighbours {
                src.push_str(&format!("_ := ex.submit(fn(): {f}())\n"));
            }
            src.push_str("t := ex.submit(fn(): owner())\nmatch t.get():\n    ?_: pass\n    !e: panic(e.message())\n");
        }
        other => panic!("unknown owner kind {other}"),
    }
    src.push_str("print(\"end\")\n");
    src
}

#[test]
fn owner_fault_is_reported_at_every_owner_escape_child_neighbour_and_worker_count() {
    let dir = std::env::temp_dir().join(format!("chz-t287-grid-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let mut cells = Vec::new();
    for owner in OWNERS {
        for escape in ESCAPES {
            let depths: &[usize] = if NESTED.contains(&escape) {
                &DEPTHS
            } else {
                &[1]
            };
            for &depth in depths {
                for (child, wait) in CHILDREN {
                    for (nb, fns) in NEIGHBOURS {
                        let name = format!("{owner}_{escape}_d{depth}_{child}_{nb}");
                        let path = dir.join(format!("{name}.chz"));
                        std::fs::write(&path, program(owner, escape, wait, depth, fns))
                            .expect("write program");
                        for threads in THREADS {
                            cells.push((name.clone(), escape, path.clone(), threads));
                        }
                    }
                }
            }
        }
    }
    let width = std::thread::available_parallelism().map_or(4, |n| n.get());
    let mut bad = Vec::new();
    for chunk in cells.chunks(width) {
        let results: Vec<_> = std::thread::scope(|s| {
            let handles: Vec<_> = chunk
                .iter()
                .map(|(name, escape, path, threads)| {
                    s.spawn(move || {
                        let out = hang_deadline::run_with_hang_deadline(path, *threads);
                        (name, *escape, *threads, out)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("cell thread"))
                .collect()
        });
        for (name, escape, threads, out) in results {
            let t = threads.unwrap_or("default");
            match out {
                None => bad.push(format!("{name} T={t}: no exit within the hang deadline")),
                Some(o) => {
                    let all = format!(
                        "{}{}",
                        String::from_utf8_lossy(&o.stdout),
                        String::from_utf8_lossy(&o.stderr)
                    );
                    // The `caught` form must also run its second `parallel:` to the end, and a
                    // defer form must keep its `defer` fault as the `recover:` value.
                    let needed = match escape {
                        "caught" => "second done",
                        "return_defer" | "question_defer" => "defer boom",
                        _ => "",
                    };
                    let continued = all.contains(needed);
                    if o.status.code() != Some(1) || !all.contains("inner boom") || !continued {
                        bad.push(format!("{name} T={t}: rc={:?} {all:?}", o.status.code()));
                    }
                }
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        bad.is_empty(),
        "{} of {} cells did not report `inner boom`:\n{}",
        bad.len(),
        cells.len(),
        bad.join("\n")
    );
}

/// An owner asleep in its `parallel:` body is woken by a child fault into a `recover:` outside the
/// nursery. The catch parks at the resume (`on_step_fault` from the offload resume), and the run
/// must not resume that park at once: the cancelled child holds its family open for 700 ms, and a
/// neighbour that wakes meanwhile and blocks in place must not land on the owner's stack. Which
/// runner takes the neighbour is a race, so the program runs 12 times per worker count.
const SLEEP_CATCH: &str = "import std.time\nfn on_sleep(x: int) -> int:\n    time.sleep_ms(60000)\n    return x\nfn late():\n    time.sleep_ms(300)\n    print([0].map(on_sleep))\nfn owner():\n    r := recover:\n        parallel:\n            spawn:\n                defer time.sleep_ms(700)\n                time.sleep_ms(60000)\n            spawn:\n                time.sleep_ms(100)\n                assert false, \"first boom\"\n            time.sleep_ms(60000)\n        1\n    print(r)\n    assert false, \"inner boom\"\nparallel:\n    spawn:\n        owner()\n    spawn:\n        late()\nprint(\"end\")\n";

#[test]
fn caught_fault_woken_at_an_offload_resume_parks_before_it_waits() {
    let dir = std::env::temp_dir().join(format!("chz-t287-sleep-catch-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("sleep_catch.chz");
    std::fs::write(&path, SLEEP_CATCH).expect("write program");
    let runs: Vec<(&str, usize)> = ["2", "3"]
        .into_iter()
        .flat_map(|t| (0..12).map(move |i| (t, i)))
        .collect();
    let results: Vec<_> = std::thread::scope(|s| {
        let handles: Vec<_> = runs
            .iter()
            .map(|(t, _)| {
                let path = &path;
                s.spawn(move || hang_deadline::run_with_hang_deadline(path, Some(*t)))
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("run thread"))
            .collect()
    });
    let _ = std::fs::remove_dir_all(&dir);
    let mut bad = Vec::new();
    for ((t, i), out) in runs.iter().zip(results) {
        match out {
            None => bad.push(format!("T={t} run {i}: no exit within the hang deadline")),
            Some(o) => {
                let all = format!(
                    "{}{}",
                    String::from_utf8_lossy(&o.stdout),
                    String::from_utf8_lossy(&o.stderr)
                );
                if o.status.code() != Some(1)
                    || !all.contains("inner boom")
                    || !all.contains("first boom")
                {
                    bad.push(format!("T={t} run {i}: rc={:?} {all:?}", o.status.code()));
                }
            }
        }
    }
    assert!(
        bad.is_empty(),
        "{} of {} runs did not report:\n{}",
        bad.len(),
        runs.len(),
        bad.join("\n")
    );
}

/// TICKET-294: a `recover:` around `depth` nested `parallel:` levels. Every level has a child that
/// waits on a dead channel and sleeps in a `defer`, so the fault unwind parks at each level in turn.
/// The catch resume must park per level too; a neighbour that wakes late and blocks in place in a
/// callback must not land on the owner's stack. Which runner takes the neighbour is a race.
fn nested_catch_program(depth: usize) -> String {
    let mut src = String::from(
        "import std.time\ndead: Channel[int] = Channel()\ndead2: Channel[int] = Channel()\nfn on_dead(x: int) -> int:\n    v := dead2.recv() else: panic(\"dead2 closed\")\n    return x + v\nfn late_inplace():\n    time.sleep_ms(150)\n    print([0].map(on_dead))\nfn owner():\n    r := recover:\n",
    );
    for level in 0..depth {
        let ind = " ".repeat(8 + 4 * level);
        let sleep = if level + 1 == depth { 50 } else { 600 };
        src.push_str(&format!(
            "{ind}parallel:\n{ind}    spawn:\n{ind}        defer time.sleep_ms({sleep})\n{ind}        defer print(\"child {level} defer\")\n{ind}        print(dead.recv())\n"
        ));
    }
    let ind = " ".repeat(8 + 4 * (depth - 1) + 4);
    src.push_str(&format!("{ind}assert false, \"boom\"\n        1\n"));
    src.push_str("    print(\"caught\", r)\n    assert false, \"inner boom\"\nparallel:\n    spawn:\n        owner()\n    spawn:\n        late_inplace()\nprint(\"end\")\n");
    src
}

#[test]
fn caught_fault_under_nested_levels_parks_per_level_beside_a_late_inplace_neighbour() {
    let dir = std::env::temp_dir().join(format!("chz-t294-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let mut runs = Vec::new();
    for depth in [2usize, 3] {
        let path = dir.join(format!("nested{depth}.chz"));
        std::fs::write(&path, nested_catch_program(depth)).expect("write program");
        for i in 0..12 {
            runs.push((depth, i, path.clone()));
        }
    }
    let results: Vec<_> = std::thread::scope(|s| {
        let handles: Vec<_> = runs
            .iter()
            .map(|(_, _, path)| {
                s.spawn(move || hang_deadline::run_with_hang_deadline(path, Some("2")))
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("run thread"))
            .collect()
    });
    let _ = std::fs::remove_dir_all(&dir);
    let mut bad = Vec::new();
    for ((depth, i, _), out) in runs.iter().zip(results) {
        match out {
            None => bad.push(format!(
                "depth {depth} run {i}: no exit within the hang deadline"
            )),
            Some(o) => {
                let all = format!(
                    "{}{}",
                    String::from_utf8_lossy(&o.stdout),
                    String::from_utf8_lossy(&o.stderr)
                );
                if o.status.code() != Some(1)
                    || !all.contains("inner boom")
                    || !all.contains("caught")
                {
                    bad.push(format!(
                        "depth {depth} run {i}: rc={:?} {all:?}",
                        o.status.code()
                    ));
                }
            }
        }
    }
    assert!(
        bad.is_empty(),
        "{} of {} nested-catch runs did not report:\n{}",
        bad.len(),
        runs.len(),
        bad.join("\n")
    );
}
