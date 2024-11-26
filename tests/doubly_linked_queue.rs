//! Implementation of Ramalhete and Correia's "DoubleLink" lock-free queue
//! (<https://concurrencyfreaks.blogspot.com/2017/01/doublelink-low-overhead-lock-free-queue.html>).

use std::sync::atomic::Ordering;

use circ::{AtomicRc, AtomicWeak, Rc, RcObject, Snapshot};
use crossbeam_utils::CachePadded;

#[derive(Default)]
pub struct Holder<T> {
    pri: Snapshot<Node<T>>,
    sub: Snapshot<Node<T>>,
    new: Snapshot<Node<T>>,
}

struct Node<T> {
    item: Option<T>,
    prev: AtomicWeak<Node<T>>,
    next: CachePadded<AtomicRc<Node<T>>>,
}

unsafe impl<T> RcObject for Node<T> {
    fn pop_edges(&mut self, _out: &mut circ::EdgeTaker<'_>) {
        todo!()
    }
}

impl<T> Node<T> {
    fn sentinel() -> Self {
        Self {
            item: None,
            prev: AtomicWeak::null(),
            next: CachePadded::new(AtomicRc::null()),
        }
    }

    fn new(item: T) -> Self {
        Self {
            item: Some(item),
            prev: AtomicWeak::null(),
            next: CachePadded::new(AtomicRc::null()),
        }
    }
}

unsafe impl<T: Sync> Sync for Node<T> {}
unsafe impl<T: Sync> Send for Node<T> {}

pub struct DoubleLink<T: Sync + Send> {
    head: CachePadded<AtomicRc<Node<T>>>,
    tail: CachePadded<AtomicRc<Node<T>>>,
}

impl<T: Sync + Send> Default for DoubleLink<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Sync + Send> DoubleLink<T> {
    pub fn new() -> Self {
        let sentinel = Rc::new(Node::sentinel());
        // Note: In RC-based SMRs(CDRC, CIRC, ...), `sentinel.prev` MUST NOT be set to the self.
        // It will make a loop after the first enqueue, blocking the entire reclamation.
        Self {
            head: CachePadded::new(AtomicRc::from(sentinel.clone())),
            tail: CachePadded::new(AtomicRc::from(sentinel)),
        }
    }

    pub fn enqueue(&self, item: T, holder: &mut Holder<T>) {
        let new = &mut holder.new;
        let ltail = &mut holder.pri;
        let lprev = &mut holder.sub;

        let mut node = Rc::new(Node::new(item));
        new.protect(&node);

        loop {
            self.tail.load(ltail, Ordering::Acquire);
            node.as_ref()
                .unwrap()
                .prev
                .store(ltail.weak(), Ordering::Relaxed);

            // Try to help the previous enqueue to complete.
            ltail
                .as_ref()
                .unwrap()
                .prev
                .try_load(lprev, Ordering::SeqCst);
            if let Some(lprev) = lprev.as_ref() {
                if lprev.next.load_raw(Ordering::SeqCst).is_null() {
                    lprev.next.store(ltail.counted(), Ordering::Relaxed);
                }
            }
            match self
                .tail
                .compare_exchange(&*ltail, node, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => {
                    ltail
                        .as_ref()
                        .unwrap()
                        .next
                        .store(new.counted(), Ordering::Release);
                    return;
                }
                Err(e) => node = e.desired,
            }
        }
    }

    pub fn dequeue<'h>(&self, holder: &'h mut Holder<T>) -> Option<&'h T> {
        let lhead = &mut holder.pri;
        let lnext = &mut holder.sub;

        loop {
            self.head.load(lhead, Ordering::Acquire);
            lhead.as_ref().unwrap().next.load(lnext, Ordering::Acquire);
            // Check if this queue is empty.
            if lnext.is_null() {
                return None;
            }

            if self
                .head
                .compare_exchange(&*lhead, lnext.counted(), Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return Some(lnext.as_ref().and_then(|node| node.item.as_ref()).unwrap());
            }
        }
    }
}

#[cfg(test)]
mod test {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::{DoubleLink, Holder};
    use crossbeam_utils::thread::scope;

    #[test]
    fn simple() {
        let queue = DoubleLink::new();
        let holder = &mut Holder::default();
        assert!(queue.dequeue(holder).is_none());
        queue.enqueue(1, holder);
        queue.enqueue(2, holder);
        queue.enqueue(3, holder);
        assert_eq!(*queue.dequeue(holder).unwrap(), 1);
        assert_eq!(*queue.dequeue(holder).unwrap(), 2);
        assert_eq!(*queue.dequeue(holder).unwrap(), 3);
        assert!(queue.dequeue(holder).is_none());
    }

    #[test]
    fn smoke() {
        const THREADS: usize = 100;
        const ELEMENTS_PER_THREAD: usize = 10000;

        let queue = DoubleLink::new();
        let mut found = Vec::new();
        found.resize_with(THREADS * ELEMENTS_PER_THREAD, || AtomicU32::new(0));

        scope(|s| {
            for t in 0..THREADS {
                let queue = &queue;
                s.spawn(move |_| {
                    let holder = &mut Holder::default();
                    for i in 0..ELEMENTS_PER_THREAD {
                        queue.enqueue((t * ELEMENTS_PER_THREAD + i).to_string(), holder);
                    }
                });
            }
        })
        .unwrap();

        scope(|s| {
            for _ in 0..THREADS {
                let queue = &queue;
                let found = &found;
                s.spawn(move |_| {
                    let holder = &mut Holder::default();
                    for _ in 0..ELEMENTS_PER_THREAD {
                        let res = queue.dequeue(holder).unwrap();
                        assert_eq!(
                            found[res.parse::<usize>().unwrap()].fetch_add(1, Ordering::Relaxed),
                            0
                        );
                    }
                });
            }
        })
        .unwrap();

        assert!(
            found
                .iter()
                .filter(|v| v.load(Ordering::Relaxed) == 0)
                .count()
                == 0
        );
    }
}
