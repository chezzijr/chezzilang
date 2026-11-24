// vm::sched — split out of vm/mod.rs. `super::*` == the `vm` module.
// Spawn targets, runner and helper threads, and the runner/drainer/blocked-owner guards. The spawn and
// nursery code is in `nursery.rs`, the M:N scheduler in `mnsched*.rs`.

use super::*;

/// Where [`Vm::spawn_into`] puts the task it starts.
pub(super) enum SpawnTarget {
    /// The innermost open `parallel:` of the calling party (`spawn`).
    Nursery,
    /// The own tail scope of `sched`, with no nursery open on the caller (`Executor.submit`).
    /// `settle` is the job's [`JobSettle`] (TICKET-219, `_submit_settled`); `executor` is the
    /// Executor's key ([`super::deadlock::TaskPath::job`]).
    Scope {
        sched: Arc<MnSched>,
        settle: JobSettle,
        executor: Vec<u64>,
    },
}

/// What the head of a one-line `spawn <call>` (or of `Executor.submit(f)`) is, and the entry thunk
/// its site compiled (TICKET-235). Read by [`Vm::task_entry`] only.
#[derive(Clone, Copy)]
pub(super) enum SpawnHead {
    /// A callee value. The thunk is used only when the callee pushes no frame by itself; `None` =
    /// the receiverless wrapper closure, which always does.
    Callee(Option<ProtoId>),
    /// A receiver: the thunk's `Op::CallMethod` dispatches the member on it.
    Receiver(ProtoId),
}

/// TICKET-205 — THE spawn of a raw worker thread of `sched` (the drainer, an eager helper, the
/// replacement of a demoted worker).
pub(super) fn spawn_worker_thread(
    mut shell: Vm,
    sched: &Arc<MnSched>,
    name: &str,
    wid: usize,
    owner_scope: usize,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let s = Arc::clone(sched);
    spawn_runner_thread(sched, name, move || {
        shell.mn_thread_loop(&s, wid, owner_scope)
    })
}

/// TICKET-211 — THE birth of a runner OS thread, behind [`spawn_worker_thread`]: width registration (adopted slot), the VM stack size, the thread name and
/// the `catch_unwind` around `body`. Every runner is gated from birth (TICKET-230), and its SPAWNER
/// lists its slot with the sched, so a wake queues it for the runner permit in the waker's order
/// instead of on the child's own start time. A refused thread withdraws the slot.
pub(super) fn spawn_runner_thread(
    sched: &Arc<MnSched>,
    name: &str,
    body: impl FnOnce() + Send + 'static,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    #[cfg(test)]
    sched.quiesce.runner_spawns.fetch_add(1, Ordering::SeqCst);
    let child_slot = width::new_slot();
    sched.idle_register_child(&child_slot);
    let adopted = Arc::clone(&child_slot);
    let r = std::thread::Builder::new()
        .stack_size(VM_STACK_BYTES)
        .name(name.into())
        .spawn(move || {
            width::adopt(adopted);
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));
        });
    if r.is_err() {
        sched.idle_forget(&child_slot);
    }
    r
}

#[cfg(test)]
thread_local! {
    /// TICKET-211 — this OS thread is already counted in `QuiesceState::runner_threads`.
    static IN_RUNNER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// TICKET-211 — counts one runner thread for the span of its outermost `mn_worker_loop`. A loop
/// nested on a thread that already runs one (a fiber's inline join) is inert. Test-only.
/// TICKET-230 — the outermost loop also points this thread's permits at the run's
/// `QuiesceState::width_probe`, restoring the previous probe on drop.
#[cfg(test)]
pub(super) struct RunnerCount(
    Option<Arc<crate::vm::quiesce::QuiesceState>>,
    Option<Arc<width::WidthProbe>>,
);

#[cfg(test)]
impl RunnerCount {
    pub(super) fn enter(q: &Arc<crate::vm::quiesce::QuiesceState>) -> Self {
        if IN_RUNNER.with(|f| f.replace(true)) {
            return RunnerCount(None, None);
        }
        let now = q.runner_threads.fetch_add(1, Ordering::SeqCst) + 1;
        q.peak_runner_threads.fetch_max(now, Ordering::SeqCst);
        let prev = width::set_probe(Some(Arc::clone(&q.width_probe)));
        RunnerCount(Some(Arc::clone(q)), prev)
    }
}

#[cfg(test)]
impl Drop for RunnerCount {
    fn drop(&mut self) {
        if let Some(q) = self.0.take() {
            q.runner_threads.fetch_sub(1, Ordering::SeqCst);
            IN_RUNNER.with(|f| f.set(false));
            width::set_probe(self.1.take());
        }
    }
}

/// TICKET-205 — see [`Vm::gated_register`].
pub(super) struct GatedReg {
    pub(super) cores: Vec<Arc<ChannelCore>>,
    pub(super) me: Arc<width::Slot>,
}
impl Drop for GatedReg {
    fn drop(&mut self) {
        for c in &self.cores {
            c.gated
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|(s, _)| !Arc::ptr_eq(s, &self.me));
        }
        width::cancel(&self.me);
    }
}
