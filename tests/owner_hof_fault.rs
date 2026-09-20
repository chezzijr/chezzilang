//! TICKET-155 — `Vm::guarded_checkpoint` has no owner rung, so a nursery OWNER running a native-HOF
//! callback (`list.map`) never learns a child faulted and burns every remaining element first.

use std::process::{Command, Output};

fn run_fixture(src: &[&str], threads: Option<&str>) -> Output {
    let dir = std::env::temp_dir().join(format!("chz-ticket155-{}", std::process::id()));
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

#[test]
fn child_fault_cuts_short_owner_map() {
    let out = run_fixture(
        &[
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
        ],
        None,
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("child boom") && !out.status.success(),
        "the child's fault must still be reported at nonzero rc (rc {}, stderr: {stderr})",
        out.status
    );
    assert!(
        !stdout.contains("owner finished map"),
        "owner finished map: the child fault did not cut the owner's map short (stdout: {stdout:?})"
    );
}
