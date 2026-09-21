//! TICKET-155 — `Vm::guarded_checkpoint` has no owner rung, so a nursery OWNER running a native-HOF
//! callback (`list.map`) never learns a child faulted and burns every remaining element first.

use std::process::{Command, Output};

/// One fixture path per test: `RUST_TEST_THREADS = "2"` (`.cargo/config.toml`) runs two of these at once.
fn run_fixture(name: &str, src: &[&str], threads: Option<&str>) -> Output {
    let dir = std::env::temp_dir().join(format!("chz-ticket155-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    let path = dir.join("owner_map.chz");
    std::fs::write(&path, src.join("\n")).expect("write fixture");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.arg("run").arg(&path);
    match threads {
        Some(n) => cmd.env("CHEZZI_THREADS", n),
        None => cmd.env_remove("CHEZZI_THREADS"),
    };
    cmd.output().expect("run chezzi")
}

const OWNER_MAP: &[&str] = &[
    "fn main():",
    "    xs := [0]",
    "    i := 0",
    "    while i < 3000000:",
    "        xs.push(i)",
    "        i += 1",
    "    parallel:",
    "        spawn:",
    "            panic(\"child boom\")",
    "        ys := xs.map(fn(x) -> int: x * 2 + 1)",
    "        print(\"owner finished map len={ys.len()}\")",
    "main()",
    "",
];

fn stdout_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn assert_cut_short(out: &Output, marker: &str) {
    let stdout = stdout_of(out);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("child boom") && !out.status.success(),
        "the child's fault must still be reported at nonzero rc (rc {}, stderr: {stderr})",
        out.status
    );
    assert!(
        !stdout.contains(marker),
        "{marker}: the child fault did not cut the owner's map short (stdout: {stdout:?})"
    );
}

#[test]
fn child_fault_cuts_short_owner_map() {
    assert_cut_short(
        &run_fixture("map_default", OWNER_MAP, None),
        "owner finished map",
    );
}

#[test]
fn child_fault_cuts_short_owner_map_at_threads_one() {
    assert_cut_short(
        &run_fixture("map_t1", OWNER_MAP, Some("1")),
        "owner finished map",
    );
}

#[test]
fn child_fault_cuts_short_owner_map_at_threads_two() {
    assert_cut_short(
        &run_fixture("map_t2", OWNER_MAP, Some("2")),
        "owner finished map",
    );
}

#[test]
fn child_fault_cuts_short_owner_fold() {
    let src = [
        "fn main():",
        "    xs := [0]",
        "    i := 0",
        "    while i < 3000000:",
        "        xs.push(i)",
        "        i += 1",
        "    parallel:",
        "        spawn:",
        "            panic(\"child boom\")",
        "        total := xs.fold(0, fn(acc: int, x: int) -> int: acc + x)",
        "        print(\"owner finished fold total={total}\")",
        "main()",
        "",
    ];
    assert_cut_short(&run_fixture("fold", &src, None), "owner finished fold");
}

#[test]
fn an_owner_defer_running_a_map_is_not_truncated() {
    let src = [
        "fn main():",
        "    xs := [0]",
        "    i := 0",
        "    while i < 300000:",
        "        xs.push(i)",
        "        i += 1",
        "    parallel:",
        "        defer print(\"defer map len={xs.map(fn(x) -> int: x * 2).len()}\")",
        "        spawn:",
        "            panic(\"child boom\")",
        "        ys := xs.map(fn(x) -> int: x * 2 + 1)",
        "        print(\"owner finished map len={ys.len()}\")",
        "main()",
        "",
    ];
    let out = run_fixture("defer", &src, None);
    let stdout = stdout_of(&out);
    assert!(
        !stdout.contains("owner finished map"),
        "the owner's map was not cut short (stdout: {stdout:?})"
    );
    assert!(
        stdout.contains("defer map len=300001"),
        "the owner's defer body was truncated by the owner rung (stdout: {stdout:?})"
    );
}

#[test]
fn a_recover_outside_the_parallel_still_catches_the_child_fault() {
    let src = [
        "fn main():",
        "    xs := [0]",
        "    i := 0",
        "    while i < 3000000:",
        "        xs.push(i)",
        "        i += 1",
        "    r := recover:",
        "        parallel:",
        "            spawn:",
        "                panic(\"child boom\")",
        "            ys := xs.map(fn(x) -> int: x * 2 + 1)",
        "            print(\"owner finished map len={ys.len()}\")",
        "    match r:",
        "        Ok(_): print(\"UNEXPECTED Ok\")",
        "        Err(e): print(\"outer caught {e.message()}\")",
        "    zs := xs.map(fn(x) -> int: x * 3)",
        "    print(\"after recover map len={zs.len()}\")",
        "main()",
        "",
    ];
    let out = run_fixture("recover", &src, None);
    let stdout = stdout_of(&out);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stdout.contains("outer caught child boom"),
        "the outer recover did not catch the child fault (stdout: {stdout:?}, stderr: {stderr})"
    );
    assert!(
        stdout.contains("after recover map len=3000001"),
        "a map after the recovered fault was truncated (stdout: {stdout:?})"
    );
    assert!(
        !stdout.contains("owner finished map"),
        "the owner's map was not cut short (stdout: {stdout:?})"
    );
    assert!(out.status.success(), "rc {}, stderr: {stderr}", out.status);
}

#[test]
fn a_healthy_nursery_owner_map_runs_to_completion() {
    let src = [
        "fn main():",
        "    xs := [0]",
        "    i := 0",
        "    while i < 3000000:",
        "        xs.push(i)",
        "        i += 1",
        "    parallel:",
        "        spawn:",
        "            print(\"child ok\")",
        "        ys := xs.map(fn(x) -> int: x * 2 + 1)",
        "        print(\"owner finished map len={ys.len()}\")",
        "main()",
        "",
    ];
    let out = run_fixture("healthy", &src, None);
    let stdout = stdout_of(&out);
    assert!(
        stdout.contains("owner finished map len=3000001") && out.status.success(),
        "a healthy nursery's owner map was cut short (rc {}, stdout: {stdout:?})",
        out.status
    );
}

#[test]
fn a_map_with_no_open_nursery_runs_to_completion() {
    let src = [
        "fn main():",
        "    xs := [0]",
        "    i := 0",
        "    while i < 3000000:",
        "        xs.push(i)",
        "        i += 1",
        "    print(\"plain map len={xs.map(fn(x) -> int: x * 2 + 1).len()}\")",
        "main()",
        "",
    ];
    let out = run_fixture("plain", &src, None);
    let stdout = stdout_of(&out);
    assert!(
        stdout.contains("plain map len=3000001") && out.status.success(),
        "a plain map was cut short (rc {}, stdout: {stdout:?})",
        out.status
    );
}
