use std::{
    array,
    fmt::{Debug, Formatter, Pointer},
    hash::{Hash, Hasher},
    marker::PhantomData,
    mem::{forget, size_of, take, transmute},
    sync::atomic::{AtomicUsize, Ordering},
};

use atomic::Atomic;
use static_assertions::const_assert;

use crate::ebr_impl::{global_epoch, Guard, Tagged};
use crate::utils::{try_ird_with_raw, DisposeContext, Raw, RcInner};
use crate::{Weak, WeakSnapshot};

/// A common trait for reference-counted object types.
///
/// This trait enables *immediate recursive destruction*,
/// which identifies the outgoing edges of the reclaiming nodes and
/// recursively destructs the subsequent chain of unreachable nodes.
///
/// # Examples
///
/// ```
/// use circ::{AtomicRc, RcObject, Rc, EdgeTaker};
///
/// // A simple singly linked list node.
/// struct ListNode {
///     item: usize,
///     next: AtomicRc<Self>,
/// }
///
/// unsafe impl RcObject for ListNode {
///     fn pop_edges(&mut self, out: &mut EdgeTaker<'_>) {
///         out.take(&mut self.next);
///     }
/// }
///
/// // A tree node with two children.
/// struct TreeNode {
///     item: usize,
///     left: AtomicRc<Self>,
///     right: AtomicRc<Self>,
/// }
///
/// unsafe impl RcObject for TreeNode {
///     fn pop_edges(&mut self, out: &mut EdgeTaker<'_>) {
///         out.take(&mut self.left);
///         out.take(&mut self.right);
///     }
/// }
/// ```
///
/// # Safety
///
/// `out` should take the `AtomicRc`s and `Rc`s obtained from only the given object.
/// If an unrelated `Rc` is added, its referent can be prematurely reclaimed.
pub unsafe trait RcObject: Sized {
    /// Takes all `AtomicRc`s and `Rc`s in the object by calling the `take` method of `out`.
    ///
    /// This method is called by CIRC just before the object is destructed.
    ///
    /// It does not need to take all the edges in the node, because the destructors of `Rc` and
    /// `AtomicRc` schedule the decrement and destruction anyway. However, it may
    /// impact performance and memory usage, especially if the structure forms a long chain.
    fn pop_edges(&mut self, out: &mut EdgeTaker<'_>);
}

pub(crate) struct TryIRD {
    rc: Raw<()>,
    ird: unsafe fn(Raw<()>, DisposeContext, u32),
}

impl TryIRD {
    pub(crate) unsafe fn try_ird(self, ctx: DisposeContext<'_>, succ_epoch: u32) {
        (self.ird)(self.rc, ctx, succ_epoch)
    }
}

pub struct EdgeTaker<'r> {
    popped: &'r mut Vec<TryIRD>,
}

impl<'r> EdgeTaker<'r> {
    pub(crate) fn new(popped: &'r mut Vec<TryIRD>) -> Self {
        Self { popped }
    }

    /// Takes an underlying [`Rc`] from `outgoing` edge, and stores it in a local buffer.
    /// The taken [`Rc`]s will be efficiently destructed by CIRC.
    pub fn take<T: RcObject>(&mut self, outgoing: &mut impl OwnRc<T>) {
        let rc = outgoing.take().into_raw();
        self.popped.push(TryIRD {
            rc: unsafe { transmute::<Raw<T>, Raw<()>>(rc) },
            ird: try_ird_with_raw::<T>,
        });
    }
}

/// A trait for types owning a strong reference count.
pub trait OwnRc<T: RcObject> {
    /// Takes an underlying [`Rc`] from this object, leaving a null pointer.
    fn take(&mut self) -> Rc<T>;
}

impl<T> Tagged<RcInner<T>> {
    fn with_timestamp(self) -> Self {
        if self.is_null() {
            self
        } else {
            self.with_high_tag(global_epoch())
        }
    }
}

