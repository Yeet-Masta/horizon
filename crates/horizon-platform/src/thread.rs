//! `std::thread`. The web build runs on the page's one thread (no shared
//! memory, so no cross-origin isolation is needed to serve it): a spawned
//! thread runs to its end where it is spawned, and `join` hands back what it
//! returned. That suits the game's workers that load or compute one thing;
//! the long-lived ones (the chunk workers, the log writer) are turned into
//! work the frame loop pumps where the game starts them.
pub use std::thread::{
    AccessError, LocalKey, Result, Thread, ThreadId, current, panicking, park, park_timeout,
    yield_now,
};
#[cfg(not(target_family = "wasm"))]
pub use std::thread::{
    Builder, JoinHandle, Scope, ScopedJoinHandle, available_parallelism, scope, sleep, spawn,
};
#[cfg(target_family = "wasm")]
pub use web::*;

/// Runs `task` on a thread named `name`, blocked on there, as a thread that
/// waits on the network does.
#[cfg(not(target_family = "wasm"))]
pub fn spawn_task<F>(name: &str, task: F) -> std::io::Result<JoinHandle<()>>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    Builder::new()
        .name(name.to_owned())
        .spawn(move || crate::block_on(task))
}
/// On the web nothing may block: the task runs beside the page's event loop
/// (and may hold the browser's objects, which never leave its thread). The
/// handle's `join` does not wait for it.
#[cfg(target_family = "wasm")]
pub fn spawn_task<F>(_name: &str, task: F) -> std::io::Result<JoinHandle<()>>
where
    F: std::future::Future<Output = ()> + 'static,
{
    wasm_bindgen_futures::spawn_local(task);
    Ok(JoinHandle((), std::thread::current()))
}

/// Runs `task`, which blocks its thread (it reads a pipe or waits on a
/// channel), on a thread of its own: as [`spawn_task`] natively, and on the
/// web build with threads a worker wherever it is started (the page's thread
/// may not block, and a worker's has no event loop for [`spawn_task`] to run
/// the task beside). The one-thread web build runs it beside the page.
#[cfg(not(target_family = "wasm"))]
pub fn spawn_blocking_task<F>(name: &str, task: F) -> std::io::Result<JoinHandle<()>>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    spawn_task(name, task)
}
#[cfg(all(target_family = "wasm", target_feature = "atomics"))]
pub fn spawn_blocking_task<F>(name: &str, task: F) -> std::io::Result<JoinHandle<()>>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    spawn_worker(name, move || crate::block_on(task))?;
    Ok(JoinHandle((), std::thread::current()))
}
#[cfg(all(target_family = "wasm", not(target_feature = "atomics")))]
pub fn spawn_blocking_task<F>(name: &str, task: F) -> std::io::Result<JoinHandle<()>>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    spawn_task(name, task)
}

/// Whether this thread may block: everywhere but the web build's page
/// thread (every thread of the one-thread build is the page's).
pub fn may_block() -> bool {
    #[cfg(all(target_family = "wasm", target_feature = "atomics"))]
    return !crate::web::on_page_thread();
    #[allow(unreachable_code)]
    !crate::WEB
}

