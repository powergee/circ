use std::sync::atomic::Ordering;

use circ::{AtomicRc, Cs, GraphNode, Pointer, Rc, Snapshot, Weak};
use crossbeam_utils::CachePadded;

pub struct Output<'g, T> {
    found: Snapshot<'g, Node<T>>,
}

impl<'g, T> Output<'g, T> {
    pub fn output(&self) -> &T {
        self.found
            .as_ref()
            .map(|node| node.item.as_ref().unwrap())
            .unwrap()
    }
}

struct Node<T> {
    item: Option<T>,
    prev: Weak<Node<T>>,
    next: CachePadded<AtomicRc<Node<T>>>,
}

impl<T> GraphNode for Node<T> {
    fn pop_outgoings(&mut self, out: &mut Vec<Rc<Self>>) {
        out.push(self.next.take())
    }
}

impl<T> Node<T> {
    fn sentinel() -> Self {
        Self {
            item: None,
            prev: Weak::null(),
            next: CachePadded::new(AtomicRc::null()),
        }
    }

    fn new(item: T) -> Self {
        Self {
            item: Some(item),
            prev: Weak::null(),
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

impl<T: Sync + Send> DoubleLink<T> {
    #[inline]
    pub fn new() -> Self {
        let sentinel = Rc::new(Node::sentinel());
        // Note: In RC-based SMRs(CDRC, CIRC, ...), `sentinel.prev` MUST NOT be set to the self.
        // It will make a loop after the first enqueue, blocking the entire reclamation.
        Self {
            head: CachePadded::new(AtomicRc::from(sentinel.clone())),
            tail: CachePadded::new(AtomicRc::from(sentinel)),
        }
    }

    #[inline]
    pub fn enqueue(&self, item: T, cs: &Cs) {
        let [mut node, sub] = Rc::new_many(Node::new(item));

        loop {
            let ltail = self.tail.load(Ordering::Acquire, cs);
            unsafe { node.deref_mut() }.prev = ltail.downgrade();

            // Try to help the previous enqueue to complete.
            if let Some(lprev) = ltail
                .as_ref()
                .unwrap()
                .prev
                .as_snapshot(cs)
                .and_then(Snapshot::as_ref)
            {
                if lprev.next.load(Ordering::SeqCst, cs).is_null() {
                    lprev.next.store(ltail.upgrade(), Ordering::Relaxed, cs);
                }
            }
            match self
                .tail
                .compare_exchange(ltail, node, Ordering::SeqCst, Ordering::SeqCst, cs)
            {
                Ok(_) => {
                    ltail
                        .as_ref()
                        .unwrap()
                        .next
                        .store(sub, Ordering::Release, cs);
                    return;
                }
                Err(e) => node = e.desired,
            }
        }
    }

    #[inline]
    pub fn dequeue<'g>(&self, cs: &'g Cs) -> Option<Output<'g, T>> {
        loop {
            let lhead = self.head.load(Ordering::Acquire, cs);
            let lnext = lhead.as_ref().unwrap().next.load(Ordering::Acquire, cs);
            // Check if this queue is empty.
            if lnext.is_null() {
                return None;
            }

            if self
                .head
                .compare_exchange(
                    lhead,
                    lnext.upgrade(),
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                    cs,
                )
                .is_ok()
            {
                return Some(Output { found: lnext });
            }
        }
    }
}

#[cfg(test)]
mod test {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::DoubleLink;
    use circ::pin;
    use crossbeam_utils::thread::scope;

    #[test]
    fn simple() {
        let queue = DoubleLink::new();
        let cs = &pin();
        assert!(queue.dequeue(cs).is_none());
        queue.enqueue(1, cs);
        queue.enqueue(2, cs);
        queue.enqueue(3, cs);
        assert_eq!(*queue.dequeue(cs).unwrap().output(), 1);
        assert_eq!(*queue.dequeue(cs).unwrap().output(), 2);
        assert_eq!(*queue.dequeue(cs).unwrap().output(), 3);
        assert!(queue.dequeue(cs).is_none());
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
                    for i in 0..ELEMENTS_PER_THREAD {
                        queue.enqueue((t * ELEMENTS_PER_THREAD + i).to_string(), &pin());
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
                    for _ in 0..ELEMENTS_PER_THREAD {
                        let guard = pin();
                        let output = queue.dequeue(&guard).unwrap();
                        let res = output.output();
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