/// Result of a failed `compare_exchange` operation.
///
/// It returns the ownership of the pointer which was given as a parameter `desired`.
pub struct CompareExchangeError<P, S> {
    /// The desired value that was passed to `compare_exchange`.
    pub desired: P,
    /// The current pointer value inside the atomic pointer.
    pub current: S,
}

/// A thread-safe (atomic) mutable memory location that contains an [`Rc<T>`].
///
/// The pointer must be properly aligned. Since it is aligned, a tag can be stored into the unused
/// least significant bits of the address. For example, the tag for a pointer to a sized type `T`
/// should be less than `(1 << align_of::<T>().trailing_zeros())`.
pub struct AtomicRc<T: RcObject> {
    link: Atomic<Raw<T>>,
    _marker: PhantomData<T>,
}

unsafe impl<T: RcObject + Send + Sync> Send for AtomicRc<T> {}
unsafe impl<T: RcObject + Send + Sync> Sync for AtomicRc<T> {}

// Ensure that TaggedPtr<T> is 8-byte long,
// so that lock-free atomic operations are possible.
const_assert!(Atomic::<Raw<u8>>::is_lock_free());
const_assert!(size_of::<Raw<u8>>() == size_of::<usize>());
const_assert!(size_of::<Atomic<Raw<u8>>>() == size_of::<AtomicUsize>());

impl<T: RcObject> AtomicRc<T> {
    /// Constructs a new `AtomicRc` by allocating a new reference-couned object.
    #[inline(always)]
    pub fn new(obj: T) -> Self {
        Self {
            link: Atomic::new(Rc::<T>::new(obj).into_raw()),
            _marker: PhantomData,
        }
    }

    /// Constructs a new `AtomicRc` containing a null pointer.
    #[inline(always)]
    pub fn null() -> Self {
        Self {
            link: Atomic::new(Tagged::null()),
            _marker: PhantomData,
        }
    }

