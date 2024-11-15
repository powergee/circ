use bitflags::bitflags;
use circ::{
    AtomicRc, AtomicWeak, CompareExchangeError, EdgeTaker, Guard, OwnRc, Rc, RcObject, Snapshot,
};
use std::sync::atomic::Ordering;

bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    struct UpdateTag: usize {
        const CLEAN = 0usize;
        const DFLAG = 1usize;
        const IFLAG = 2usize;
        const MARK = 3usize;
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Key<K> {
    Fin(K),
    Inf1,
    Inf2,
}

impl<K> PartialOrd for Key<K>
where
    K: PartialOrd,
{
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        match (self, other) {
            (Key::Fin(k1), Key::Fin(k2)) => k1.partial_cmp(k2),
            (Key::Fin(_), Key::Inf1) => Some(std::cmp::Ordering::Less),
            (Key::Fin(_), Key::Inf2) => Some(std::cmp::Ordering::Less),
            (Key::Inf1, Key::Fin(_)) => Some(std::cmp::Ordering::Greater),
            (Key::Inf1, Key::Inf1) => Some(std::cmp::Ordering::Equal),
            (Key::Inf1, Key::Inf2) => Some(std::cmp::Ordering::Less),
            (Key::Inf2, Key::Fin(_)) => Some(std::cmp::Ordering::Greater),
            (Key::Inf2, Key::Inf1) => Some(std::cmp::Ordering::Greater),
            (Key::Inf2, Key::Inf2) => Some(std::cmp::Ordering::Equal),
        }
    }
}

impl<K> PartialEq<K> for Key<K>
where
    K: PartialEq,
{
    fn eq(&self, rhs: &K) -> bool {
        match self {
            Key::Fin(k) => k == rhs,
            _ => false,
        }
    }
}

impl<K> PartialOrd<K> for Key<K>
where
    K: PartialOrd,
{
    fn partial_cmp(&self, rhs: &K) -> Option<std::cmp::Ordering> {
        match self {
            Key::Fin(k) => k.partial_cmp(rhs),
            _ => Some(std::cmp::Ordering::Greater),
        }
    }
}

impl<K> Key<K>
where
    K: Ord,
{
    fn cmp(&self, rhs: &K) -> std::cmp::Ordering {
        match self {
            Key::Fin(k) => k.cmp(rhs),
            _ => std::cmp::Ordering::Greater,
        }
    }
}

pub struct Node<K, V> {
    key: Key<K>,
    value: Option<V>,
    // tag on low bits: {Clean, DFlag, IFlag, Mark}
    update: AtomicRc<Update<K, V>>,
    left: AtomicRc<Node<K, V>>,
    right: AtomicRc<Node<K, V>>,
}

pub enum Update<K, V> {
    Insert {
        p: AtomicWeak<Node<K, V>>,
        new_internal: AtomicRc<Node<K, V>>,
        l: AtomicWeak<Node<K, V>>,
    },
    Delete {
        gp: AtomicWeak<Node<K, V>>,
        p: AtomicWeak<Node<K, V>>,
        l: AtomicWeak<Node<K, V>>,
        pupdate: AtomicRc<Update<K, V>>,
    },
}

unsafe impl<K, V> RcObject for Node<K, V> {
    fn pop_edges(&mut self, out: &mut EdgeTaker<'_>) {
        out.take(&mut self.left);
        out.take(&mut self.right);
        out.take(&mut self.update);
    }
}

unsafe impl<K, V> RcObject for Update<K, V> {
    fn pop_edges(&mut self, out: &mut EdgeTaker<'_>) {
        match self {
            Update::Insert { new_internal, .. } => out.take(new_internal),
            Update::Delete { pupdate, .. } => out.take(pupdate),
        }
    }
}

impl<K, V> Node<K, V> {
    pub fn internal(key: Key<K>, value: Option<V>, left: Self, right: Self) -> Self {
        Self {
            key,
            value,
            update: AtomicRc::null(),
            left: AtomicRc::new(left),
            right: AtomicRc::new(right),
        }
    }

    pub fn leaf(key: Key<K>, value: Option<V>) -> Self {
        Self {
            key,
            value,
            update: AtomicRc::null(),
            left: AtomicRc::null(),
            right: AtomicRc::null(),
        }
    }

    #[inline]
    pub fn is_leaf(&self, guard: &Guard) -> bool {
        self.left.load(Ordering::Acquire, guard).is_null()
    }
}

struct Cursor<'g, K, V> {
    gp: Snapshot<'g, Node<K, V>>,
    p: Snapshot<'g, Node<K, V>>,
    l: Snapshot<'g, Node<K, V>>,
    pupdate: Snapshot<'g, Update<K, V>>,
    gpupdate: Snapshot<'g, Update<K, V>>,
}

