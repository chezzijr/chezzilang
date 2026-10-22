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
