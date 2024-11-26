use std::sync::atomic::Ordering;
use std::{mem::ManuallyDrop, sync::atomic::AtomicU64};

use crate::hp_impl::{with_thread, Tagged, Thread};
use crate::RcObject;

/// Raw pointer to a reference counted object. Allows tagging.
pub(crate) type Raw<T> = Tagged<RcInner<T>>;

trait Deferable {
    unsafe fn defer_with_inner<T, F>(&self, ptr: *mut RcInner<T>, f: F)
    where
        F: FnOnce(*mut RcInner<T>);
}

impl Deferable for Thread {
    unsafe fn defer_with_inner<T, F>(&self, ptr: *mut RcInner<T>, f: F)
    where
        F: FnOnce(*mut RcInner<T>),
    {
        debug_assert!(!ptr.is_null());
        self.defer(ptr, || f(ptr));
    }
}

impl Deferable for Option<&Thread> {
    unsafe fn defer_with_inner<T, F>(&self, ptr: *mut RcInner<T>, f: F)
    where
        F: FnOnce(*mut RcInner<T>),
    {
        if let Some(thread) = self {
            thread.defer(ptr, || f(ptr));
        } else {
            with_thread(move |thread| thread.defer(ptr, move || f(ptr)));
        }
    }
}

const DESTRUCTED: u64 = 1 << (u64::BITS - 1);
const WEAKED: u64 = 1 << (u64::BITS - 2);
const STRONG: u64 = (1 << 31) - 1;
const WEAK: u64 = ((1 << 31) - 1) << 31;
const COUNT: u64 = 1;
const WEAK_COUNT: u64 = 1 << 31;

/// Effectively wraps the presence of destruction bits.
#[derive(Clone, Copy)]
struct State {
    inner: u64,
}

impl State {
    fn from_raw(inner: u64) -> Self {
        Self { inner }
    }

    fn strong(self) -> u32 {
        ((self.inner & STRONG) / COUNT) as u32
    }

    fn weak(self) -> u32 {
        ((self.inner & WEAK) / WEAK_COUNT) as u32
    }

    fn destructed(self) -> bool {
        (self.inner & DESTRUCTED) != 0
    }

    fn weaked(&self) -> bool {
        (self.inner & WEAKED) != 0
    }

    fn add_strong(self, val: u32) -> Self {
        Self::from_raw(self.inner + (val as u64) * COUNT)
    }

    fn add_weak(self, val: u32) -> Self {
        Self::from_raw(self.inner + (val as u64) * WEAK_COUNT)
    }

    fn with_destructed(self, dest: bool) -> Self {
        Self::from_raw((self.inner & !DESTRUCTED) | if dest { DESTRUCTED } else { 0 })
    }

    fn with_weaked(self, weaked: bool) -> Self {
        Self::from_raw((self.inner & !WEAKED) | if weaked { WEAKED } else { 0 })
    }

    fn as_raw(self) -> u64 {
        self.inner
    }
}

/// A reference-counted object of type `T` with an atomic reference counts.
pub struct RcInner<T> {
    storage: ManuallyDrop<T>,
    state: AtomicU64,
}

impl<T> RcInner<T> {
    #[inline(always)]
    pub(crate) fn alloc(obj: T, init_strong: u32) -> *mut Self {
        let obj = Self {
            storage: ManuallyDrop::new(obj),
            state: AtomicU64::new((init_strong as u64) * COUNT + WEAK_COUNT),
        };
        Box::into_raw(Box::new(obj))
    }

    /// # Safety
    ///
    /// The given `ptr` must not be shared across more than one thread.
    pub(crate) unsafe fn dealloc(ptr: *mut Self) {
        drop(Box::from_raw(ptr));
    }

    /// Returns an immutable reference to the object.
    pub fn data(&self) -> &T {
        &self.storage
    }

    /// Returns a mutable reference to the object.
    pub fn data_mut(&mut self) -> &mut T {
        &mut self.storage
    }

    #[inline]
    pub(crate) fn increment_strong(&self) -> bool {
        let val = State::from_raw(self.state.fetch_add(COUNT, Ordering::SeqCst));
        if val.destructed() {
            return false;
        }
        if val.strong() == 0 {
            // The previous fetch_add created a permission to run decrement again.
            // Now create an actual reference.
            self.state.fetch_add(COUNT, Ordering::SeqCst);
        }
        true
    }

    #[inline]
    unsafe fn try_dealloc(ptr: *mut Self) {
        if State::from_raw((*ptr).state.load(Ordering::SeqCst)).weak() > 0 {
            Self::decrement_weak(ptr, None);
        } else {
            Self::dealloc(ptr);
        }
    }

    #[inline]
    pub(crate) fn increment_weak(&self, count: u32) {
        let mut old = State::from_raw(self.state.load(Ordering::SeqCst));
        while !old.weaked() {
            // In this case, `increment_weak` must have been called from `Rc::downgrade`,
            // guaranteeing weak > 0, so it can’t be incremented from 0.
            debug_assert!(old.weak() != 0);
            match self.state.compare_exchange(
                old.as_raw(),
                old.with_weaked(true).add_weak(count).as_raw(),
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return,
                Err(curr) => old = State::from_raw(curr),
            }
        }
        if State::from_raw(
            self.state
                .fetch_add(count as u64 * WEAK_COUNT, Ordering::SeqCst),
        )
        .weak()
            == 0
        {
            self.state.fetch_add(WEAK_COUNT, Ordering::SeqCst);
        }
    }

    #[inline]
    pub(crate) unsafe fn decrement_weak(ptr: *mut Self, guard: Option<&Thread>) {
        debug_assert!(State::from_raw((*ptr).state.load(Ordering::SeqCst)).weak() >= 1);
        if State::from_raw((*ptr).state.fetch_sub(WEAK_COUNT, Ordering::SeqCst)).weak() == 1 {
            guard.defer_with_inner(ptr, |inner| Self::try_dealloc(inner));
        }
    }

    #[inline]
    pub(crate) fn is_not_destructed(&self) -> bool {
        let mut old = State::from_raw(self.state.load(Ordering::SeqCst));
        while !old.destructed() && old.strong() == 0 {
            match self.state.compare_exchange(
                old.as_raw(),
                old.add_strong(1).as_raw(),
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return true,
                Err(curr) => old = State::from_raw(curr),
            }
        }
        !old.destructed()
    }
}

impl<T: RcObject> RcInner<T> {
    #[inline]
    pub(crate) unsafe fn decrement_strong(ptr: *mut Self, count: u32, guard: Option<&Thread>) {
        let count = count as u64 * COUNT;
        if (*ptr).state.fetch_sub(count, Ordering::SeqCst) & STRONG == count {
            guard.defer_with_inner(ptr, |inner| Self::try_destruct(inner));
        }
    }

    #[inline]
    unsafe fn try_destruct(ptr: *mut Self) {
        let mut old = State::from_raw((*ptr).state.load(Ordering::SeqCst));
        debug_assert!(!old.destructed());
        loop {
            if old.strong() > 0 {
                Self::decrement_strong(ptr, 1, None);
                return;
            }
            match (*ptr).state.compare_exchange(
                old.as_raw(),
                old.with_destructed(true).as_raw(),
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                // Note that `decrement_weak` will be called in `dispose`.
                Ok(_) => {
                    ManuallyDrop::drop(&mut (*ptr).storage);
                    if !old.weaked() {
                        Self::dealloc(ptr);
                    } else {
                        Self::decrement_weak(ptr, None);
                    }
                    return;
                }
                Err(curr) => old = State::from_raw(curr),
            }
        }
    }
}