    /// Loads a [`Snapshot`] pointer from this `AtomicRc`.
    ///
    /// This method takes an [`Ordering`] argument which describes the memory ordering of this
    /// operation. Possible values are `SeqCst`, `Acquire` and `Relaxed`.
    ///
    /// # Panics
    ///
    /// Panics if `order` is `Release` or `AcqRel`.
    #[inline]
    pub fn load<'g>(&self, order: Ordering, guard: &'g Guard) -> Snapshot<'g, T> {
        Snapshot::from_raw(self.link.load(order), guard)
    }

    /// Stores an [`Rc`] pointer into this `AtomicRc`.
    ///
    /// This method takes an [`Ordering`] argument which describes the memory ordering of
    /// this operation.
    #[inline]
    pub fn store(&self, ptr: Rc<T>, order: Ordering, guard: &Guard) {
        let new_ptr = ptr.ptr;
        let old_ptr = self.link.swap(new_ptr.with_timestamp(), order);
        // Skip decrementing a strong count of the inserted pointer.
        forget(ptr);
        unsafe {
            // Did not use `Rc::drop`, to reuse the given `guard`.
            if let Some(cnt) = old_ptr.as_raw().as_mut() {
                RcInner::decrement_strong(cnt, 1, Some(guard));
            }
        }
    }

    /// Stores a [`Snapshot`] or [`Rc`] pointer into this `AtomicRc`,
    /// returning the previous [`Rc`].
    ///
    /// This method takes an [`Ordering`] argument which describes the memory ordering of
    /// this operation.
    #[inline(always)]
    pub fn swap(&self, new: Rc<T>, order: Ordering) -> Rc<T> {
        let new_ptr = new.into_raw();
        let old_ptr = self.link.swap(new_ptr.with_timestamp(), order);
        Rc::from_raw(old_ptr)
    }

    /// Stores the [`Rc`] pointer `desired` into the atomic pointer if the current value is the
    /// same as `expected` [`Snapshot`] pointer. The tag is also taken into account,
    /// so two pointers to the same object, but with different tags, will not be considered equal.
    ///
    /// The return value is a result indicating whether the desired pointer was written.
    /// On success the pointer that was in this `AtomicRc` is returned.
    /// On failure the actual current value and `desired` are returned.
    ///
    /// This method takes two [`Ordering`] arguments to describe the memory
    /// ordering of this operation. `success` describes the required ordering for the
    /// read-modify-write operation that takes place if the comparison with `expected` succeeds.
    /// `failure` describes the required ordering for the load operation that takes place when
    /// the comparison fails. Using `Acquire` as success ordering makes the store part
    /// of this operation `Relaxed`, and using `Release` makes the successful load
    /// `Relaxed`. The failure ordering can only be `SeqCst`, `Acquire` or `Relaxed`
    /// and must be equivalent to or weaker than the success ordering.
    #[inline(always)]
    pub fn compare_exchange<'g>(
        &self,
        expected: Snapshot<'g, T>,
        desired: Rc<T>,
        success: Ordering,
        failure: Ordering,
        guard: &'g Guard,
    ) -> Result<Rc<T>, CompareExchangeError<Rc<T>, Snapshot<'g, T>>> {
        let mut expected_raw = expected.ptr;
        let desired_raw = desired.ptr.with_timestamp();
        loop {
            match self
                .link
                .compare_exchange(expected_raw, desired_raw, success, failure)
            {
                Ok(_) => {
                    // Skip decrementing a strong count of the inserted pointer.
                    forget(desired);
                    let rc = Rc::from_raw(expected_raw);
                    return Ok(rc);
                }
                Err(current_raw) => {
                    if current_raw.ptr_eq(expected_raw) {
                        expected_raw = current_raw;
                    } else {
                        let current = Snapshot::from_raw(current_raw, guard);
                        return Err(CompareExchangeError { desired, current });
                    }
                }
            }
        }
    }

    /// Stores the [`Rc`] pointer `desired` into the atomic pointer if the current value is the
    /// same as `expected` [`Snapshot`] pointer. The tag is also taken into account,
    /// so two pointers to the same object, but with different tags, will not be considered equal.
    ///
    /// Unlike [`AtomicRc::compare_exchange`], this method is allowed to spuriously fail
    /// even when comparison succeeds, which can result in more efficient code on some platforms.
    /// The return value is a result indicating whether the desired pointer was written.
    /// On success the pointer that was in this `AtomicRc` is returned.
    /// On failure the actual current value and `desired` are returned.
    ///
    /// This method takes two [`Ordering`] arguments to describe the memory
    /// ordering of this operation. `success` describes the required ordering for the
    /// read-modify-write operation that takes place if the comparison with `expected` succeeds.
    /// `failure` describes the required ordering for the load operation that takes place when
    /// the comparison fails. Using `Acquire` as success ordering makes the store part
    /// of this operation `Relaxed`, and using `Release` makes the successful load
    /// `Relaxed`. The failure ordering can only be `SeqCst`, `Acquire` or `Relaxed`
    /// and must be equivalent to or weaker than the success ordering.
    #[inline(always)]
    pub fn compare_exchange_weak<'g>(
        &self,
        expected: Snapshot<'g, T>,
        desired: Rc<T>,
        success: Ordering,
        failure: Ordering,
        guard: &'g Guard,
    ) -> Result<Rc<T>, CompareExchangeError<Rc<T>, Snapshot<'g, T>>> {
        let mut expected_raw = expected.ptr;
        let desired_raw = desired.ptr.with_timestamp();
        loop {
            match self
                .link
                .compare_exchange_weak(expected_raw, desired_raw, success, failure)
            {
                Ok(_) => {
                    // Skip decrementing a strong count of the inserted pointer.
                    forget(desired);
                    let rc = Rc::from_raw(expected_raw);
                    return Ok(rc);
                }
                Err(current_raw) => {
                    if current_raw.ptr_eq(expected_raw) {
                        expected_raw = current_raw;
                    } else {
                        let current = Snapshot::from_raw(current_raw, guard);
                        return Err(CompareExchangeError { desired, current });
                    }
                }
            }
        }
    }

    /// Overwrites the tag value `desired_tag` to the atomic pointer if the current value is the
    /// same as `expected` [`Snapshot`] pointer. The tag is also taken into account,
    /// so two pointers to the same object, but with different tags, will not be considered equal.
    ///
    /// If the `desired_tag` uses more bits than the unused least significant bits of the pointer
    /// to `T`, it will be truncated to be fit.
    ///
    /// The return value is a result indicating whether the desired pointer was written.
    /// On success the pointer that was in this `AtomicRc` is returned.
    /// On failure the actual current value and a desired pointer to write are returned.
    /// For both cases, the ownership of `expected` is returned by a dedicated field.
    ///
    /// This method takes two [`Ordering`] arguments to describe the memory
    /// ordering of this operation. `success` describes the required ordering for the
    /// read-modify-write operation that takes place if the comparison with `expected` succeeds.
    /// `failure` describes the required ordering for the load operation that takes place when
    /// the comparison fails. Using `Acquire` as success ordering makes the store part
    /// of this operation `Relaxed`, and using `Release` makes the successful load
    /// `Relaxed`. The failure ordering can only be `SeqCst`, `Acquire` or `Relaxed`
    /// and must be equivalent to or weaker than the success ordering.
    ///
    /// [`AtomicRc::compare_exchange`] subsumes this method, but it is more efficient because it
    /// does not require [`Rc`] as `desired`.
    #[inline]
    pub fn compare_exchange_tag<'g>(
        &self,
        expected: Snapshot<'g, T>,
        desired_tag: usize,
        success: Ordering,
        failure: Ordering,
        guard: &'g Guard,
    ) -> Result<Snapshot<'g, T>, CompareExchangeError<Snapshot<'g, T>, Snapshot<'g, T>>> {
        let mut expected_raw = expected.ptr;
        let desired_raw = expected_raw.with_tag(desired_tag).with_timestamp();
        loop {
            match self
                .link
                .compare_exchange(expected_raw, desired_raw, success, failure)
            {
                Ok(current_raw) => return Ok(Snapshot::from_raw(current_raw, guard)),
                Err(current_raw) => {
                    if current_raw.ptr_eq(expected_raw) {
                        expected_raw = current_raw;
                    } else {
                        return Err(CompareExchangeError {
                            desired: Snapshot::from_raw(desired_raw, guard),
                            current: Snapshot::from_raw(current_raw, guard),
                        });
                    }
                }
            }
        }
    }

    // get_mut is unsound, because it allows writing ref without link epoch.
    // Consider the motivating 3-thread example where
    // * T1 @e+1 loads node1
    // * T2 unlink node1 @e
    // * T3 @e+2 makes node2 and sends Rc to T1
    // * T1 installs node2 Rc in node1, exits CS
    // * node1 is destructed @e+3
    // ... Or is it actually fine T1 can't have &mut of node1?
    //
    // /// Returns a mutable reference to the stored `Rc`.
    // ///
    // /// This is safe because the mutable reference guarantees that no other threads are
    // /// concurrently accessing.
    // pub fn get_mut(&mut self) -> &mut Rc<T> {
    //     unsafe { core::mem::transmute(self.link.get_mut()) }
    // }
}

