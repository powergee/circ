use std::mem;

use atomic::Ordering;

use crate::internal::utils::RcInner;
use crate::internal::{Acquired, Cs, TaggedCnt};

/// A tagged pointer which is pointing a `CountedObjPtr<T>`.
///
/// We may want to use `crossbeam_ebr::Shared` as a `Acquired`,
/// but trait interfaces can be complicated because `crossbeam_ebr::Shared`
/// requires to specify a lifetime specifier.
pub struct AcquiredEBR<T>(TaggedCnt<T>);

impl<T> Acquired<T> for AcquiredEBR<T> {
    #[inline(always)]
    fn as_ptr(&self) -> TaggedCnt<T> {
        self.0
    }

    #[inline(always)]
    fn null() -> Self {
        Self(TaggedCnt::null())
    }

    #[inline(always)]
    fn is_null(&self) -> bool {
        self.0.is_null()
    }

    #[inline(always)]
    fn swap(p1: &mut Self, p2: &mut Self) {
        mem::swap(p1, p2);
    }

    #[inline(always)]
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }

    #[inline]
    fn clear(&mut self) {
        self.0 = TaggedCnt::null();
    }

    #[inline]
    fn set_tag(&mut self, tag: usize) {
        self.0 = self.0.with_tag(tag);
    }
}

pub struct CsEBR {
    guard: Option<crossbeam::epoch::Guard>,
}

impl From<crossbeam::epoch::Guard> for CsEBR {
    #[inline(always)]
    fn from(guard: crossbeam::epoch::Guard) -> Self {
        Self { guard: Some(guard) }
    }
}

impl Cs for CsEBR {
    type RawShield<T> = AcquiredEBR<T>;

    #[inline(always)]
    fn new() -> Self {
        Self::from(crossbeam::epoch::pin())
    }

    #[inline]
    unsafe fn unprotected() -> Self {
        Self { guard: None }
    }

    #[inline(always)]
    fn create_object<T>(obj: T) -> *mut RcInner<T> {
        let obj = RcInner::new(obj);
        Box::into_raw(Box::new(obj))
    }

    #[inline]
    unsafe fn own_object<T>(ptr: *mut RcInner<T>) -> RcInner<T> {
        *Box::from_raw(ptr)
    }

    #[inline(always)]
    fn reserve<T>(&self, ptr: TaggedCnt<T>, shield: &mut Self::RawShield<T>) {
        *shield = AcquiredEBR(ptr);
    }

    #[inline(always)]
    fn acquire<T>(
        &self,
        link: &atomic::Atomic<TaggedCnt<T>>,
        shield: &mut Self::RawShield<T>,
    ) -> TaggedCnt<T> {
        let ptr = link.load(Ordering::Acquire);
        *shield = AcquiredEBR(ptr);
        ptr
    }

    #[inline(always)]
    unsafe fn defer<T, F>(&self, ptr: *mut RcInner<T>, f: F)
    where
        F: FnOnce(&mut RcInner<T>),
    {
        debug_assert!(!ptr.is_null());
        let cnt = &mut *ptr;
        if let Some(guard) = &self.guard {
            guard.defer_unchecked(move || f(cnt));
        } else {
            f(cnt);
        }
    }

    #[inline]
    fn clear(&mut self) {
        if let Some(guard) = &mut self.guard {
            guard.repin();
        }
    }
}