impl<'g, K, V> Cursor<'g, K, V>
where
    K: Ord + Clone,
    V: Clone,
{
    fn new(root: Snapshot<'g, Node<K, V>>) -> Self {
        Self {
            gp: Snapshot::null(),
            p: Snapshot::null(),
            l: root,
            pupdate: Snapshot::null(),
            gpupdate: Snapshot::null(),
        }
    }

    /// Used by Insert, Delete and Find to traverse a branch of the BST.
    ///
    /// # Safety
    /// It satisfies following postconditions:
    ///
    /// 1. l points to a Leaf node and p points to an Internal node
    /// 2. Either p → left has contained l (if k<p → key) or p → right has contained l (if k ≥ p → key)
    /// 3. p → update has contained pupdate
    /// 4. if l → key != Inf1, then the following three statements hold:
    ///     - gp points to an Internal node
    ///     - either gp → left has contained p (if k < gp → key) or gp → right has contained p (if k ≥ gp → key)
    ///     - gp → update has contained gpupdate
    #[inline]
    fn search(&mut self, key: &K, guard: &'g Guard) {
        loop {
            let l_node = self.l.as_ref().unwrap();
            if l_node.is_leaf(guard) {
                break;
            }
            self.gp = self.p;
            self.p = self.l;
            self.gpupdate = self.pupdate;
            self.pupdate = l_node.update.load(Ordering::Acquire, guard);
            self.l = match l_node.key.cmp(key) {
                std::cmp::Ordering::Greater => l_node.left.load(Ordering::Acquire, guard),
                _ => l_node.right.load(Ordering::Acquire, guard),
            }
        }
    }
}

pub struct EFRBTree<K, V> {
    root: AtomicRc<Node<K, V>>,
}

impl<K, V> Default for EFRBTree<K, V>
where
    K: Ord + Clone,
    V: Clone,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<K, V> EFRBTree<K, V>
