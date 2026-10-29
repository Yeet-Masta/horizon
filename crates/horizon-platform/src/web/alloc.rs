//! The threaded web build's allocator. std's on wasm is one dlmalloc behind
//! one lock, taken by spinning (a browser's page thread may not wait), by
//! every allocation of every thread: the workers' allocations held the
//! page's frames up and their spinning took the cores. Here each thread
//! keeps the small blocks it frees, by size class, and hands them out again
//! without the lock; only a miss, a large or over-aligned block, or a full
//! class goes to std's allocator.
//!
//! A thread's lists live in its thread-local storage, which wasm-bindgen
//! sets up by allocating it (and the thread's stack): until then this
//! allocator passes straight through to std's.
//!
//! What does go to std's goes through a lock of ours first ([`Lock`]), so
//! std's never sees contention: everyone waiting on std's spins, and with a
//! worker loading the world's generation data while the page and the other
//! workers allocated, the spinners kept the holder from running (Chromium
//! took 143 s over what Firefox did in 2). On ours the workers sleep until
//! the holder wakes them; only the page's thread, which may not, spins.
#[cfg(target_arch = "wasm32")]
use core::arch::wasm32::{memory_atomic_notify, memory_atomic_wait32};
#[cfg(target_arch = "wasm64")]
use core::arch::wasm64::{memory_atomic_notify, memory_atomic_wait32};
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::{Cell, UnsafeCell},
    ptr,
    sync::atomic::{AtomicI32, Ordering},
};

/// Size classes, in bytes: a block up to the largest comes from a class.
const CLASSES: [usize; 12] = [16, 32, 48, 64, 96, 128, 192, 256, 384, 512, 768, 1024];
/// Blocks a thread keeps of a class before its frees go back to std's.
const KEEP: u32 = 512;
/// Every class's blocks are aligned this far.
const ALIGN: usize = 16;
/// Below this address a thread-local's address is only its offset in the
/// block: the thread's storage isn't set up yet (its base is 0).
const SET_UP: usize = 64 << 10;

/// One thread's free blocks of each class: a list through the blocks' first
/// word, and its length.
struct Lists {
    head: [*mut u8; CLASSES.len()],
    len: [u32; CLASSES.len()],
}

thread_local! {
    static LISTS: UnsafeCell<Lists> = const {
        UnsafeCell::new(Lists { head: [ptr::null_mut(); CLASSES.len()], len: [0; CLASSES.len()] })
    };
    /// This thread is a worker, which may sleep ([`worker_started`]).
    static SLEEPS: Cell<bool> = const { Cell::new(false) };
}

/// Called on a worker once its thread-local storage is set up: from then
/// on it sleeps while another thread holds std's allocator.
pub(crate) fn worker_started() {
    let _ = SLEEPS.try_with(|s| s.set(true));
}
fn may_sleep() -> bool {
    SLEEPS
        .try_with(|s| (s as *const Cell<bool> as usize) >= SET_UP && s.get())
        .unwrap_or(false)
}

/// The lock in front of std's allocator: 0 free, 1 held, 2 held with
/// threads waiting (whose release wakes one). Ulrich Drepper's futex mutex.
struct Lock(AtomicI32);
static LOCK: Lock = Lock(AtomicI32::new(0));
/// Looks at a held lock before going to sleep: holds are short.
const SPINS: u32 = 64;
/// A sleeper looks again after this long whatever happens (nanoseconds).
const NAP: i64 = 1_000_000;
impl Lock {
    fn lock(&self) {
        if self
            .0
            .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            return;
        }
        for _ in 0..SPINS {
            std::hint::spin_loop();
            if self.0.load(Ordering::Relaxed) == 0
                && self
                    .0
                    .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
            {
                return;
            }
        }
        let sleep = may_sleep();
        while self.0.swap(2, Ordering::Acquire) != 0 {
            if sleep {
                // SAFETY: the lock's word, a valid aligned i32 for as long as
                // the module runs; waiting is allowed off the page's thread.
                unsafe { memory_atomic_wait32(self.0.as_ptr(), 2, NAP) };
            } else {
                for _ in 0..SPINS {
                    std::hint::spin_loop();
                }
            }
        }
    }
    fn unlock(&self) {
        if self.0.swap(0, Ordering::Release) == 2 {
            // SAFETY: as in `lock`; notifying is allowed on any thread.
            unsafe { memory_atomic_notify(self.0.as_ptr(), 1) };
        }
    }
}
/// `f` (a call of std's allocator) with the lock held.
fn locked<R>(f: impl FnOnce() -> R) -> R {
    LOCK.lock();
    let out = f();
    LOCK.unlock();
    out
}

