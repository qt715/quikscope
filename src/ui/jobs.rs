//! Work in flight that writes the user's files: recordings being saved,
//! exports, editor saves. Quitting waits for all of it, so `quikscope
//! quit`, the tray's Quit or SIGTERM never kill a half-written file, and
//! refuses new work from the moment it begins.

use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

use futures::channel::oneshot;

static JOBS: LazyLock<Arc<Registry>> = LazyLock::new(|| Arc::new(Registry::new()));

/// Registers new work, refused once quitting began.
pub fn begin() -> anyhow::Result<Job> {
    Registry::begin(&JOBS)
}

/// Refuses new work and resolves once every running job has ended.
pub async fn finish_all() {
    JOBS.finish_all().await
}

/// One piece of work quitting waits for; dropping it ends the work.
#[must_use = "the job ends when this is dropped"]
#[derive(Debug)]
pub struct Job(Arc<Registry>);

impl Drop for Job {
    fn drop(&mut self) {
        let mut state = self.0.lock();
        state.running -= 1;
        if state.running == 0 {
            for idle in state.idle.drain(..) {
                let _ = idle.send(());
            }
        }
    }
}

#[derive(Debug)]
struct Registry(Mutex<State>);

#[derive(Debug)]
struct State {
    running: usize,
    closed: bool,
    /// Woken when `running` drops to zero.
    idle: Vec<oneshot::Sender<()>>,
}

impl Registry {
    fn new() -> Self {
        Registry(Mutex::new(State {
            running: 0,
            closed: false,
            idle: Vec::new(),
        }))
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // The state stays consistent across a panic: every update is one
        // field assignment.
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn begin(this: &Arc<Self>) -> anyhow::Result<Job> {
        let mut state = this.lock();
        anyhow::ensure!(!state.closed, "quikscope is quitting");
        state.running += 1;
        Ok(Job(this.clone()))
    }

    async fn finish_all(&self) {
        loop {
            let idle = {
                let mut state = self.lock();
                state.closed = true;
                if state.running == 0 {
                    return;
                }
                log::info!("waiting for {} file(s) still being written", state.running);
                let (tx, rx) = oneshot::channel();
                state.idle.push(tx);
                rx
            };
            let _ = idle.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::FutureExt as _;

    #[test]
    fn quitting_waits_for_running_jobs_and_refuses_new_ones() {
        let jobs = Arc::new(Registry::new());
        let (a, b) = (
            Registry::begin(&jobs).unwrap(),
            Registry::begin(&jobs).unwrap(),
        );
        let mut finished = Box::pin(jobs.finish_all());
        assert!(finished.as_mut().now_or_never().is_none());
        assert!(
            Registry::begin(&jobs).is_err(),
            "no new work once quitting began"
        );
        drop(a);
        assert!(finished.as_mut().now_or_never().is_none(), "b still runs");
        let other = std::thread::spawn(move || drop(b));
        futures::executor::block_on(finished);
        other.join().unwrap();
    }
}