impl<T: RcObject> OwnRc<T> for AtomicRc<T> {
    #[inline]
    fn take(&mut self) -> Rc<T> {
        Rc::from_raw(take(self.link.get_mut()))
    }
}

impl<T: RcObject> Drop for AtomicRc<T> {
    #[inline(always)]
    fn drop(&mut self) {
        let ptr = (*self.link.get_mut()).as_raw();
        unsafe {
            if let Some(cnt) = ptr.as_mut() {
                RcInner::decrement_strong(cnt, 1, None);
            }
        }
    }
}

impl<T: RcObject> Default for AtomicRc<T> {
    #[inline(always)]
    fn default() -> Self {
        Self::null()
    }
}

impl<T: RcObject> From<Rc<T>> for AtomicRc<T> {
    #[inline]
    fn from(value: Rc<T>) -> Self {
        let ptr = value.into_raw();
        Self {
            link: Atomic::new(ptr),
            _marker: PhantomData,
        }
    }
}

impl<T: RcObject> From<&Rc<T>> for AtomicRc<T> {
    #[inline]
    fn from(value: &Rc<T>) -> Self {
        Self::from(value.clone())
    }
}

impl<T: RcObject> Debug for AtomicRc<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        Debug::fmt(&self.link.load(Ordering::Relaxed), f)
    }
}

