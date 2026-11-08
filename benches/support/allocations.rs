use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

pub struct CountingAllocator;

#[derive(Clone, Copy, Default, Debug)]
pub struct Counts {
    pub allocations: usize,
    pub bytes: usize,
    pub live: usize,
    pub peak: usize,
}

thread_local! {
    static COUNTS: Cell<Option<Counts>> = const { Cell::new(None) };
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            COUNTS.with(|counter| {
                if let Some(mut counts) = counter.get() {
                    counts.allocations += 1;
                    counts.bytes += layout.size();
                    counts.live += layout.size();
                    counts.peak = counts.peak.max(counts.live);
                    counter.set(Some(counts));
                }
            });
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        COUNTS.with(|counter| {
            if let Some(mut counts) = counter.get() {
                counts.live = counts.live.saturating_sub(layout.size());
                counter.set(Some(counts));
            }
        });
        unsafe { System.dealloc(pointer, layout) };
    }
}

// Count only this thread; the operation must release its own allocations.
pub fn measure(operation: impl FnOnce()) -> Counts {
    COUNTS.with(|counter| counter.set(Some(Counts::default())));
    operation();
    COUNTS.with(|counter| counter.replace(None).unwrap())
}