/// A thread that runs beside the one that started it, such as the
/// integrated server's, and is joined: std's natively; a worker on the web
/// build with threads, whose `join` waits as [`may_block`] allows (the page's
/// thread spins). Without threads it ran where it was started.
#[cfg(not(all(target_family = "wasm", target_feature = "atomics")))]
pub type ThreadHandle<T> = JoinHandle<T>;
#[cfg(not(all(target_family = "wasm", target_feature = "atomics")))]
pub fn spawn_thread<T: Send + 'static>(
    name: &str,
    f: impl FnOnce() -> T + Send + 'static,
) -> std::io::Result<ThreadHandle<T>> {
    Builder::new().name(name.to_owned()).spawn(f)
}
#[cfg(all(target_family = "wasm", target_feature = "atomics"))]
pub struct ThreadHandle<T>(std::sync::Arc<crate::sync::Mutex<Option<T>>>);
#[cfg(all(target_family = "wasm", target_feature = "atomics"))]
impl<T> ThreadHandle<T> {
    /// What the thread returned, once it has. (A panic aborts the whole
    /// web build, so there is never one to hand back.)
    pub fn join(self) -> Result<T> {
        loop {
            if let Some(out) = self.0.lock().unwrap().take() {
                return Ok(out);
            }
            if may_block() {
                std::thread::sleep(std::time::Duration::from_millis(1));
            } else {
                std::hint::spin_loop();
            }
        }
    }
    pub fn is_finished(&self) -> bool {
        self.0.lock().unwrap().is_some()
    }
}
#[cfg(all(target_family = "wasm", target_feature = "atomics"))]
pub fn spawn_thread<T: Send + 'static>(
    name: &str,
    f: impl FnOnce() -> T + Send + 'static,
) -> std::io::Result<ThreadHandle<T>> {
    let slot = std::sync::Arc::new(crate::sync::Mutex::new(None));
    let out = slot.clone();
    spawn_worker(name, move || {
        let value = f();
        *out.lock().unwrap() = Some(value);
    })?;
    Ok(ThreadHandle(slot))
}

/// Starts a long-lived worker, such as the chunk workers, on a thread of
/// its own: a std thread natively; a Web Worker sharing the page's memory on
/// the web build with threads (web/README.md). Unlike [`spawn`], which runs
/// where it is called on the web, the work then runs beside the page, so it
/// may wait on locks and channels; it is never joined.
#[cfg(not(all(target_family = "wasm", target_feature = "atomics")))]
pub fn spawn_worker(name: &str, work: impl FnOnce() + Send + 'static) -> std::io::Result<()> {
    Builder::new().name(name.to_owned()).spawn(work).map(drop)
}
#[cfg(all(target_family = "wasm", target_feature = "atomics"))]
pub fn spawn_worker(name: &str, work: impl FnOnce() + Send + 'static) -> std::io::Result<()> {
    crate::web::spawn_thread(name, Box::new(work));
    Ok(())
}
/// How many [`spawn_worker`] threads make sense: natively the cores less the
/// one the game runs on (its workers run behind it, `horizon_threads`). A
/// browser can't put a worker behind the page, so the threaded web build
/// takes half the cores, at most 3 (more ran slower in Firefox and Chromium
/// alike), leaving the rest to the page and the browser's own threads;
/// `HORIZON_WEB_WORKERS` sets it.
pub fn worker_count() -> usize {
    #[cfg(all(target_family = "wasm", target_feature = "atomics"))]
    {
        let set = crate::env::var("HORIZON_WEB_WORKERS")
            .ok()
            .and_then(|n| n.trim().parse::<usize>().ok());
        return set
            .unwrap_or((crate::web::hardware_concurrency() / 2).min(3))
            .clamp(1, 16);
    }
    #[allow(unreachable_code)]
    available_parallelism().map_or(1, |n| n.get().saturating_sub(1).max(1))
}

/// Waits `duration` in a task: the thread sleeps natively and on the web
/// build's workers; on the page's thread the task gives the page back until
/// a timer fires. The future is `Send`, as a task's may need to be.
pub async fn sleep_task(duration: std::time::Duration) {
    if may_block() {
        sleep(duration);
        return;
    }
    #[cfg(target_family = "wasm")]
    web::Timer::new(duration).await;
}

#[cfg(target_family = "wasm")]
mod web {
    use std::{io, marker::PhantomData, num::NonZeroUsize, time::Duration};

    /// One thread: the page's.
    pub fn available_parallelism() -> io::Result<NonZeroUsize> {
        Ok(NonZeroUsize::MIN)
    }
    /// Nothing else runs while the page's thread would wait, so it doesn't;
    /// the threaded build's workers sleep.
    pub fn sleep(duration: Duration) {
        if super::may_block() {
            std::thread::sleep(duration);
        }
    }