/// The class a layout's blocks come from.
fn class(layout: Layout) -> Option<usize> {
    if layout.align() > ALIGN {
        return None;
    }
    CLASSES.iter().position(|&size| layout.size() <= size)
}

/// What std's allocator gives a class's blocks.
fn block(class: usize) -> Layout {
    // SAFETY: the classes are nonzero multiples of ALIGN, a power of two.
    unsafe { Layout::from_size_align_unchecked(CLASSES[class], ALIGN) }
}

/// This thread's lists, once its storage is set up.
fn with_lists<R>(f: impl FnOnce(&mut Lists) -> R) -> Option<R> {
    LISTS
        .try_with(|lists| {
            if (lists.get() as usize) < SET_UP {
                return None;
            }
            // SAFETY: a thread's lists are only touched by that thread, and
            // never reentrantly (nothing here allocates).
            Some(f(unsafe { &mut *lists.get() }))
        })
        .ok()
        .flatten()
}

pub struct Cached;

// SAFETY: every block handed out is one std's allocator gave for its
// class's layout (or the caller's, for the rest), and goes back to it with
// that same layout.
unsafe impl GlobalAlloc for Cached {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let Some(c) = class(layout) else {
            // SAFETY: the caller's layout, as GlobalAlloc requires.
            return locked(|| unsafe { System.alloc(layout) });
        };
        let kept = with_lists(|lists| {
            let head = lists.head[c];
            if !head.is_null() {
                // SAFETY: a kept block's first word links to the next one.
                lists.head[c] = unsafe { *head.cast::<*mut u8>() };
                lists.len[c] -= 1;
            }
            head
        });
        match kept {
            Some(block) if !block.is_null() => block,
            // SAFETY: a class's layout is nonzero.
            _ => locked(|| unsafe { System.alloc(block(c)) }),
        }
    }

    unsafe fn dealloc(&self, block_ptr: *mut u8, layout: Layout) {
        let Some(c) = class(layout) else {
            // SAFETY: allocated by std's with this layout.
            return locked(|| unsafe { System.dealloc(block_ptr, layout) });
        };
        let kept = with_lists(|lists| {
            if lists.len[c] >= KEEP {
                return false;
            }
            // SAFETY: the block is at least 16 bytes, 16-aligned, and ours now.
            unsafe { *block_ptr.cast::<*mut u8>() = lists.head[c] };
            lists.head[c] = block_ptr;
            lists.len[c] += 1;
            true
        });
        if kept != Some(true) {
            // SAFETY: the class's blocks come from std's with its layout.
            locked(|| unsafe { System.dealloc(block_ptr, block(c)) });
        }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if class(layout).is_none() {
            // SAFETY: the caller's layout.
            return locked(|| unsafe { System.alloc_zeroed(layout) });
        }
        // A kept block holds what it last held.
        // SAFETY: as `alloc`; the block has at least `layout.size()` bytes.
        unsafe {
            let block = self.alloc(layout);
            if !block.is_null() {
                ptr::write_bytes(block, 0, layout.size());
            }
            block
        }
    }

    unsafe fn realloc(&self, block_ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: GlobalAlloc promises a nonzero size that doesn't overflow
        // with this alignment.
        let new = unsafe { Layout::from_size_align_unchecked(new_size, layout.align()) };
        match (class(layout), class(new)) {
            // SAFETY: std's block, with its layout.
            (None, None) => locked(|| unsafe { System.realloc(block_ptr, layout, new_size) }),
            // The class's block has room for it already.
            (Some(a), Some(b)) if a == b => block_ptr,
            // SAFETY: a fresh block for the new layout, the old one's bytes
            // copied into it and the old one freed with its layout.
            _ => unsafe {
                let moved = self.alloc(new);
                if !moved.is_null() {
                    ptr::copy_nonoverlapping(block_ptr, moved, layout.size().min(new_size));
                    self.dealloc(block_ptr, layout);
                }
                moved
            },
        }
    }
}

#[global_allocator]
static ALLOCATOR: Cached = Cached;
