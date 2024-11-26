//! Concurrent map based on Harris's lock-free linked list
//! (<https://www.cl.cam.ac.uk/research/srg/netos/papers/2001-caslists.pdf>).

use atomic::Ordering;
use circ::{AtomicRc, EdgeTaker, Rc, RcObject, Snapshot};

use std::cmp::Ordering::{Equal, Greater, Less};

struct Node<K, V> {
    next: AtomicRc<Self>,
    key: K,
    value: V,
}

unsafe impl<K, V> RcObject for Node<K, V> {
    fn pop_edges(&mut self, out: &mut EdgeTaker<'_>) {
        out.take(&mut self.next);
    }
}

struct ListMap<K, V> {
    head: AtomicRc<Node<K, V>>,
}

impl<K, V> Default for ListMap<K, V>
where
    K: Ord + Default,
    V: Default,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<K, V> Node<K, V>
where
    K: Default,
    V: Default,
{
    /// Creates a new node.
    fn new(key: K, value: V) -> Self {
        Self {
            next: AtomicRc::null(),
            key,
            value,
        }
    }

    /// Creates a dummy head.
    /// We never deref key and value of this head node.
    fn head() -> Self {
        Self {
            next: AtomicRc::null(),
            key: K::default(),
            value: V::default(),
        }
    }
}

#[derive(Default)]
pub struct Cursor<K, V> {
    // The previous node of `curr`.
    prev: Snapshot<Node<K, V>>,
    // Tag of `curr` should always be zero so when `curr` is stored in a `prev`, we don't store a
    // tagged pointer and cause cleanup to fail.
    curr: Snapshot<Node<K, V>>,
    next: Snapshot<Node<K, V>>,

    // Additional fields for HList.
    anchor: Snapshot<Node<K, V>>,
    anchor_next: Snapshot<Node<K, V>>,
}

impl<K: Ord, V> Cursor<K, V> {
    /// Initializes a cursor.
    fn initialize(&mut self, head: &AtomicRc<Node<K, V>>) {
        head.load(&mut self.prev, Ordering::Relaxed);
        self.prev
            .as_ref()
            .unwrap()
            .next
            .load(&mut self.curr, Ordering::Acquire);
        self.anchor.clear();
        self.anchor_next.clear();
    }

    /// Clean up a chain of logically removed nodes in each traversal.
    #[inline]
    fn find_harris(&mut self, key: &K) -> Result<bool, ()> {
        let found = loop {
            // * 0 deleted: <prev> -> <curr>
            // * 1 deleted: <anchor> -> <prev> -x-> <curr>
            // * 2 deleted: <anchor> -> <anchor_next> -x-> <prev> -x-> <curr>
            // * n deleted: <anchor> -> <anchor_next> -x> (...) -x-> <prev> -x-> <curr>
            let Some(curr_node) = self.curr.as_ref() else {
                break false;
            };
            curr_node.next.load(&mut self.next, Ordering::Acquire);

            if self.next.tag() != 0 {
                // We add a 0 tag here so that `self.curr`s tag is always 0.
                self.next.set_tag(0);

                // <prev> -?-> <curr> -x-> <next>
                Snapshot::swap(&mut self.next, &mut self.curr);
                // <prev> -?-> <next> -x-> <curr>
                Snapshot::swap(&mut self.next, &mut self.prev);
                // <next> -?-> <prev> -x-> <curr>

                if self.anchor.is_null() {
                    // <next> -> <prev> -x-> <curr>, anchor = null, anchor_next = null
                    debug_assert!(self.anchor_next.is_null());
                    Snapshot::swap(&mut self.next, &mut self.anchor);
                    // <anchor> -> <prev> -x-> <curr>
                } else if self.anchor_next.is_null() {
                    // <anchor> -> <next> -x-> <prev> -x-> <curr>, anchor_next = null
                    Snapshot::swap(&mut self.next, &mut self.anchor_next);
                    // <anchor> -> <anchor_next> -x-> <prev> -x-> <curr>
                }
                continue;
            }

            match curr_node.key.cmp(key) {
                Less => {
                    Snapshot::swap(&mut self.prev, &mut self.curr);
                    Snapshot::swap(&mut self.curr, &mut self.next);
                    self.anchor.clear();
                    self.anchor_next.clear();
                }
                Equal => break true,
                Greater => break false,
            }
        };

        // If the anchor is not installed, no need to clean up
        if self.anchor.is_null() {
            return Ok(found);
        }

        // cleanup tagged nodes between anchor and curr
        let expected = if self.anchor_next.is_null() {
            &self.prev
        } else {
            &self.anchor_next
        };
        unsafe { self.anchor.deref() }
            .next
            .compare_exchange(
                expected,
                self.curr.counted(),
                Ordering::Release,
                Ordering::Relaxed,
            )
            .map_err(|_| ())?;

        Snapshot::swap(&mut self.anchor, &mut self.prev);
        Ok(found)
    }

    /// Inserts a value.
    #[inline]
    fn insert(&mut self, node: Rc<Node<K, V>>) -> Result<(), Rc<Node<K, V>>> {
        node.as_ref()
            .unwrap()
            .next
            .swap(self.curr.counted(), Ordering::Relaxed);

        self.prev
            .as_ref()
            .unwrap()
            .next
            .compare_exchange(&self.curr, node, Ordering::Release, Ordering::Relaxed)
            .map(|_| ())
            .map_err(|e| e.desired)
    }

