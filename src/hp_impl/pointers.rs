use core::hash::Hash;
use core::mem::align_of;
use core::ptr::null_mut;
use std::fmt::{Debug, Formatter, Pointer};

pub struct Tagged<T: ?Sized> {
    ptr: *mut T,
}

impl<T> Debug for Tagged<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        Pointer::fmt(&self.as_ptr(), f)
    }
}

impl<T> Pointer for Tagged<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        Pointer::fmt(&self.as_ptr(), f)
    }
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

impl<T> Hash for Tagged<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.ptr.hash(state)
    }
}

impl<T> From<*const T> for Tagged<T> {
    fn from(value: *const T) -> Self {
        Self {
            ptr: value.cast_mut(),
        }
    }
}

impl<T> From<*mut T> for Tagged<T> {
    fn from(value: *mut T) -> Self {
        Self { ptr: value }
    }
}

impl<T> Tagged<T> {
    pub fn null() -> Self {
        Self { ptr: null_mut() }
    }

    pub fn is_null(&self) -> bool {
        self.as_ptr().is_null()
    }

    pub fn tag(&self) -> usize {
        let ptr = self.ptr as usize;
        ptr & low_bits::<T>()
    }

    /// Converts the pointer to a raw pointer (without the tag).
    pub fn as_ptr(&self) -> *mut T {
        let ptr = self.ptr as usize;
        (ptr & !low_bits::<T>()) as *mut T
    }

    pub fn with_tag(&self, tag: usize) -> Self {
        Self::from(with_tag(self.ptr, tag))
    }

    /// # Safety
    ///
    /// The pointer (without tag bits) must be a valid location to dereference.
    pub unsafe fn deref<'g>(&self) -> &'g T {
        &*self.as_ptr()
    }

    /// # Safety
    ///
    /// The pointer (without tag bits) must be a valid location to dereference.
    pub unsafe fn deref_mut<'g>(&mut self) -> &'g mut T {
        &mut *self.as_ptr()
    }

    /// # Safety
    ///
    /// The pointer (without tag bits) must be a valid location to dereference.
    pub unsafe fn as_ref<'g>(&self) -> Option<&'g T> {
        if self.is_null() {
            None
        } else {
            Some(self.deref())
        }
    }

    /// Returns `true` if the two pointer values, including the tag values set by `with_tag`,
    /// are identical.
    pub fn ptr_eq(self, other: Self) -> bool {
        self.ptr == other.ptr
    }
}

/// Returns a bitmask containing the unused least significant bits of an aligned pointer to `T`.
const fn low_bits<T>() -> usize {
    (1 << align_of::<T>().trailing_zeros()) - 1
}

/// Returns the pointer with the given tag
fn with_tag<T>(ptr: *mut T, tag: usize) -> *mut T {
    ((ptr as usize & !low_bits::<T>()) | (tag & low_bits::<T>())) as *mut T
}