impl<T: RcObject> Pointer for AtomicRc<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        Pointer::fmt(&self.link.load(Ordering::Relaxed), f)
    }
}

/// A reference-counted pointer to an object of type `T`.
///
/// When `T` implements [`Send`] and [`Sync`], [`Rc<T>`] also implements these traits.
///
/// The pointer must be properly aligned. Since it is aligned, a tag can be stored into the unused
/// least significant bits of the address. For example, the tag for a pointer to a sized type `T`
/// should be less than `(1 << align_of::<T>().trailing_zeros())`.
pub struct Rc<T: RcObject> {
    ptr: Raw<T>,
    _marker: PhantomData<T>,
}

unsafe impl<T: RcObject + Send + Sync> Send for Rc<T> {}
unsafe impl<T: RcObject + Send + Sync> Sync for Rc<T> {}

impl<T: RcObject> Clone for Rc<T> {
    fn clone(&self) -> Self {
        let rc = Self {
            ptr: self.ptr,
            _marker: PhantomData,
        };
        unsafe {
            if let Some(cnt) = rc.ptr.as_raw().as_ref() {
                cnt.increment_strong();
            }
        }
        rc
    }
}

impl<T: RcObject> Rc<T> {
    /// Constructs a null `Rc` pointer.
    #[inline(always)]
    pub fn null() -> Self {
        Self::from_raw(Raw::null())
    }

    /// Returns `true` if the pointer is null ignoring the tag.
    #[inline(always)]
    pub fn is_null(&self) -> bool {
        self.ptr.is_null()
    }

    #[inline(always)]
    pub(crate) fn from_raw(ptr: Raw<T>) -> Self {
        Self {
            ptr,
            _marker: PhantomData,
        }
    }

    /// Constructs a new `Rc` by allocating a new reference-couned object.
    #[inline(always)]
    pub fn new(obj: T) -> Self {
        let ptr = RcInner::alloc(obj, 1);
        Self {
            ptr: Raw::from(ptr),
            _marker: PhantomData,
        }
    }

    /// Constructs multiple [`Rc`]s that point to the same object,
    /// which is allocated as a new reference-counted object.
    ///
    /// This method is more efficient than calling [`Rc::new`] once and cloning multiple times
    /// because it is sufficient to set the reference counter only once, avoiding expensive
    /// read-modify-write operations.
    #[inline(always)]
    pub fn new_many<const N: usize>(obj: T) -> [Self; N] {
        let ptr = RcInner::alloc(obj, N as _);
        [(); N].map(|_| Self {
            ptr: Raw::from(ptr),
            _marker: PhantomData,
        })
    }

    /// Constructs an iterator that produces the [`Rc`]s that point to the same object,
    /// which is allocated as a new reference-counted object.
    ///
    /// This method is more efficient than calling [`Rc::new`] once and cloning multiple times
    /// because it is sufficient to set the reference counter only once, avoiding expensive
    /// read-modify-write operations.
    #[inline(always)]
    pub fn new_many_iter(obj: T, count: usize) -> NewRcIter<T> {
        let ptr = RcInner::alloc(obj, count as _);
        NewRcIter {
            remain: count,
            ptr: Raw::from(ptr),
        }
    }

    /// Constructs multiple [`Weak`]s that point to the current object.
    ///
    /// This method is more efficient than calling [`Rc::downgrade`] multiple times
    /// because it is sufficient to set the reference counter only once, avoiding expensive
    /// read-modify-write operations.
    #[inline]
    pub fn weak_many<const N: usize>(&self) -> [Weak<T>; N] {
        if let Some(cnt) = unsafe { self.ptr.as_raw().as_ref() } {
            cnt.increment_weak(N as u32);
        }
        array::from_fn(|_| Weak::null())
    }