where
    K: Ord + Clone,
    V: Clone,
{
    pub fn new() -> Self {
        Self {
            root: AtomicRc::new(Node::internal(
                Key::Inf2,
                None,
                Node::leaf(Key::Inf1, None),
                Node::leaf(Key::Inf2, None),
            )),
        }
    }

    pub fn find<'g>(&self, key: &'g K, guard: &'g Guard) -> Option<&'g V> {
        let mut cursor = Cursor::new(self.root.load(Ordering::Relaxed, guard));
        cursor.search(key, guard);
        let l_node = cursor.l.as_ref().unwrap();
        if l_node.key.eq(key) {
            Some(l_node.value.as_ref().unwrap())
        } else {
            None
        }
    }

    pub fn insert(&self, key: &K, value: V, guard: &Guard) -> bool {
        loop {
            let mut cursor = Cursor::new(self.root.load(Ordering::Relaxed, guard));
            cursor.search(key, guard);
            let l_node = cursor.l.as_ref().unwrap();
            let p_node = cursor.p.as_ref().unwrap();

            if l_node.key == *key {
                return false;
            } else if cursor.pupdate.tag() != UpdateTag::CLEAN.bits() {
                self.help(cursor.pupdate, guard);
            } else {
                let new = Node::leaf(Key::Fin(key.clone()), Some(value.clone()));
                let new_sibling = Node::leaf(l_node.key.clone(), l_node.value.clone());

                let (left, right) = match new.key.partial_cmp(&new_sibling.key) {
                    Some(std::cmp::Ordering::Less) => (new, new_sibling),
                    _ => (new_sibling, new),
                };

                let new_internal = Rc::new(Node::internal(
                    // key field max(k, l → key)
                    right.key.clone(),
                    None,
                    // two child fields equal to new and newSibling
                    // (the one with the smaller key is the left child)
                    left,
                    right,
                ));

                let op = Update::Insert {
                    p: AtomicWeak::from(cursor.p.downgrade().counted()),
                    new_internal: AtomicRc::from(new_internal),
                    l: AtomicWeak::from(cursor.l.downgrade().counted()),
                };

                let new_pupdate = Rc::new(op).with_tag(UpdateTag::IFLAG.bits());
                let new_pupdate_sh = new_pupdate.snapshot(guard);

                match p_node.update.compare_exchange(
                    cursor.pupdate,
                    new_pupdate,
                    Ordering::Release,
                    Ordering::Relaxed,
                    guard,
                ) {
                    Ok(_) => {
                        self.help_insert(new_pupdate_sh, guard);
                        return true;
                    }
                    Err(e) => {
                        let mut new_pupdate = e.desired;
                        unsafe {
                            let mut new_pupdate_failed = new_pupdate.deref_mut();
                            if let Update::Insert { new_internal, .. } = &mut new_pupdate_failed {
                                let mut new_internal_failed = new_internal.take();
                                new_internal_failed.deref_mut().left.take().finalize(guard);
                                new_internal_failed.deref_mut().right.take().finalize(guard);
                                new_internal_failed.finalize(guard);
                            }
                            new_pupdate.finalize(guard);
                        }
                        self.help(e.current, guard);
                    }
                }
            }
        }
    }

    pub fn delete<'g>(&self, key: &'g K, guard: &'g Guard) -> Option<&'g V> {
        loop {
            let mut cursor = Cursor::new(self.root.load(Ordering::Relaxed, guard));
            cursor.search(key, guard);

            if cursor.gp.is_null() {
                // The tree is empty. There's no more things to do.
                return None;
            }

            let l_node = cursor.l.as_ref().unwrap();

            if l_node.key != Key::Fin(key.clone()) {
                return None;
            }
            if cursor.gpupdate.tag() != UpdateTag::CLEAN.bits() {
                self.help(cursor.gpupdate, guard);
            } else if cursor.pupdate.tag() != UpdateTag::CLEAN.bits() {
                self.help(cursor.pupdate, guard);
            } else {
                let op = Update::Delete {
                    gp: AtomicWeak::from(cursor.gp.downgrade().counted()),
                    p: AtomicWeak::from(cursor.p.downgrade().counted()),
                    l: AtomicWeak::from(cursor.l.downgrade().counted()),
                    pupdate: AtomicRc::from(cursor.pupdate.counted()),
                };

                let new_update = Rc::new(op).with_tag(UpdateTag::DFLAG.bits());
                let new_update_sh = new_update.snapshot(guard);

                match cursor.gp.as_ref().unwrap().update.compare_exchange(
                    cursor.gpupdate,
                    new_update,
                    Ordering::Release,
                    Ordering::Relaxed,
                    guard,
                ) {
                    Ok(_) => {
                        if self.help_delete(new_update_sh, guard).is_some() {
                            return Some(l_node.value.as_ref().unwrap());
                        }
                    }
                    Err(e) => {
                        let new_update = e.desired;
                        new_update.finalize(guard);
                        self.help(e.current, guard);
                    }
                }
            }
        }
    }

    #[inline]
    fn help<'g>(&'g self, update: Snapshot<'g, Update<K, V>>, guard: &'g Guard) {
        let _ = match UpdateTag::from_bits_truncate(update.tag()) {
            UpdateTag::IFLAG => self.help_insert(update, guard),
            UpdateTag::MARK => self.help_marked(update, guard),
            UpdateTag::DFLAG => self.help_delete(update, guard),
            _ => Some(()),
        };
    }

    fn help_delete<'g>(&'g self, op: Snapshot<'g, Update<K, V>>, guard: &'g Guard) -> Option<()> {
        // Precondition: op points to a DInfo record (i.e., it is not ⊥)
        let op_ref = op.as_ref().unwrap();
        if let Update::Delete { gp, p, pupdate, .. } = op_ref {
            let p_ref = p
                .load(Ordering::Relaxed, guard)
                .upgrade()?
                .as_ref()
                .unwrap();
            let pupdate_sh = pupdate.load(Ordering::Relaxed, guard);
            let new_op = op.with_tag(UpdateTag::MARK.bits());

            match p_ref.update.compare_exchange(
                pupdate_sh,
                new_op.counted(),
                Ordering::Release,
                Ordering::Acquire,
                guard,
            ) {
                Ok(_) => {
                    // (prev value) = op → pupdate
                    self.help_marked(new_op, guard);
                    Some(())
                }
                Err(e) => {
                    if e.current.ptr_eq(new_op) {
                        // (prev value) = <Mark, op>
                        self.help_marked(new_op, guard);
                        Some(())
                    } else {
                        self.help(e.current, guard);
                        let _ = gp
                            .load(Ordering::Acquire, guard)
                            .upgrade()?
                            .as_ref()
                            .unwrap()
                            .update
                            .compare_exchange(
                                op.with_tag(UpdateTag::DFLAG.bits()),
                                op.with_tag(UpdateTag::CLEAN.bits()).counted(),
                                Ordering::Release,
                                Ordering::Relaxed,
                                guard,
                            );
                        None
                    }
                }
            }
        } else {
            panic!("op is not pointing to a DInfo record")
        }
    }

    fn help_marked<'g>(&'g self, op: Snapshot<'g, Update<K, V>>, guard: &'g Guard) -> Option<()> {
        // Precondition: op points to a DInfo record (i.e., it is not ⊥)
        let op_ref = op.as_ref().unwrap();
        if let Update::Delete { gp, p, l, .. } = op_ref {
            // Set other to point to the sibling of the node to which op → l points
            let gp = gp.load(Ordering::Relaxed, guard).upgrade()?;
            let p = p.load(Ordering::Relaxed, guard).upgrade()?;
            let l = l.load(Ordering::Relaxed, guard).upgrade()?;

            let p_ref = p.as_ref().unwrap();
            let other = if p_ref.right.load(Ordering::Acquire, guard).ptr_eq(l) {
                &p_ref.left
            } else {
                &p_ref.right
            };
            // Splice the node to which op → p points out of the tree, replacing it by other
            let other_sh = other.load(Ordering::Acquire, guard);

            let success = self.cas_child(gp, p, other_sh, guard).ok();
            let _ = gp.as_ref().unwrap().update.compare_exchange(
                op.with_tag(UpdateTag::DFLAG.bits()),
                op.with_tag(UpdateTag::CLEAN.bits()).counted(),
                Ordering::Release,
                Ordering::Relaxed,
                guard,
            );

            success.map(|_| ())
        } else {
            panic!("op is not pointing to a DInfo record")
        }
    }

    fn help_insert<'g>(&'g self, op: Snapshot<'g, Update<K, V>>, guard: &'g Guard) -> Option<()> {
        // Precondition: op points to an IInfo record (i.e., it is not ⊥)
        let op_ref = op.as_ref().unwrap();
        if let Update::Insert { p, new_internal, l } = op_ref {
            let p = p.load(Ordering::Relaxed, guard).upgrade()?;
            let new_internal = new_internal.load(Ordering::Relaxed, guard);
            let l = l.load(Ordering::Relaxed, guard).upgrade()?;

            let success = self.cas_child(p, l, new_internal, guard).ok();
            let p_ref = p.as_ref().unwrap();
            let _ = p_ref.update.compare_exchange(
                op.with_tag(UpdateTag::IFLAG.bits()),
                op.with_tag(UpdateTag::CLEAN.bits()).counted(),
                Ordering::Release,
                Ordering::Relaxed,
                guard,
            );

            success.map(|_| ())
        } else {
            panic!("op is not pointing to an IInfo record")
        }
    }

    #[inline]
    fn cas_child<'g>(
        &'g self,
        parent: Snapshot<'g, Node<K, V>>,
        old: Snapshot<'g, Node<K, V>>,
        new: Snapshot<'g, Node<K, V>>,
        guard: &'g Guard,
    ) -> Result<Rc<Node<K, V>>, CompareExchangeError<Rc<Node<K, V>>, Snapshot<'g, Node<K, V>>>>
    {
        // Precondition: parent points to an Internal node and new points to a Node (i.e., neither is ⊥)
        // This routine tries to change one of the child fields of the node that parent points to from old to new.
        let new_node = new.as_ref().unwrap();
        let parent_node = parent.as_ref().unwrap();
        let node_to_cas = if new_node.key < parent_node.key {
            &parent_node.left
        } else {
            &parent_node.right
        };
        node_to_cas.compare_exchange(
            old,
            new.counted(),
            Ordering::Release,
            Ordering::Acquire,
            guard,
        )
    }
}

