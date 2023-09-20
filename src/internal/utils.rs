use core::mem;
use std::{
    mem::ManuallyDrop,
    ptr::null_mut,
    sync::atomic::{AtomicU32, Ordering},
};

use crate::Cs;

/// An instance of an object of type T with an atomic reference count.
pub struct RcInner<T> {
    storage: ManuallyDrop<T>,
    pub(crate) strong: AtomicU32,
}

impl<T> RcInner<T> {
    pub(crate) fn new(val: T) -> Self {
        Self {
            storage: ManuallyDrop::new(val),
            strong: AtomicU32::new(1),
        }
    }

    pub(crate) fn data(&self) -> &T {
        &self.storage
    }

    pub(crate) fn data_mut(&mut self) -> &mut T {
        &mut self.storage
    }

    pub(crate) unsafe fn dispose(&mut self) {
        ManuallyDrop::drop(&mut self.storage)
    }

    pub(crate) fn increment_strong(&self) {
        if self.strong.fetch_add(1, Ordering::SeqCst) == 0 {
            // Create a permission to run decrement again.
            self.strong.fetch_add(1, Ordering::SeqCst);
        }
    }

    pub(crate) unsafe fn decrement_strong<C: Cs>(&mut self, cs: Option<&C>) {
        if self.strong.fetch_sub(1, Ordering::SeqCst) == 1 {
            if let Some(cs) = cs {
                cs.defer(self, |inner| unsafe { inner.try_zero::<C>() })
            } else {
                C::new().defer(self, |inner| unsafe { inner.try_zero::<C>() })
            }
        }
    }

    pub(crate) unsafe fn try_zero<C: Cs>(&mut self) {
        if self.strong.fetch_add(0, Ordering::SeqCst) == 0 {
            // In strong-only, at this point, there can’t be guard for this pointer anymore
            // (no zero set needed)
            self.dispose();
            C::delete_object(self);
        } else {
            self.decrement_strong::<C>(None);
        }
    }
}

pub struct Tagged<T> {
    ptr: *mut T,
}

impl<T> Default for Tagged<T> {
    fn default() -> Self {
        Self { ptr: null_mut() }
    }
}

impl<T> Clone for Tagged<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Tagged<T> {}

impl<T> PartialEq for Tagged<T> {
    fn eq(&self, other: &Self) -> bool {
        self.ptr == other.ptr
    }
}

impl<T> Tagged<T> {
    pub fn new(ptr: *mut T) -> Self {
        Self { ptr }
    }

    pub fn null() -> Self {
        Self { ptr: null_mut() }
    }

    pub fn is_null(&self) -> bool {
        self.as_raw().is_null()
    }

    pub fn tag(&self) -> usize {
        let ptr = self.ptr as usize;
        ptr & low_bits::<T>()
    }

    /// Converts the pointer to a raw pointer (without the tag).
    pub fn as_raw(&self) -> *mut T {
        let ptr = self.ptr as usize;
        (ptr & !low_bits::<T>()) as *mut T
    }

    pub fn with_tag(&self, tag: usize) -> Self {
        Self::new(with_tag(self.ptr, tag))
    }

    pub unsafe fn deref<'g>(&self) -> &'g T {
        &*self.as_raw()
    }

    pub unsafe fn deref_mut<'g>(&mut self) -> &'g mut T {
        &mut *self.as_raw()
    }
}

/// Returns a bitmask containing the unused least significant bits of an aligned pointer to `T`.
const fn low_bits<T>() -> usize {
    (1 << mem::align_of::<T>().trailing_zeros()) - 1
}

/// Returns the pointer with the given tag
fn with_tag<T>(ptr: *mut T, tag: usize) -> *mut T {
    ((ptr as usize & !low_bits::<T>()) | (tag & low_bits::<T>())) as *mut T
}

pub type TaggedCnt<T> = Tagged<RcInner<T>>;

pub trait Pointer<T> {
    fn as_ptr(&self) -> TaggedCnt<T>;
    fn is_null(&self) -> bool {
        self.as_ptr().is_null()
    }
}