    /// Returns the tag stored within the pointer.
    #[inline(always)]
    pub fn tag(&self) -> usize {
        self.ptr.tag()
    }

    /// Returns the same pointer, but tagged with `tag`. `tag` is truncated to be fit into the
    /// unused bits of the pointer to `T`.
    #[inline(always)]
    pub fn with_tag(mut self, tag: usize) -> Self {
        self.ptr = self.ptr.with_tag(tag);
        self
    }

    #[inline]
    pub(crate) fn into_raw(self) -> Raw<T> {
        let new_ptr = self.ptr;
        // Skip decrementing the ref count.
        forget(self);
        new_ptr
    }

    /// Consumes this pointer and release a strong reference count it was owning.
    ///
    /// This method is more efficient than just `Drop`ing the pointer. The `Drop` method
    /// checks whether the current thread is pinned and pin the thread if it is not.
    /// However, this method skips that procedure as it already requires `Guard` as an argument.
    #[inline]
    pub fn finalize(self, guard: &Guard) {
        unsafe {
            if let Some(cnt) = self.ptr.as_raw().as_mut() {
                RcInner::decrement_strong(cnt, 1, Some(guard));
            }
        }
        forget(self);
    }

    /// Creates a [`Weak`] pointer by incrementing the weak reference counter.
    #[inline]
    pub fn downgrade(&self) -> Weak<T> {
        unsafe {
            if let Some(cnt) = self.ptr.as_raw().as_ref() {
                cnt.increment_weak(1);
                return Weak::from_raw(self.ptr);
            }
        }
        Weak::from_raw(self.ptr)
    }

    /// Creates a [`Snapshot`] pointer to the same object.
    #[inline]
    pub fn snapshot<'g>(&self, guard: &'g Guard) -> Snapshot<'g, T> {
        Snapshot::from_raw(self.ptr, guard)
    }

    /// Dereferences the pointer and returns an immutable reference.
    ///
    /// It does not check whether the pointer is null.
    ///
    /// # Safety
    ///
    /// The pointer must be a valid memory location to dereference.
    #[inline]
    pub unsafe fn deref(&self) -> &T {
        self.ptr.deref().data()
    }

    /// Dereferences the pointer and returns a mutable reference.
    ///
    /// It does not check whether the pointer is null.
    ///
    /// # Safety
    ///
    /// The pointer must be a valid memory location to dereference and
    /// other threads must not have references to the object.
    #[inline]
    pub unsafe fn deref_mut(&mut self) -> &mut T {
        self.ptr.deref_mut().data_mut()
    }

    /// Dereferences the pointer and returns an immutable reference if it is not null.
    #[inline]
    pub fn as_ref(&self) -> Option<&T> {
        if self.ptr.is_null() {
            None
        } else {
            Some(unsafe { self.deref() })
        }
    }

    /// Dereferences the pointer and returns a mutable reference if it is not null.
    ///
    /// # Safety
    ///
    /// Other threads must not have references to the object.
    #[inline]
    pub unsafe fn as_mut(&mut self) -> Option<&mut T> {
        if self.ptr.is_null() {
            None
        } else {
            Some(unsafe { self.deref_mut() })
        }
    }

    /// Returns `true` if the two pointer values, including the tag values set by `with_tag`,
    /// are identical.
    #[inline]
    pub fn ptr_eq(&self, other: &Self) -> bool {
        // Instead of using a direct equality comparison (`==`), we use `ptr_eq`, which ignores
        // the epoch tag in the high bits. This is because the epoch tags hold no significance
        // for clients; they are only used internally by the CIRC engine to track the last
        // accessed epoch for the pointer.
        self.ptr.ptr_eq(other.ptr)
    }
}

impl<T: RcObject> OwnRc<T> for Rc<T> {
    #[inline]
    fn take(&mut self) -> Rc<T> {
        take(self)
    }
}

impl<'g, T: RcObject> From<Snapshot<'g, T>> for Rc<T> {
    fn from(value: Snapshot<'g, T>) -> Self {
        value.counted()
    }
}