#[test]
fn smoke() {
    extern crate rand;
    use circ::cs;
    use crossbeam_utils::thread;
    use rand::prelude::*;

    const THREADS: i32 = 30;
    const ELEMENTS_PER_THREADS: i32 = 1000;

    let map = &EFRBTree::new();

    thread::scope(|s| {
        for t in 0..THREADS {
            s.spawn(move |_| {
                let rng = &mut rand::thread_rng();
                let mut keys: Vec<i32> =
                    (0..ELEMENTS_PER_THREADS).map(|k| k * THREADS + t).collect();
                keys.shuffle(rng);
                for i in keys {
                    assert!(map.insert(&i, i.to_string(), &cs()));
                }
            });
        }
    })
    .unwrap();

    thread::scope(|s| {
        for t in 0..(THREADS / 2) {
            s.spawn(move |_| {
                let rng = &mut rand::thread_rng();
                let mut keys: Vec<i32> =
                    (0..ELEMENTS_PER_THREADS).map(|k| k * THREADS + t).collect();
                keys.shuffle(rng);
                let mut guard = cs();
                for i in keys {
                    assert_eq!(i.to_string(), *map.delete(&i, &guard).unwrap());
                    guard = cs();
                }
            });
        }
    })
    .unwrap();

    thread::scope(|s| {
        for t in (THREADS / 2)..THREADS {
            s.spawn(move |_| {
                let rng = &mut rand::thread_rng();
                let mut keys: Vec<i32> =
                    (0..ELEMENTS_PER_THREADS).map(|k| k * THREADS + t).collect();
                keys.shuffle(rng);
                let mut guard = cs();
                for i in keys {
                    assert_eq!(i.to_string(), *map.find(&i, &guard).unwrap());
                    guard = cs();
                }
            });
        }
    })
    .unwrap();
}
