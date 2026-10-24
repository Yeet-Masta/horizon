//! `std::sync`'s `Mutex` and `RwLock`. A browser traps the page's thread
//! when it would wait on shared memory (`memory.atomic.wait`), so on the web
//! build with threads (`target_feature = "atomics"`, web/README.md) these
//! locks never sleep there: on the page's thread a contended lock spins
//! until it is free, which the short holds the game makes allow. Workers
//! wait as std's locks do (a worldgen lock can be held for a whole chunk).
//! The guards are std's, so std's `Condvar` works with them (on a worker).
//! Everywhere else these are std's own types.
#[cfg(all(target_family = "wasm", target_feature = "atomics"))]
pub use spin::{Mutex, RwLock};
#[cfg(not(all(target_family = "wasm", target_feature = "atomics")))]
pub use std::sync::{Mutex, RwLock};

#[cfg(all(target_family = "wasm", target_feature = "atomics"))]
mod spin {
    use std::{
        fmt,
        sync::{
            self, LockResult, MutexGuard, RwLockReadGuard, RwLockWriteGuard, TryLockError,
            TryLockResult,
        },
    };

    /// Taken by spinning on `try` on the page's thread; elsewhere by `wait`.
    fn spin<G>(
        mut attempt: impl FnMut() -> TryLockResult<G>,
        wait: impl FnOnce() -> LockResult<G>,
    ) -> LockResult<G> {
        if !crate::web::on_page_thread() {
            return wait();
        }
        loop {
            match attempt() {
                Ok(guard) => return Ok(guard),
                Err(TryLockError::Poisoned(poisoned)) => return Err(poisoned),
                Err(TryLockError::WouldBlock) => std::hint::spin_loop(),
            }
        }
    }

    pub struct Mutex<T: ?Sized>(sync::Mutex<T>);
    impl<T> Mutex<T> {
        pub const fn new(value: T) -> Self {
            Self(sync::Mutex::new(value))
        }
        pub fn into_inner(self) -> LockResult<T> {
            self.0.into_inner()
        }
    }
    impl<T: ?Sized> Mutex<T> {
        pub fn lock(&self) -> LockResult<MutexGuard<'_, T>> {
            spin(|| self.0.try_lock(), || self.0.lock())
        }
        pub fn try_lock(&self) -> TryLockResult<MutexGuard<'_, T>> {
            self.0.try_lock()
        }
        pub fn get_mut(&mut self) -> LockResult<&mut T> {
            self.0.get_mut()
        }
        pub fn is_poisoned(&self) -> bool {
            self.0.is_poisoned()
        }
        pub fn clear_poison(&self) {
            self.0.clear_poison();
        }
    }
    impl<T: Default> Default for Mutex<T> {
        fn default() -> Self {
            Self::new(T::default())
        }
    }
    impl<T> From<T> for Mutex<T> {
        fn from(value: T) -> Self {
            Self::new(value)
        }
    }
    impl<T: ?Sized + fmt::Debug> fmt::Debug for Mutex<T> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            self.0.fmt(f)
        }
    }

    pub struct RwLock<T: ?Sized>(sync::RwLock<T>);
    impl<T> RwLock<T> {
        pub const fn new(value: T) -> Self {
            Self(sync::RwLock::new(value))
        }
        pub fn into_inner(self) -> LockResult<T> {
            self.0.into_inner()
        }
    }
    impl<T: ?Sized> RwLock<T> {
        pub fn read(&self) -> LockResult<RwLockReadGuard<'_, T>> {
            spin(|| self.0.try_read(), || self.0.read())
        }
        pub fn write(&self) -> LockResult<RwLockWriteGuard<'_, T>> {
            spin(|| self.0.try_write(), || self.0.write())
        }
        pub fn try_read(&self) -> TryLockResult<RwLockReadGuard<'_, T>> {
            self.0.try_read()
        }
        pub fn try_write(&self) -> TryLockResult<RwLockWriteGuard<'_, T>> {
            self.0.try_write()
        }
        pub fn get_mut(&mut self) -> LockResult<&mut T> {
            self.0.get_mut()
        }
        pub fn is_poisoned(&self) -> bool {
            self.0.is_poisoned()
        }
        pub fn clear_poison(&self) {
            self.0.clear_poison();
        }
    }
    impl<T: Default> Default for RwLock<T> {
        fn default() -> Self {
            Self::new(T::default())
        }
    }
    impl<T> From<T> for RwLock<T> {
        fn from(value: T) -> Self {
            Self::new(value)
        }
    }
    impl<T: ?Sized + fmt::Debug> fmt::Debug for RwLock<T> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            self.0.fmt(f)
        }
    }
}