impl<T: RcObject + Debug> Debug for Rc<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        if let Some(cnt) = self.as_ref() {
            f.debug_tuple("RcObject").field(cnt).finish()
        } else {
            f.write_str("Null")
        }
    }
}

impl<T: RcObject> Pointer for Rc<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        Pointer::fmt(&self.ptr, f)
    }
}

impl<T: RcObject> Default for Rc<T> {
    #[inline]
    fn default() -> Self {
        Self::null()
    }
}

impl<T: RcObject> Drop for Rc<T> {
    #[inline(always)]
    fn drop(&mut self) {
        unsafe {
            if let Some(cnt) = self.ptr.as_raw().as_mut() {
                RcInner::decrement_strong(cnt, 1, None);
            }
        }
    }
}

impl<T: RcObject + PartialEq> PartialEq for Rc<T> {
    #[inline(always)]
    fn eq(&self, other: &Self) -> bool {
        self.as_ref() == other.as_ref()
    }
}

impl<T: RcObject + Eq> Eq for Rc<T> {}

impl<T: RcObject + Hash> Hash for Rc<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_ref().hash(state);
    }
}

impl<T: RcObject + PartialOrd> PartialOrd for Rc<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.as_ref().partial_cmp(&other.as_ref())
    }
}

impl<T: RcObject + Ord> Ord for Rc<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.as_ref().cmp(&other.as_ref())
    }
}

/// An iterator generating [`Rc`] pointers to the same and newly allocated object.
///
/// See [`Rc::new_many_iter`] for the purpose of this iterator.
pub struct NewRcIter<T: RcObject> {
    remain: usize,
    ptr: Raw<T>,
}

impl<T: RcObject> Iterator for NewRcIter<T> {
    type Item = Rc<T>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.remain == 0 {
            None
        } else {
            self.remain -= 1;
            Some(Rc {
                ptr: self.ptr,
                _marker: PhantomData,
            })
        }
    }
}

impl<T: RcObject> NewRcIter<T> {
    /// Aborts generating [`Rc`]s.
    ///
    /// It decreases the strong reference counter as the remaining number of [`Rc`]s that are not
    /// generated yet.
    #[inline]
    pub fn abort(self, guard: &Guard) {
        if self.remain > 0 {
            unsafe {
                RcInner::decrement_strong(self.ptr.as_raw(), self.remain as _, Some(guard));
            };
        }
        forget(self);
    }
}

impl<T: RcObject> Drop for NewRcIter<T> {
    #[inline]
    fn drop(&mut self) {
        if self.remain > 0 {
            unsafe {
                RcInner::decrement_strong(self.ptr.as_raw(), self.remain as _, None);
            };
        }
    }
}

/// A local pointer protected by the backend EBR.
///
/// Unlike [`Rc`] pointer, this pointer does not own a strong reference count by itself.
/// This pointer is valid for use only during the lifetime of EBR guard `'g`.
pub struct Snapshot<'g, T> {
    pub(crate) ptr: Raw<T>,
    pub(crate) _marker: PhantomData<&'g T>,
}

impl<T> Clone for Snapshot<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Snapshot<'_, T> {}

impl<'g, T: RcObject> Snapshot<'g, T> {
    /// Returns `true` if the pointer is null ignoring the tag.
    #[inline(always)]
    pub fn is_null(&self) -> bool {
        self.ptr.is_null()
    }

    /// Creates an [`Rc`] pointer by incrementing the strong reference counter.
    #[inline]
    pub fn counted(self) -> Rc<T> {
        let rc = Rc::from_raw(self.ptr);
        unsafe {
            if let Some(cnt) = rc.ptr.as_raw().as_ref() {
                cnt.increment_strong();
            }
        }
        rc
    }

