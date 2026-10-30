//! VM: the reaper, the one OS thread that drops the freed objects whose drop can block.
//!
//! A sweep frees a garbage `io.buffered` writer whose drop writes its tail to the inner file
//! (`WriterCore::drop`) and a `Cffi` whose drop runs `dlclose`. Each drop can block for as long as the fd or the C
//! code wants. If the thread that collected ran it, the block would stall a task runner, and the
//! task that runner takes next would not run (two Executor jobs, each with a FIFO: job 2 waits
//! behind job 1's flush). Go runs every finalizer on one dedicated goroutine for the same reason.
//!
//! The reaper thread runs no task and never attaches to the GC, so a stop never waits for it. It
//! drops the batches in the order the collectors sent them, so two blocked drops queue behind each
//! other (Go: one blocked finalizer blocks the next). A batch holds only the kinds
//! `heap_core::drop_site` sends here, and their drops read no heap object: a `WriterCore` holds a
//! `File`, an inner `Arc<WriterCore>` and bytes, and a `Cffi` holds a library handle, a
//! symbol address and its signature.
//!
//! A run waits for its own batches when its heap core drops (`HeapCore`'s `Drop`), so a tail that
//! a sweep freed reaches its fd before the run returns, as a live writer's does at teardown.

use super::heap::Obj;
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

/// The batches of one heap core that the reaper did not drop yet.
#[derive(Debug, Default)]
pub(super) struct Pending {
    n: Mutex<usize>,
    cv: Condvar,
}

impl Pending {
    /// Wait until the reaper dropped every batch of this core.
    pub(super) fn wait(&self) {
        let mut n = super::heap_core::lock(&self.n);
        while *n > 0 {
            n = self.cv.wait(n).unwrap_or_else(|e| e.into_inner());
        }
    }

    fn add(&self) {
        *super::heap_core::lock(&self.n) += 1;
    }

    fn done(&self) {
        *super::heap_core::lock(&self.n) -= 1;
        self.cv.notify_all();
    }
}

struct Batch {
    dead: Vec<Obj>,
    pending: Arc<Pending>,
}

/// The reaper's queue; `None` when the thread could not start.
fn queue() -> Option<&'static Sender<Batch>> {
    static QUEUE: OnceLock<Option<Sender<Batch>>> = OnceLock::new();
    QUEUE
        .get_or_init(|| {
            let (tx, rx) = channel::<Batch>();
            std::thread::Builder::new()
                .name("chezzi-reaper".into())
                .spawn(move || {
                    for b in rx {
                        drop(b.dead);
                        b.pending.done();
                    }
                })
                .ok()
                .map(|_| tx)
        })
        .as_ref()
}

/// Hand `dead` to the reaper, counted on `pending`. With no reaper thread, drop it here.
pub(super) fn reap(dead: Vec<Obj>, pending: &Arc<Pending>) {
    let Some(tx) = queue() else {
        drop(dead);
        return;
    };
    pending.add();
    if let Err(e) = tx.send(Batch {
        dead,
        pending: Arc::clone(pending),
    }) {
        drop(e.0.dead);
        pending.done();
    }
}
