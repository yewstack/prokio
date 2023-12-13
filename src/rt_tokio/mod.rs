use std::future::Future;
use std::marker::PhantomData;
use std::sync::Arc;
use std::{fmt, io};

use once_cell::sync::Lazy;

pub(crate) mod time;

mod local_worker;

use local_worker::LocalWorker;

/// We test whether thread is supported for current target.
static THREAD_SUPPORTED: Lazy<bool> =
    Lazy::new(|| std::thread::Builder::new().spawn(|| {}).is_ok());

pub(crate) fn get_default_runtime_size() -> usize {
    // We use num_cpus as std::thread::available_parallelism() does not take
    // system resource constraint (e.g.: cgroups) into consideration.
    #[cfg(not(target_os = "wasi"))]
    {
        num_cpus::get()
    }
    // WASI does not support multi-threading at this moment.
    #[cfg(target_os = "wasi")]
    {
        0
    }
}

#[inline(always)]
pub(super) fn spawn_local<F>(f: F)
where
    F: Future<Output = ()> + 'static,
{
    match LocalHandle::try_current() {
        Some(m) => {
            // If within a prokio runtime, use a local handle increases the local task count.
            m.spawn_local(f);
        }
        None => {
            tokio::task::spawn_local(f);
        }
    }
}

#[derive(Clone)]
enum RuntimeInner {
    /// Target has multi-threading support.
    Threaded { workers: Arc<Vec<LocalWorker>> },
    /// Target does not have multi-threading support.
    Main,
}

impl fmt::Debug for RuntimeInner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Threaded { .. } => f
                .debug_struct("RuntimeInner::Threaded")
                .field("workers", &"Vec<LocalWorker>")
                .finish(),
            Self::Main => f.debug_struct("RuntimeInner::Main").finish(),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Runtime {
    inner: RuntimeInner,
}

impl Default for Runtime {
    fn default() -> Self {
        static DEFAULT_RT: Lazy<Runtime> = Lazy::new(|| {
            Runtime::new(get_default_runtime_size()).expect("failed to create runtime.")
        });

        DEFAULT_RT.clone()
    }
}

impl Runtime {
    pub fn new(size: usize) -> io::Result<Self> {
        if *THREAD_SUPPORTED {
            assert!(size > 0, "must have more than 1 worker.");

            let mut workers = Vec::with_capacity(size);

            for _ in 0..size {
                let worker = LocalWorker::new()?;
                workers.push(worker);
            }

            Ok(Self {
                inner: RuntimeInner::Threaded {
                    workers: workers.into(),
                },
            })
        } else {
            Ok(Self {
                inner: RuntimeInner::Main,
            })
        }
    }

    fn find_least_busy_local_worker(workers: &[LocalWorker]) -> &LocalWorker {
        let mut workers = workers.iter();

        let mut worker = workers.next().expect("must have more than 1 worker.");
        let mut task_count = worker.task_count();

        for current_worker in workers {
            if task_count == 0 {
                // We don't have to search until the end.
                break;
            }

            let current_worker_task_count = current_worker.task_count();

            if current_worker_task_count < task_count {
                task_count = current_worker_task_count;
                worker = current_worker;
            }
        }

        worker
    }

    pub fn spawn_pinned<F, Fut>(&self, create_task: F)
    where
        F: FnOnce() -> Fut,
        F: Send + 'static,
        Fut: Future<Output = ()> + 'static,
    {
        match self.inner {
            RuntimeInner::Threaded { ref workers } => {
                let worker = Self::find_least_busy_local_worker(workers);

                worker.spawn_pinned(create_task);
            }

            RuntimeInner::Main => {
                tokio::task::spawn_local(create_task());
            }
        }
    }
}

#[derive(Debug, Clone)]
enum LocalHandleInner {
    Threaded(local_worker::LocalHandle),
    Main,
}

#[derive(Debug, Clone)]
pub(crate) struct LocalHandle {
    inner: LocalHandleInner,
    // This type is not send or sync.
    _marker: PhantomData<*const ()>,
}

impl LocalHandle {
    pub fn try_current() -> Option<Self> {
        if *THREAD_SUPPORTED {
            Some(Self {
                inner: LocalHandleInner::Threaded(local_worker::LocalHandle::try_current()?),
                _marker: PhantomData,
            })
        } else {
            Some(Self {
                inner: LocalHandleInner::Main,
                _marker: PhantomData,
            })
        }
    }

    pub fn current() -> Self {
        if *THREAD_SUPPORTED {
            Self {
                inner: LocalHandleInner::Threaded(local_worker::LocalHandle::current()),
                _marker: PhantomData,
            }
        } else {
            Self {
                inner: LocalHandleInner::Main,
                _marker: PhantomData,
            }
        }
    }

    pub fn spawn_local<F>(&self, f: F)
    where
        F: Future<Output = ()> + 'static,
    {
        match self.inner {
            LocalHandleInner::Threaded(ref m) => {
                m.spawn_local(f);
            }
            LocalHandleInner::Main => {
                tokio::task::spawn_local(f);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use futures::channel::oneshot;
    use tokio::sync::Barrier;
    use tokio::test;
    use tokio::time::timeout;

    use super::*;

    #[test]
    async fn test_spawn_pinned_least_busy() {
        let runtime = Runtime::new(2).expect("failed to create runtime.");

        let (tx1, rx1) = oneshot::channel();
        let (tx2, rx2) = oneshot::channel();

        let bar = Arc::new(Barrier::new(2));

        {
            let bar = bar.clone();
            runtime.spawn_pinned(move || async move {
                bar.wait().await;
                tx1.send(std::thread::current().id())
                    .expect("failed to send!");
            });
        }

        runtime.spawn_pinned(move || async move {
            bar.wait().await;
            tx2.send(std::thread::current().id())
                .expect("failed to send!");
        });

        let result1 = timeout(Duration::from_secs(5), rx1)
            .await
            .expect("task timed out")
            .expect("failed to receive");
        let result2 = timeout(Duration::from_secs(5), rx2)
            .await
            .expect("task timed out")
            .expect("failed to receive");

        // first task and second task are not on the same thread.
        assert_ne!(result1, result2);
    }

    #[test]
    async fn test_spawn_local_within_send() {
        let runtime = Runtime::default();

        let (tx, rx) = oneshot::channel();

        runtime.spawn_pinned(move || async move {
            tokio::task::spawn(async move {
                // tokio::task::spawn_local cannot spawn tasks outside of a local context.
                //
                // prokio::spawn_local can spawn tasks within a Send task as long as running
                // under a Prokio Runtime.
                spawn_local(async move {
                    tx.send(()).expect("failed to send!");
                })
            });
        });

        timeout(Duration::from_secs(5), rx)
            .await
            .expect("task timed out")
            .expect("failed to receive");
    }
}
