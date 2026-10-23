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

#[cfg(target_family = "wasm")]
mod web {
    use std::{io, marker::PhantomData, num::NonZeroUsize, time::Duration};

    /// One thread: the page's.
    pub fn available_parallelism() -> io::Result<NonZeroUsize> {
        Ok(NonZeroUsize::MIN)
    }
    /// Nothing else runs while this thread would wait, so it doesn't.
    pub fn sleep(_: Duration) {}

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
