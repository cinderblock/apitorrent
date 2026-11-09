//! A per-isolate byte budget for `ArrayBuffer` memory.
//!
//! V8's heap limit doesn't cover `ArrayBuffer` backing stores: they are allocated outside
//! the JavaScript heap, through the embedder's allocator. Without this, a plugin under a
//! 64 MiB heap limit could hold gigabytes in `Uint8Array`s (a probe allocated 4 GiB under a
//! 32 MiB limit). Each isolate gets its own allocator that refuses allocations past its
//! budget; V8 then throws a `RangeError` in the plugin, which it can catch.
//!
//! This is the crate's only `unsafe` code: V8's allocator interface is C function pointers
//! over raw memory.

#![allow(unsafe_code)]

use std::alloc::{Layout, alloc, alloc_zeroed, dealloc};
use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use deno_core::v8;

/// Matches what `malloc` guarantees, so any typed array view is aligned.
const ALIGN: usize = 16;

#[derive(Debug)]
pub(crate) struct Budget {
    used: AtomicUsize,
    limit: usize,
    /// An allocation was refused since the last [`Budget::take_refusal`].
    refused: AtomicBool,
}

impl Budget {
    /// Whether an allocation was refused since the last call. V8 answers a refusal with a
    /// last-resort garbage collection that also invokes the near-heap-limit callback; this
    /// tells that callback the pressure came from the buffer budget, not the heap.
    pub fn take_refusal(&self) -> bool {
        self.refused.swap(false, Ordering::AcqRel)
    }

    fn reserve(&self, len: usize) -> bool {
        let mut used = self.used.load(Ordering::Acquire);
        loop {
            let Some(next) = used.checked_add(len).filter(|n| *n <= self.limit) else {
                self.refused.store(true, Ordering::Release);
                return false;
            };
            match self
                .used
                .compare_exchange_weak(used, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return true,
                Err(actual) => used = actual,
            }
        }
    }

    fn release(&self, len: usize) {
        self.used.fetch_sub(len, Ordering::AcqRel);
    }
}

fn layout(len: usize) -> Option<Layout> {
    // A zero-size allocation must still return a unique, non-null pointer.
    Layout::from_size_align(len.max(1), ALIGN).ok()
}

unsafe extern "C" fn allocate(budget: &Budget, len: usize) -> *mut c_void {
    let Some(layout) = layout(len) else {
        return std::ptr::null_mut();
    };
    if !budget.reserve(len) {
        return std::ptr::null_mut();
    }
    // SAFETY: `layout` has a non-zero size.
    let p = unsafe { alloc_zeroed(layout) };
    if p.is_null() {
        budget.release(len);
    }
    p.cast()
}

unsafe extern "C" fn allocate_uninitialized(budget: &Budget, len: usize) -> *mut c_void {
    let Some(layout) = layout(len) else {
        return std::ptr::null_mut();
    };
    if !budget.reserve(len) {
        return std::ptr::null_mut();
    }
    // SAFETY: `layout` has a non-zero size. V8 asked for uninitialised memory and won't
    // read it before writing.
    let p = unsafe { alloc(layout) };
    if p.is_null() {
        budget.release(len);
    }
    p.cast()
}

unsafe extern "C" fn free(budget: &Budget, data: *mut c_void, len: usize) {
    if data.is_null() {
        return;
    }
    let Some(layout) = layout(len) else {
        return;
    };
    // SAFETY: V8 frees only what this allocator returned, with the length it asked for, so
    // `data` came from `alloc`/`alloc_zeroed` with this same layout.
    unsafe { dealloc(data.cast(), layout) };
    budget.release(len);
}

unsafe extern "C" fn drop_budget(budget: *const Budget) {
    // SAFETY: `budgeted` handed V8 one strong count from `Arc::into_raw`; V8 calls this
    // once, when it destroys the allocator.
    unsafe { drop(Arc::from_raw(budget)) };
}

static VTABLE: v8::RustAllocatorVtable<Budget> = v8::RustAllocatorVtable {
    allocate,
    allocate_uninitialized,
    free,
    drop: drop_budget,
};

/// An allocator that refuses to hold more than `limit` bytes at once, and its budget.
pub(crate) fn budgeted(limit: usize) -> (v8::SharedRef<v8::Allocator>, Arc<Budget>) {
    let budget = Arc::new(Budget {
        used: AtomicUsize::new(0),
        limit,
        refused: AtomicBool::new(false),
    });
    let handle = Arc::into_raw(budget.clone());
    // SAFETY: `handle` is a valid `Budget` that stays alive until V8 calls `drop_budget`,
    // and `VTABLE`'s functions all take a `&Budget`, matching it.
    let allocator = unsafe { v8::new_rust_allocator(handle, &VTABLE) };
    (allocator.make_shared(), budget)
}