    /// removes the current node.
    #[inline]
    fn remove(&mut self) -> Result<(), ()> {
        let curr_node = unsafe { self.curr.deref() };

        curr_node.next.load(&mut self.next, Ordering::Acquire);
        curr_node
            .next
            .compare_exchange_tag(
                self.next.with_tag(0),
                1,
                Ordering::AcqRel,
                Ordering::Relaxed,
            )
            .map_err(|_| ())?;

        unsafe { self.prev.deref() }
            .next
            .compare_exchange(
                &self.curr,
                self.next.counted(),
                Ordering::Release,
                Ordering::Relaxed,
            )
            .map(|_| ())
            .map_err(|_| ())
    }
}

impl<K, V> ListMap<K, V>
where
    K: Ord + Default,
    V: Default,
{
    /// Creates a new list.
    pub fn new() -> Self {
        ListMap {
            head: AtomicRc::new(Node::head()),
        }
    }

    #[inline]
    fn get<'h, F>(&'h self, key: &K, find: F, cursor: &'h mut Cursor<K, V>) -> Option<&'h V>
    where
        F: Fn(&mut Cursor<K, V>, &K) -> Result<bool, ()>,
    {
        if self.get_inner(key, find, cursor) {
            Some(cursor.curr.as_ref().map(|node| &node.value).unwrap())
        } else {
            None
        }
    }

    #[inline]
    fn get_inner<'h, F>(&'h self, key: &K, find: F, cursor: &'h mut Cursor<K, V>) -> bool
    where
        F: Fn(&mut Cursor<K, V>, &K) -> Result<bool, ()>,
    {
        loop {
            cursor.initialize(&self.head);
            if let Ok(r) = find(cursor, key) {
                return r;
            }
        }
    }

    #[inline]
    fn insert<'h, F>(&'h self, key: K, value: V, find: F, cursor: &'h mut Cursor<K, V>) -> bool
    where
        F: Fn(&mut Cursor<K, V>, &K) -> Result<bool, ()>,
    {
        let mut node = Rc::new(Node::new(key, value));
        loop {
            let found = self.get_inner(node.as_ref().map(|node| &node.key).unwrap(), &find, cursor);
            if found {
                return false;
            }

            match cursor.insert(node) {
                Err(n) => node = n,
                Ok(()) => return true,
            }
        }
    }

    #[inline]
    fn remove<'h, F>(&'h self, key: &K, find: F, cursor: &'h mut Cursor<K, V>) -> Option<&'h V>
    where
        F: Fn(&mut Cursor<K, V>, &K) -> Result<bool, ()>,
    {
        if self.remove_inner(key, find, cursor) {
            Some(cursor.curr.as_ref().map(|node| &node.value).unwrap())
        } else {
            None
        }
    }

    #[inline]
    fn remove_inner<'h, F>(&'h self, key: &K, find: F, cursor: &'h mut Cursor<K, V>) -> bool
    where
        F: Fn(&mut Cursor<K, V>, &K) -> Result<bool, ()>,
    {
        loop {
            let found = self.get_inner(key, &find, cursor);
            if !found {
                return false;
            }

            match cursor.remove() {
                Err(()) => continue,
                Ok(_) => return true,
            }
        }
    }

    pub fn harris_get<'h>(&'h self, key: &K, cursor: &'h mut Cursor<K, V>) -> Option<&'h V> {
        self.get(key, Cursor::find_harris, cursor)
    }

    pub fn harris_insert<'h>(&'h self, key: K, value: V, cursor: &'h mut Cursor<K, V>) -> bool {
        self.insert(key, value, Cursor::find_harris, cursor)
    }

    pub fn harris_remove<'h>(&'h self, key: &K, cursor: &'h mut Cursor<K, V>) -> Option<&'h V> {
        self.remove(key, Cursor::find_harris, cursor)
    }
}

#[test]
fn smoke() {
    extern crate rand;
    use crossbeam_utils::thread;
    use rand::prelude::*;

    const THREADS: i32 = 30;
    const ELEMENTS_PER_THREADS: i32 = 1000;

    let map = &ListMap::new();

    thread::scope(|s| {
        for t in 0..THREADS {
            s.spawn(move |_| {
                let rng = &mut rand::thread_rng();
                let mut keys: Vec<i32> =
                    (0..ELEMENTS_PER_THREADS).map(|k| k * THREADS + t).collect();
                keys.shuffle(rng);
                let cursor = &mut Cursor::default();
                for i in keys {
                    assert!(map.harris_insert(i, i.to_string(), cursor));
                }
            });
        }
    })
    .unwrap();

    thread::scope(|s| {
        for t in 0..THREADS {
            s.spawn(move |_| {
                let rng = &mut rand::thread_rng();
                let mut keys: Vec<i32> =
                    (0..ELEMENTS_PER_THREADS).map(|k| k * THREADS + t).collect();
                keys.shuffle(rng);
                let cursor = &mut Cursor::default();
                if t < THREADS / 2 {
                    for i in keys {
                        assert_eq!(i.to_string(), *map.harris_remove(&i, cursor).unwrap());
                    }
                } else {
                    for i in keys {
                        assert_eq!(i.to_string(), *map.harris_get(&i, cursor).unwrap());
                    }
                }
            });
        }
    })
    .unwrap();
}