    /// Converts to `WeakSnapshot`. This does not touch the reference counter.
    #[inline]
    pub fn downgrade(self) -> WeakSnapshot<'g, T> {
        WeakSnapshot {
            ptr: self.ptr,
            _marker: PhantomData,
        }
    }

    /// Returns the tag stored within the pointer.
    #[inline(always)]
    pub fn tag(self) -> usize {
        self.ptr.tag()
    }

    /// Returns the same pointer, but tagged with `tag`. `tag` is truncated to be fit into the
    /// unused bits of the pointer to `T`.
    #[inline]
    pub fn with_tag(self, tag: usize) -> Self {
        let mut result = self;
        result.ptr = result.ptr.with_tag(tag);
        result
    }

    /// Dereferences the pointer and returns an immutable reference.
    ///
    /// It does not check whether the pointer is null.
    ///
    /// # Safety
    ///
    /// The pointer must be a valid memory location to dereference.
    #[inline]
    pub unsafe fn deref(self) -> &'g T {
        self.ptr.deref().data()
    }

    /// Dereferences the pointer and returns a mutable reference.
    ///
    /// It does not check whether the pointer is null.
    ///
    /// # Safety
    ///
    /// The pointer must be a valid memory location to dereference and
    /// other threads must not have references to the object.
    #[inline]
    pub unsafe fn deref_mut(mut self) -> &'g mut T {
        self.ptr.deref_mut().data_mut()
    }

    /// Dereferences the pointer and returns an immutable reference if it is not null.
    #[inline]
    pub fn as_ref(self) -> Option<&'g T> {
        if self.ptr.is_null() {
            None
        } else {
            Some(unsafe { self.deref() })
        }
    }

    /// Dereferences the pointer and returns a mutable reference if it is not null.
    ///
    /// # Safety
    ///
    /// Other threads must not have references to the object.
    #[inline]
    pub unsafe fn as_mut(self) -> Option<&'g mut T> {
        if self.ptr.is_null() {
            None
        } else {
            Some(unsafe { self.deref_mut() })
        }
    }

    /// Returns `true` if the two pointer values, including the tag values set by `with_tag`,
    /// are identical.
    #[inline]
    pub fn ptr_eq(self, other: Self) -> bool {
        // Instead of using a direct equality comparison (`==`), we use `ptr_eq`, which ignores
        // the epoch tag in the high bits. This is because the epoch tags hold no significance
        // for clients; they are only used internally by the CIRC engine to track the last
        // accessed epoch for the pointer.
        self.ptr.ptr_eq(other.ptr)
    }
}

impl<'g, T> Snapshot<'g, T> {
    /// Constructs a new `Snapshot` representing a null pointer.
    #[inline(always)]
    pub fn null() -> Self {
        Self {
            ptr: Tagged::null(),
            _marker: PhantomData,
        }
    }

    #[inline]
    pub(crate) fn from_raw(acquired: Raw<T>, _: &'g Guard) -> Self {
        Self {
            ptr: acquired,
            _marker: PhantomData,
        }
    }
}

impl<T: RcObject> Default for Snapshot<'_, T> {
    #[inline]
    fn default() -> Self {
        Self::null()
    }
}

impl<T: RcObject + PartialEq> PartialEq for Snapshot<'_, T> {
    #[inline(always)]
    fn eq(&self, other: &Self) -> bool {
        self.as_ref() == other.as_ref()
    }
}

impl<T: RcObject + Eq> Eq for Snapshot<'_, T> {}

impl<T: RcObject + Hash> Hash for Snapshot<'_, T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_ref().hash(state);
    }
}

impl<T: RcObject + PartialOrd> PartialOrd for Snapshot<'_, T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.as_ref().partial_cmp(&other.as_ref())
    }
}

impl<T: RcObject + Ord> Ord for Snapshot<'_, T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.as_ref().cmp(&other.as_ref())
    }
}

impl<T: RcObject + Debug> Debug for Snapshot<'_, T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        if let Some(cnt) = self.as_ref() {
            f.debug_tuple("RcObject").field(cnt).finish()
        } else {
            f.write_str("Null")
        }
    }
}

impl<T: RcObject> Pointer for Snapshot<'_, T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        Pointer::fmt(&self.ptr, f)
    }
}