    /// The page's `setTimeout`, as a future: what it shares with the
    /// timer's handler is `Send`, as the task holding it may be.
    pub(super) struct Timer {
        duration: Duration,
        fired: std::sync::Arc<std::sync::Mutex<(bool, Option<std::task::Waker>)>>,
        set: bool,
    }
    impl Timer {
        pub(super) fn new(duration: Duration) -> Self {
            Self {
                duration,
                fired: Default::default(),
                set: false,
            }
        }
    }
    impl std::future::Future for Timer {
        type Output = ();
        fn poll(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<()> {
            use wasm_bindgen::{JsCast, closure::Closure};
            {
                let mut fired = self.fired.lock().unwrap();
                if fired.0 {
                    return std::task::Poll::Ready(());
                }
                fired.1 = Some(cx.waker().clone());
            }
            if !self.set {
                self.set = true;
                let fired = self.fired.clone();
                let handler = Closure::once_into_js(move || {
                    let mut fired = fired.lock().unwrap();
                    fired.0 = true;
                    if let Some(waker) = fired.1.take() {
                        waker.wake();
                    }
                });
                let _ = js_sys::Reflect::get(&js_sys::global(), &"setTimeout".into())
                    .ok()
                    .and_then(|f| f.dyn_into::<js_sys::Function>().ok())
                    .map(|set_timeout| {
                        set_timeout.call2(
                            &wasm_bindgen::JsValue::UNDEFINED,
                            &handler,
                            &(self.duration.as_secs_f64() * 1000.0).into(),
                        )
                    });
            }
            std::task::Poll::Pending
        }
    }

    pub fn spawn<F, T>(f: F) -> JoinHandle<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        Builder::new().spawn(f).expect("spawn")
    }

    #[derive(Debug, Default)]
    pub struct Builder {
        name: Option<String>,
    }
    impl Builder {
        pub fn new() -> Self {
            Self::default()
        }
        pub fn name(mut self, name: String) -> Self {
            self.name = Some(name);
            self
        }
        pub fn stack_size(self, _: usize) -> Self {
            self
        }
        pub fn spawn<F, T>(self, f: F) -> io::Result<JoinHandle<T>>
        where
            F: FnOnce() -> T + Send + 'static,
            T: Send + 'static,
        {
            Ok(JoinHandle(f(), std::thread::current()))
        }
        pub fn spawn_scoped<'scope, 'env, F, T>(
            self,
            scope: &'scope Scope<'scope, 'env>,
            f: F,
        ) -> io::Result<ScopedJoinHandle<'scope, T>>
        where
            F: FnOnce() -> T + Send + 'scope,
            T: Send + 'scope,
        {
            Ok(scope.spawn(f))
        }
    }

    /// A finished thread's result.
    pub struct JoinHandle<T>(pub(super) T, pub(super) std::thread::Thread);
    impl<T> JoinHandle<T> {
        pub fn join(self) -> std::thread::Result<T> {
            Ok(self.0)
        }
        pub fn is_finished(&self) -> bool {
            true
        }
        pub fn thread(&self) -> &std::thread::Thread {
            &self.1
        }
    }

    pub struct Scope<'scope, 'env: 'scope> {
        scope: PhantomData<&'scope mut &'scope ()>,
        env: PhantomData<&'env mut &'env ()>,
    }
    impl<'scope> Scope<'scope, '_> {
        pub fn spawn<F, T>(&'scope self, f: F) -> ScopedJoinHandle<'scope, T>
        where
            F: FnOnce() -> T + Send + 'scope,
            T: Send + 'scope,
        {
            ScopedJoinHandle(f(), PhantomData)
        }
    }
    pub struct ScopedJoinHandle<'scope, T>(T, PhantomData<&'scope ()>);
    impl<T> ScopedJoinHandle<'_, T> {
        pub fn join(self) -> std::thread::Result<T> {
            Ok(self.0)
        }
        pub fn is_finished(&self) -> bool {
            true
        }
    }
    pub fn scope<'env, F, T>(f: F) -> T
    where
        F: for<'scope> FnOnce(&'scope Scope<'scope, 'env>) -> T,
    {
        f(&Scope {
            scope: PhantomData,
            env: PhantomData,
        })
    }
}
