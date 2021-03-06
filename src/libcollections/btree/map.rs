// Copyright 2014 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

// This implementation is largely based on the high-level description and analysis of B-Trees
// found in *Open Data Structures* (ODS). Although our implementation does not use any of
// the source found in ODS, if one wishes to review the high-level design of this structure, it
// can be freely downloaded at http://opendatastructures.org/. Its contents are as of this
// writing (August 2014) freely licensed under the following Creative Commons Attribution
// License: [CC BY 2.5 CA](http://creativecommons.org/licenses/by/2.5/ca/).

pub use self::Entry::*;

use core::prelude::*;

use core::borrow::BorrowFrom;
use core::cmp::Ordering;
use core::default::Default;
use core::fmt::Show;
use core::hash::{Writer, Hash};
use core::iter::{Map, FromIterator};
use core::ops::{Index, IndexMut};
use core::{iter, fmt, mem};

use ring_buf::RingBuf;

use self::Continuation::{Continue, Finished};
use self::StackOp::*;
use super::node::ForceResult::{Leaf, Internal};
use super::node::TraversalItem::{self, Elem, Edge};
use super::node::{Traversal, MutTraversal, MoveTraversal};
use super::node::{self, Node, Found, GoDown};

// FIXME(conventions): implement bounded iterators

/// A map based on a B-Tree.
///
/// B-Trees represent a fundamental compromise between cache-efficiency and actually minimizing
/// the amount of work performed in a search. In theory, a binary search tree (BST) is the optimal
/// choice for a sorted map, as a perfectly balanced BST performs the theoretical minimum amount of
/// comparisons necessary to find an element (log<sub>2</sub>n). However, in practice the way this
/// is done is *very* inefficient for modern computer architectures. In particular, every element
/// is stored in its own individually heap-allocated node. This means that every single insertion
/// triggers a heap-allocation, and every single comparison should be a cache-miss. Since these
/// are both notably expensive things to do in practice, we are forced to at very least reconsider
/// the BST strategy.
///
/// A B-Tree instead makes each node contain B-1 to 2B-1 elements in a contiguous array. By doing
/// this, we reduce the number of allocations by a factor of B, and improve cache efficiency in
/// searches. However, this does mean that searches will have to do *more* comparisons on average.
/// The precise number of comparisons depends on the node search strategy used. For optimal cache
/// efficiency, one could search the nodes linearly. For optimal comparisons, one could search
/// the node using binary search. As a compromise, one could also perform a linear search
/// that initially only checks every i<sup>th</sup> element for some choice of i.
///
/// Currently, our implementation simply performs naive linear search. This provides excellent
/// performance on *small* nodes of elements which are cheap to compare. However in the future we
/// would like to further explore choosing the optimal search strategy based on the choice of B,
/// and possibly other factors. Using linear search, searching for a random element is expected
/// to take O(B log<sub>B</sub>n) comparisons, which is generally worse than a BST. In practice,
/// however, performance is excellent. `BTreeMap` is able to readily outperform `TreeMap` under
/// many workloads, and is competitive where it doesn't. BTreeMap also generally *scales* better
/// than TreeMap, making it more appropriate for large datasets.
///
/// However, `TreeMap` may still be more appropriate to use in many contexts. If elements are very
/// large or expensive to compare, `TreeMap` may be more appropriate. It won't allocate any
/// more space than is needed, and will perform the minimal number of comparisons necessary.
/// `TreeMap` also provides much better performance stability guarantees. Generally, very few
/// changes need to be made to update a BST, and two updates are expected to take about the same
/// amount of time on roughly equal sized BSTs. However a B-Tree's performance is much more
/// amortized. If a node is overfull, it must be split into two nodes. If a node is underfull, it
/// may be merged with another. Both of these operations are relatively expensive to perform, and
/// it's possible to force one to occur at every single level of the tree in a single insertion or
/// deletion. In fact, a malicious or otherwise unlucky sequence of insertions and deletions can
/// force this degenerate behaviour to occur on every operation. While the total amount of work
/// done on each operation isn't *catastrophic*, and *is* still bounded by O(B log<sub>B</sub>n),
/// it is certainly much slower when it does.
#[derive(Clone)]
#[stable]
pub struct BTreeMap<K, V> {
    root: Node<K, V>,
    length: uint,
    depth: uint,
    b: uint,
}

/// An abstract base over-which all other BTree iterators are built.
struct AbsIter<T> {
    lca: T,
    left: RingBuf<T>,
    right: RingBuf<T>,
    size: uint,
}

/// An iterator over a BTreeMap's entries.
#[stable]
pub struct Iter<'a, K: 'a, V: 'a> {
    inner: AbsIter<Traversal<'a, K, V>>
}

/// A mutable iterator over a BTreeMap's entries.
#[stable]
pub struct IterMut<'a, K: 'a, V: 'a> {
    inner: AbsIter<MutTraversal<'a, K, V>>
}

/// An owning iterator over a BTreeMap's entries.
#[stable]
pub struct IntoIter<K, V> {
    inner: AbsIter<MoveTraversal<K, V>>
}

/// An iterator over a BTreeMap's keys.
#[stable]
pub struct Keys<'a, K: 'a, V: 'a> {
    inner: Map<(&'a K, &'a V), &'a K, Iter<'a, K, V>, fn((&'a K, &'a V)) -> &'a K>
}

/// An iterator over a BTreeMap's values.
#[stable]
pub struct Values<'a, K: 'a, V: 'a> {
    inner: Map<(&'a K, &'a V), &'a V, Iter<'a, K, V>, fn((&'a K, &'a V)) -> &'a V>
}

/// A view into a single entry in a map, which may either be vacant or occupied.
#[unstable = "precise API still under development"]
pub enum Entry<'a, K:'a, V:'a> {
    /// A vacant Entry
    Vacant(VacantEntry<'a, K, V>),
    /// An occupied Entry
    Occupied(OccupiedEntry<'a, K, V>),
}

/// A vacant Entry.
#[unstable = "precise API still under development"]
pub struct VacantEntry<'a, K:'a, V:'a> {
    key: K,
    stack: stack::SearchStack<'a, K, V, node::handle::Edge, node::handle::Leaf>,
}

/// An occupied Entry.
#[unstable = "precise API still under development"]
pub struct OccupiedEntry<'a, K:'a, V:'a> {
    stack: stack::SearchStack<'a, K, V, node::handle::KV, node::handle::LeafOrInternal>,
}

impl<K: Ord, V> BTreeMap<K, V> {
    /// Makes a new empty BTreeMap with a reasonable choice for B.
    #[stable]
    pub fn new() -> BTreeMap<K, V> {
        //FIXME(Gankro): Tune this as a function of size_of<K/V>?
        BTreeMap::with_b(6)
    }

    /// Makes a new empty BTreeMap with the given B.
    ///
    /// B cannot be less than 2.
    pub fn with_b(b: uint) -> BTreeMap<K, V> {
        assert!(b > 1, "B must be greater than 1");
        BTreeMap {
            length: 0,
            depth: 1,
            root: Node::make_leaf_root(b),
            b: b,
        }
    }

    /// Clears the map, removing all values.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::collections::BTreeMap;
    ///
    /// let mut a = BTreeMap::new();
    /// a.insert(1u, "a");
    /// a.clear();
    /// assert!(a.is_empty());
    /// ```
    #[stable]
    pub fn clear(&mut self) {
        let b = self.b;
        // avoid recursive destructors by manually traversing the tree
        for _ in mem::replace(self, BTreeMap::with_b(b)).into_iter() {};
    }

    // Searching in a B-Tree is pretty straightforward.
    //
    // Start at the root. Try to find the key in the current node. If we find it, return it.
    // If it's not in there, follow the edge *before* the smallest key larger than
    // the search key. If no such key exists (they're *all* smaller), then just take the last
    // edge in the node. If we're in a leaf and we don't find our key, then it's not
    // in the tree.

    /// Returns a reference to the value corresponding to the key.
    ///
    /// The key may be any borrowed form of the map's key type, but the ordering
    /// on the borrowed form *must* match the ordering on the key type.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::collections::BTreeMap;
    ///
    /// let mut map = BTreeMap::new();
    /// map.insert(1u, "a");
    /// assert_eq!(map.get(&1), Some(&"a"));
    /// assert_eq!(map.get(&2), None);
    /// ```
    #[stable]
    pub fn get<Q: ?Sized>(&self, key: &Q) -> Option<&V> where Q: BorrowFrom<K> + Ord {
        let mut cur_node = &self.root;
        loop {
            match Node::search(cur_node, key) {
                Found(handle) => return Some(handle.into_kv().1),
                GoDown(handle) => match handle.force() {
                    Leaf(_) => return None,
                    Internal(internal_handle) => {
                        cur_node = internal_handle.into_edge();
                        continue;
                    }
                }
            }
        }
    }

    /// Returns true if the map contains a value for the specified key.
    ///
    /// The key may be any borrowed form of the map's key type, but the ordering
    /// on the borrowed form *must* match the ordering on the key type.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::collections::BTreeMap;
    ///
    /// let mut map = BTreeMap::new();
    /// map.insert(1u, "a");
    /// assert_eq!(map.contains_key(&1), true);
    /// assert_eq!(map.contains_key(&2), false);
    /// ```
    #[stable]
    pub fn contains_key<Q: ?Sized>(&self, key: &Q) -> bool where Q: BorrowFrom<K> + Ord {
        self.get(key).is_some()
    }

    /// Returns a mutable reference to the value corresponding to the key.
    ///
    /// The key may be any borrowed form of the map's key type, but the ordering
    /// on the borrowed form *must* match the ordering on the key type.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::collections::BTreeMap;
    ///
    /// let mut map = BTreeMap::new();
    /// map.insert(1u, "a");
    /// match map.get_mut(&1) {
    ///     Some(x) => *x = "b",
    ///     None => (),
    /// }
    /// assert_eq!(map[1], "b");
    /// ```
    // See `get` for implementation notes, this is basically a copy-paste with mut's added
    #[stable]
    pub fn get_mut<Q: ?Sized>(&mut self, key: &Q) -> Option<&mut V> where Q: BorrowFrom<K> + Ord {
        // temp_node is a Borrowck hack for having a mutable value outlive a loop iteration
        let mut temp_node = &mut self.root;
        loop {
            let cur_node = temp_node;
            match Node::search(cur_node, key) {
                Found(handle) => return Some(handle.into_kv_mut().1),
                GoDown(handle) => match handle.force() {
                    Leaf(_) => return None,
                    Internal(internal_handle) => {
                        temp_node = internal_handle.into_edge_mut();
                        continue;
                    }
                }
            }
        }
    }

    // Insertion in a B-Tree is a bit complicated.
    //
    // First we do the same kind of search described in `find`. But we need to maintain a stack of
    // all the nodes/edges in our search path. If we find a match for the key we're trying to
    // insert, just swap the vals and return the old ones. However, when we bottom out in a leaf,
    // we attempt to insert our key-value pair at the same location we would want to follow another
    // edge.
    //
    // If the node has room, then this is done in the obvious way by shifting elements. However,
    // if the node itself is full, we split node into two, and give its median key-value
    // pair to its parent to insert the new node with. Of course, the parent may also be
    // full, and insertion can propagate until we reach the root. If we reach the root, and
    // it is *also* full, then we split the root and place the two nodes under a newly made root.
    //
    // Note that we subtly deviate from Open Data Structures in our implementation of split.
    // ODS describes inserting into the node *regardless* of its capacity, and then
    // splitting *afterwards* if it happens to be overfull. However, this is inefficient.
    // Instead, we split beforehand, and then insert the key-value pair into the appropriate
    // result node. This has two consequences:
    //
    // 1) While ODS produces a left node of size B-1, and a right node of size B,
    // we may potentially reverse this. However, this shouldn't effect the analysis.
    //
    // 2) While ODS may potentially return the pair we *just* inserted after
    // the split, we will never do this. Again, this shouldn't effect the analysis.

    /// Inserts a key-value pair from the map. If the key already had a value
    /// present in the map, that value is returned. Otherwise, `None` is returned.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::collections::BTreeMap;
    ///
    /// let mut map = BTreeMap::new();
    /// assert_eq!(map.insert(37u, "a"), None);
    /// assert_eq!(map.is_empty(), false);
    ///
    /// map.insert(37, "b");
    /// assert_eq!(map.insert(37, "c"), Some("b"));
    /// assert_eq!(map[37], "c");
    /// ```
    #[stable]
    pub fn insert(&mut self, mut key: K, mut value: V) -> Option<V> {
        // This is a stack of rawptrs to nodes paired with indices, respectively
        // representing the nodes and edges of our search path. We have to store rawptrs
        // because as far as Rust is concerned, we can mutate aliased data with such a
        // stack. It is of course correct, but what it doesn't know is that we will only
        // be popping and using these ptrs one at a time in child-to-parent order. The alternative
        // to doing this is to take the Nodes from their parents. This actually makes
        // borrowck *really* happy and everything is pretty smooth. However, this creates
        // *tons* of pointless writes, and requires us to always walk all the way back to
        // the root after an insertion, even if we only needed to change a leaf. Therefore,
        // we accept this potential unsafety and complexity in the name of performance.
        //
        // Regardless, the actual dangerous logic is completely abstracted away from BTreeMap
        // by the stack module. All it can do is immutably read nodes, and ask the search stack
        // to proceed down some edge by index. This makes the search logic we'll be reusing in a
        // few different methods much neater, and of course drastically improves safety.
        let mut stack = stack::PartialSearchStack::new(self);

        loop {
            let result = stack.with(move |pusher, node| {
                // Same basic logic as found in `find`, but with PartialSearchStack mediating the
                // actual nodes for us
                return match Node::search(node, &key) {
                    Found(mut handle) => {
                        // Perfect match, swap the values and return the old one
                        mem::swap(handle.val_mut(), &mut value);
                        Finished(Some(value))
                    },
                    GoDown(handle) => {
                        // We need to keep searching, try to get the search stack
                        // to go down further
                        match handle.force() {
                            Leaf(leaf_handle) => {
                                // We've reached a leaf, perform the insertion here
                                pusher.seal(leaf_handle).insert(key, value);
                                Finished(None)
                            }
                            Internal(internal_handle) => {
                                // We've found the subtree to insert this key/value pair in,
                                // keep searching
                                Continue((pusher.push(internal_handle), key, value))
                            }
                        }
                    }
                }
            });
            match result {
                Finished(ret) => { return ret; },
                Continue((new_stack, renewed_key, renewed_val)) => {
                    stack = new_stack;
                    key = renewed_key;
                    value = renewed_val;
                }
            }
        }
    }

    // Deletion is the most complicated operation for a B-Tree.
    //
    // First we do the same kind of search described in
    // `find`. But we need to maintain a stack of all the nodes/edges in our search path.
    // If we don't find the key, then we just return `None` and do nothing. If we do find the
    // key, we perform two operations: remove the item, and then possibly handle underflow.
    //
    // # removing the item
    //      If the node is a leaf, we just remove the item, and shift
    //      any items after it back to fill the hole.
    //
    //      If the node is an internal node, we *swap* the item with the smallest item in
    //      in its right subtree (which must reside in a leaf), and then revert to the leaf
    //      case
    //
    // # handling underflow
    //      After removing an item, there may be too few items in the node. We want nodes
    //      to be mostly full for efficiency, although we make an exception for the root, which
    //      may have as few as one item. If this is the case, we may first try to steal
    //      an item from our left or right neighbour.
    //
    //      To steal from the left (right) neighbour,
    //      we take the largest (smallest) item and child from it. We then swap the taken item
    //      with the item in their mutual parent that separates them, and then insert the
    //      parent's item and the taken child into the first (last) index of the underflowed node.
    //
    //      However, stealing has the possibility of underflowing our neighbour. If this is the
    //      case, we instead *merge* with our neighbour. This of course reduces the number of
    //      children in the parent. Therefore, we also steal the item that separates the now
    //      merged nodes, and insert it into the merged node.
    //
    //      Merging may cause the parent to underflow. If this is the case, then we must repeat
    //      the underflow handling process on the parent. If merging merges the last two children
    //      of the root, then we replace the root with the merged node.

    /// Removes a key from the map, returning the value at the key if the key
    /// was previously in the map.
    ///
    /// The key may be any borrowed form of the map's key type, but the ordering
    /// on the borrowed form *must* match the ordering on the key type.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::collections::BTreeMap;
    ///
    /// let mut map = BTreeMap::new();
    /// map.insert(1u, "a");
    /// assert_eq!(map.remove(&1), Some("a"));
    /// assert_eq!(map.remove(&1), None);
    /// ```
    #[stable]
    pub fn remove<Q: ?Sized>(&mut self, key: &Q) -> Option<V> where Q: BorrowFrom<K> + Ord {
        // See `swap` for a more thorough description of the stuff going on in here
        let mut stack = stack::PartialSearchStack::new(self);
        loop {
            let result = stack.with(move |pusher, node| {
                return match Node::search(node, key) {
                    Found(handle) => {
                        // Perfect match. Terminate the stack here, and remove the entry
                        Finished(Some(pusher.seal(handle).remove()))
                    },
                    GoDown(handle) => {
                        // We need to keep searching, try to go down the next edge
                        match handle.force() {
                            // We're at a leaf; the key isn't in here
                            Leaf(_) => Finished(None),
                            Internal(internal_handle) => Continue(pusher.push(internal_handle))
                        }
                    }
                }
            });
            match result {
                Finished(ret) => return ret,
                Continue(new_stack) => stack = new_stack
            }
        }
    }
}

/// A helper enum useful for deciding whether to continue a loop since we can't
/// return from a closure
enum Continuation<A, B> {
    Continue(A),
    Finished(B)
}

/// The stack module provides a safe interface for constructing and manipulating a stack of ptrs
/// to nodes. By using this module much better safety guarantees can be made, and more search
/// boilerplate gets cut out.
mod stack {
    use core::prelude::*;
    use core::marker;
    use core::mem;
    use core::ops::{Deref, DerefMut};
    use super::BTreeMap;
    use super::super::node::{self, Node, Fit, Split, Internal, Leaf};
    use super::super::node::handle;
    use vec::Vec;

    /// A generic mutable reference, identical to `&mut` except for the fact that its lifetime
    /// parameter is invariant. This means that wherever an `IdRef` is expected, only an `IdRef`
    /// with the exact requested lifetime can be used. This is in contrast to normal references,
    /// where `&'static` can be used in any function expecting any lifetime reference.
    pub struct IdRef<'id, T: 'id> {
        inner: &'id mut T,
        marker: marker::InvariantLifetime<'id>
    }

    impl<'id, T> Deref for IdRef<'id, T> {
        type Target = T;

        fn deref(&self) -> &T {
            &*self.inner
        }
    }

    impl<'id, T> DerefMut for IdRef<'id, T> {
        fn deref_mut(&mut self) -> &mut T {
            &mut *self.inner
        }
    }

    type StackItem<K, V> = node::Handle<*mut Node<K, V>, handle::Edge, handle::Internal>;
    type Stack<K, V> = Vec<StackItem<K, V>>;

    /// A `PartialSearchStack` handles the construction of a search stack.
    pub struct PartialSearchStack<'a, K:'a, V:'a> {
        map: &'a mut BTreeMap<K, V>,
        stack: Stack<K, V>,
        next: *mut Node<K, V>,
    }

    /// A `SearchStack` represents a full path to an element or an edge of interest. It provides
    /// methods depending on the type of what the path points to for removing an element, inserting
    /// a new element, and manipulating to element at the top of the stack.
    pub struct SearchStack<'a, K:'a, V:'a, Type, NodeType> {
        map: &'a mut BTreeMap<K, V>,
        stack: Stack<K, V>,
        top: node::Handle<*mut Node<K, V>, Type, NodeType>,
    }

    /// A `PartialSearchStack` that doesn't hold a a reference to the next node, and is just
    /// just waiting for a `Handle` to that next node to be pushed. See `PartialSearchStack::with`
    /// for more details.
    pub struct Pusher<'id, 'a, K:'a, V:'a> {
        map: &'a mut BTreeMap<K, V>,
        stack: Stack<K, V>,
        marker: marker::InvariantLifetime<'id>
    }

    impl<'a, K, V> PartialSearchStack<'a, K, V> {
        /// Creates a new PartialSearchStack from a BTreeMap by initializing the stack with the
        /// root of the tree.
        pub fn new(map: &'a mut BTreeMap<K, V>) -> PartialSearchStack<'a, K, V> {
            let depth = map.depth;

            PartialSearchStack {
                next: &mut map.root as *mut _,
                map: map,
                stack: Vec::with_capacity(depth),
            }
        }

        /// Breaks up the stack into a `Pusher` and the next `Node`, allowing the given closure
        /// to interact with, search, and finally push the `Node` onto the stack. The passed in
        /// closure must be polymorphic on the `'id` lifetime parameter, as this statically
        /// ensures that only `Handle`s from the correct `Node` can be pushed.
        ///
        /// The reason this works is that the `Pusher` has an `'id` parameter, and will only accept
        /// handles with the same `'id`. The closure could only get references with that lifetime
        /// through its arguments or through some other `IdRef` that it has lying around. However,
        /// no other `IdRef` could possibly work - because the `'id` is held in an invariant
        /// parameter, it would need to have precisely the correct lifetime, which would mean that
        /// at least one of the calls to `with` wouldn't be properly polymorphic, wanting a
        /// specific lifetime instead of the one that `with` chooses to give it.
        ///
        /// See also Haskell's `ST` monad, which uses a similar trick.
        pub fn with<T, F: for<'id> FnOnce(Pusher<'id, 'a, K, V>,
                                          IdRef<'id, Node<K, V>>) -> T>(self, closure: F) -> T {
            let pusher = Pusher {
                map: self.map,
                stack: self.stack,
                marker: marker::InvariantLifetime
            };
            let node = IdRef {
                inner: unsafe { &mut *self.next },
                marker: marker::InvariantLifetime
            };

            closure(pusher, node)
        }
    }

    impl<'id, 'a, K, V> Pusher<'id, 'a, K, V> {
        /// Pushes the requested child of the stack's current top on top of the stack. If the child
        /// exists, then a new PartialSearchStack is yielded. Otherwise, a VacantSearchStack is
        /// yielded.
        pub fn push(mut self, mut edge: node::Handle<IdRef<'id, Node<K, V>>,
                                                     handle::Edge,
                                                     handle::Internal>)
                    -> PartialSearchStack<'a, K, V> {
            self.stack.push(edge.as_raw());
            PartialSearchStack {
                map: self.map,
                stack: self.stack,
                next: edge.edge_mut() as *mut _,
            }
        }

        /// Converts the PartialSearchStack into a SearchStack.
        pub fn seal<Type, NodeType>
                   (self, mut handle: node::Handle<IdRef<'id, Node<K, V>>, Type, NodeType>)
                    -> SearchStack<'a, K, V, Type, NodeType> {
            SearchStack {
                map: self.map,
                stack: self.stack,
                top: handle.as_raw(),
            }
        }
    }

    impl<'a, K, V, NodeType> SearchStack<'a, K, V, handle::KV, NodeType> {
        /// Gets a reference to the value the stack points to.
        pub fn peek(&self) -> &V {
            unsafe { self.top.from_raw().into_kv().1 }
        }

        /// Gets a mutable reference to the value the stack points to.
        pub fn peek_mut(&mut self) -> &mut V {
            unsafe { self.top.from_raw_mut().into_kv_mut().1 }
        }

        /// Converts the stack into a mutable reference to the value it points to, with a lifetime
        /// tied to the original tree.
        pub fn into_top(mut self) -> &'a mut V {
            unsafe {
                mem::copy_mut_lifetime(
                    self.map,
                    self.top.from_raw_mut().val_mut()
                )
            }
        }
    }

    impl<'a, K, V> SearchStack<'a, K, V, handle::KV, handle::Leaf> {
        /// Removes the key and value in the top element of the stack, then handles underflows as
        /// described in BTree's pop function.
        fn remove_leaf(mut self) -> V {
            self.map.length -= 1;

            // Remove the key-value pair from the leaf that this search stack points to.
            // Then, note if the leaf is underfull, and promptly forget the leaf and its ptr
            // to avoid ownership issues.
            let (value, mut underflow) = unsafe {
                let (_, value) = self.top.from_raw_mut().remove_as_leaf();
                let underflow = self.top.from_raw().node().is_underfull();
                (value, underflow)
            };

            loop {
                match self.stack.pop() {
                    None => {
                        // We've reached the root, so no matter what, we're done. We manually
                        // access the root via the tree itself to avoid creating any dangling
                        // pointers.
                        if self.map.root.len() == 0 && !self.map.root.is_leaf() {
                            // We've emptied out the root, so make its only child the new root.
                            // If it's a leaf, we just let it become empty.
                            self.map.depth -= 1;
                            self.map.root.hoist_lone_child();
                        }
                        return value;
                    }
                    Some(mut handle) => {
                        if underflow {
                            // Underflow! Handle it!
                            unsafe {
                                handle.from_raw_mut().handle_underflow();
                                underflow = handle.from_raw().node().is_underfull();
                            }
                        } else {
                            // All done!
                            return value;
                        }
                    }
                }
            }
        }
    }

    impl<'a, K, V> SearchStack<'a, K, V, handle::KV, handle::LeafOrInternal> {
        /// Removes the key and value in the top element of the stack, then handles underflows as
        /// described in BTree's pop function.
        pub fn remove(self) -> V {
            // Ensure that the search stack goes to a leaf. This is necessary to perform deletion
            // in a BTree. Note that this may put the tree in an inconsistent state (further
            // described in into_leaf's comments), but this is immediately fixed by the
            // removing the value we want to remove
            self.into_leaf().remove_leaf()
        }

        /// Subroutine for removal. Takes a search stack for a key that might terminate at an
        /// internal node, and mutates the tree and search stack to *make* it a search stack
        /// for that same key that *does* terminates at a leaf. If the mutation occurs, then this
        /// leaves the tree in an inconsistent state that must be repaired by the caller by
        /// removing the entry in question. Specifically the key-value pair and its successor will
        /// become swapped.
        fn into_leaf(mut self) -> SearchStack<'a, K, V, handle::KV, handle::Leaf> {
            unsafe {
                let mut top_raw = self.top;
                let mut top = top_raw.from_raw_mut();

                let key_ptr = top.key_mut() as *mut _;
                let val_ptr = top.val_mut() as *mut _;

                // Try to go into the right subtree of the found key to find its successor
                match top.force() {
                    Leaf(mut leaf_handle) => {
                        // We're a proper leaf stack, nothing to do
                        return SearchStack {
                            map: self.map,
                            stack: self.stack,
                            top: leaf_handle.as_raw()
                        }
                    }
                    Internal(mut internal_handle) => {
                        let mut right_handle = internal_handle.right_edge();

                        //We're not a proper leaf stack, let's get to work.
                        self.stack.push(right_handle.as_raw());

                        let mut temp_node = right_handle.edge_mut();
                        loop {
                            // Walk into the smallest subtree of this node
                            let node = temp_node;

                            match node.kv_handle(0).force() {
                                Leaf(mut handle) => {
                                    // This node is a leaf, do the swap and return
                                    mem::swap(handle.key_mut(), &mut *key_ptr);
                                    mem::swap(handle.val_mut(), &mut *val_ptr);
                                    return SearchStack {
                                        map: self.map,
                                        stack: self.stack,
                                        top: handle.as_raw()
                                    }
                                },
                                Internal(kv_handle) => {
                                    // This node is internal, go deeper
                                    let mut handle = kv_handle.into_left_edge();
                                    self.stack.push(handle.as_raw());
                                    temp_node = handle.into_edge_mut();
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    impl<'a, K, V> SearchStack<'a, K, V, handle::Edge, handle::Leaf> {
        /// Inserts the key and value into the top element in the stack, and if that node has to
        /// split recursively inserts the split contents into the next element stack until
        /// splits stop.
        ///
        /// Assumes that the stack represents a search path from the root to a leaf.
        ///
        /// An &mut V is returned to the inserted value, for callers that want a reference to this.
        pub fn insert(mut self, key: K, val: V) -> &'a mut V {
            unsafe {
                self.map.length += 1;

                // Insert the key and value into the leaf at the top of the stack
                let (mut insertion, inserted_ptr) = self.top.from_raw_mut()
                                                        .insert_as_leaf(key, val);

                loop {
                    match insertion {
                        Fit => {
                            // The last insertion went off without a hitch, no splits! We can stop
                            // inserting now.
                            return &mut *inserted_ptr;
                        }
                        Split(key, val, right) => match self.stack.pop() {
                            // The last insertion triggered a split, so get the next element on the
                            // stack to recursively insert the split node into.
                            None => {
                                // The stack was empty; we've split the root, and need to make a
                                // a new one. This is done in-place because we can't move the
                                // root out of a reference to the tree.
                                Node::make_internal_root(&mut self.map.root, self.map.b,
                                                         key, val, right);

                                self.map.depth += 1;
                                return &mut *inserted_ptr;
                            }
                            Some(mut handle) => {
                                // The stack wasn't empty, do the insertion and recurse
                                insertion = handle.from_raw_mut()
                                                  .insert_as_internal(key, val, right);
                                continue;
                            }
                        }
                    }
                }
            }
        }
    }
}

#[stable]
impl<K: Ord, V> FromIterator<(K, V)> for BTreeMap<K, V> {
    fn from_iter<T: Iterator<Item=(K, V)>>(iter: T) -> BTreeMap<K, V> {
        let mut map = BTreeMap::new();
        map.extend(iter);
        map
    }
}

#[stable]
impl<K: Ord, V> Extend<(K, V)> for BTreeMap<K, V> {
    #[inline]
    fn extend<T: Iterator<Item=(K, V)>>(&mut self, mut iter: T) {
        for (k, v) in iter {
            self.insert(k, v);
        }
    }
}

#[stable]
impl<S: Writer, K: Hash<S>, V: Hash<S>> Hash<S> for BTreeMap<K, V> {
    fn hash(&self, state: &mut S) {
        for elt in self.iter() {
            elt.hash(state);
        }
    }
}

#[stable]
impl<K: Ord, V> Default for BTreeMap<K, V> {
    #[stable]
    fn default() -> BTreeMap<K, V> {
        BTreeMap::new()
    }
}

#[stable]
impl<K: PartialEq, V: PartialEq> PartialEq for BTreeMap<K, V> {
    fn eq(&self, other: &BTreeMap<K, V>) -> bool {
        self.len() == other.len() &&
            self.iter().zip(other.iter()).all(|(a, b)| a == b)
    }
}

#[stable]
impl<K: Eq, V: Eq> Eq for BTreeMap<K, V> {}

#[stable]
impl<K: PartialOrd, V: PartialOrd> PartialOrd for BTreeMap<K, V> {
    #[inline]
    fn partial_cmp(&self, other: &BTreeMap<K, V>) -> Option<Ordering> {
        iter::order::partial_cmp(self.iter(), other.iter())
    }
}

#[stable]
impl<K: Ord, V: Ord> Ord for BTreeMap<K, V> {
    #[inline]
    fn cmp(&self, other: &BTreeMap<K, V>) -> Ordering {
        iter::order::cmp(self.iter(), other.iter())
    }
}

#[stable]
impl<K: Show, V: Show> Show for BTreeMap<K, V> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        try!(write!(f, "BTreeMap {{"));

        for (i, (k, v)) in self.iter().enumerate() {
            if i != 0 { try!(write!(f, ", ")); }
            try!(write!(f, "{:?}: {:?}", *k, *v));
        }

        write!(f, "}}")
    }
}

#[stable]
impl<K: Ord, Q: ?Sized, V> Index<Q> for BTreeMap<K, V>
    where Q: BorrowFrom<K> + Ord
{
    type Output = V;

    fn index(&self, key: &Q) -> &V {
        self.get(key).expect("no entry found for key")
    }
}

#[stable]
impl<K: Ord, Q: ?Sized, V> IndexMut<Q> for BTreeMap<K, V>
    where Q: BorrowFrom<K> + Ord
{
    type Output = V;

    fn index_mut(&mut self, key: &Q) -> &mut V {
        self.get_mut(key).expect("no entry found for key")
    }
}

/// Genericises over how to get the correct type of iterator from the correct type
/// of Node ownership.
trait Traverse<N> {
    fn traverse(node: N) -> Self;
}

impl<'a, K, V> Traverse<&'a Node<K, V>> for Traversal<'a, K, V> {
    fn traverse(node: &'a Node<K, V>) -> Traversal<'a, K, V> {
        node.iter()
    }
}

impl<'a, K, V> Traverse<&'a mut Node<K, V>> for MutTraversal<'a, K, V> {
    fn traverse(node: &'a mut Node<K, V>) -> MutTraversal<'a, K, V> {
        node.iter_mut()
    }
}

impl<K, V> Traverse<Node<K, V>> for MoveTraversal<K, V> {
    fn traverse(node: Node<K, V>) -> MoveTraversal<K, V> {
        node.into_iter()
    }
}

/// Represents an operation to perform inside the following iterator methods.
/// This is necessary to use in `next` because we want to modify self.left inside
/// a match that borrows it. Similarly, in `next_back` for self.right. Instead, we use this
/// enum to note what we want to do, and do it after the match.
enum StackOp<T> {
    Push(T),
    Pop,
}

impl<K, V, E, T> Iterator for AbsIter<T> where
    T: DoubleEndedIterator<Item=TraversalItem<K, V, E>> + Traverse<E>,
{
    type Item = (K, V);

    // This function is pretty long, but only because there's a lot of cases to consider.
    // Our iterator represents two search paths, left and right, to the smallest and largest
    // elements we have yet to yield. lca represents the least common ancestor of these two paths,
    // above-which we never walk, since everything outside it has already been consumed (or was
    // never in the range to iterate).
    //
    // Note that the design of these iterators permits an *arbitrary* initial pair of min and max,
    // making these arbitrary sub-range iterators. However the logic to construct these paths
    // efficiently is fairly involved, so this is a FIXME. The sub-range iterators also wouldn't be
    // able to accurately predict size, so those iterators can't implement ExactSizeIterator.
    fn next(&mut self) -> Option<(K, V)> {
        loop {
            // We want the smallest element, so try to get the top of the left stack
            let op = match self.left.back_mut() {
                // The left stack is empty, so try to get the next element of the two paths
                // LCAs (the left search path is currently a subpath of the right one)
                None => match self.lca.next() {
                    // The lca has been exhausted, walk further down the right path
                    None => match self.right.pop_front() {
                        // The right path is exhausted, so we're done
                        None => return None,
                        // The right path had something, make that the new LCA
                        // and restart the whole process
                        Some(right) => {
                            self.lca = right;
                            continue;
                        }
                    },
                    // The lca yielded an edge, make that the new head of the left path
                    Some(Edge(next)) => Push(Traverse::traverse(next)),
                    // The lca yielded an entry, so yield that
                    Some(Elem(k, v)) => {
                        self.size -= 1;
                        return Some((k, v))
                    }
                },
                // The left stack wasn't empty, so continue along the node in its head
                Some(iter) => match iter.next() {
                    // The head of the left path is empty, so Pop it off and restart the process
                    None => Pop,
                    // The head of the left path yielded an edge, so make that the new head
                    // of the left path
                    Some(Edge(next)) => Push(Traverse::traverse(next)),
                    // The head of the left path yielded entry, so yield that
                    Some(Elem(k, v)) => {
                        self.size -= 1;
                        return Some((k, v))
                    }
                }
            };

            // Handle any operation on the left stack as necessary
            match op {
                Push(item) => { self.left.push_back(item); },
                Pop => { self.left.pop_back(); },
            }
        }
    }

    fn size_hint(&self) -> (uint, Option<uint>) {
        (self.size, Some(self.size))
    }
}

impl<K, V, E, T> DoubleEndedIterator for AbsIter<T> where
    T: DoubleEndedIterator<Item=TraversalItem<K, V, E>> + Traverse<E>,
{
    // next_back is totally symmetric to next
    fn next_back(&mut self) -> Option<(K, V)> {
        loop {
            let op = match self.right.back_mut() {
                None => match self.lca.next_back() {
                    None => match self.left.pop_front() {
                        None => return None,
                        Some(left) => {
                            self.lca = left;
                            continue;
                        }
                    },
                    Some(Edge(next)) => Push(Traverse::traverse(next)),
                    Some(Elem(k, v)) => {
                        self.size -= 1;
                        return Some((k, v))
                    }
                },
                Some(iter) => match iter.next_back() {
                    None => Pop,
                    Some(Edge(next)) => Push(Traverse::traverse(next)),
                    Some(Elem(k, v)) => {
                        self.size -= 1;
                        return Some((k, v))
                    }
                }
            };

            match op {
                Push(item) => { self.right.push_back(item); },
                Pop => { self.right.pop_back(); }
            }
        }
    }
}

#[stable]
impl<'a, K, V> Iterator for Iter<'a, K, V> {
    type Item = (&'a K, &'a V);

    fn next(&mut self) -> Option<(&'a K, &'a V)> { self.inner.next() }
    fn size_hint(&self) -> (uint, Option<uint>) { self.inner.size_hint() }
}
#[stable]
impl<'a, K, V> DoubleEndedIterator for Iter<'a, K, V> {
    fn next_back(&mut self) -> Option<(&'a K, &'a V)> { self.inner.next_back() }
}
#[stable]
impl<'a, K, V> ExactSizeIterator for Iter<'a, K, V> {}

#[stable]
impl<'a, K, V> Iterator for IterMut<'a, K, V> {
    type Item = (&'a K, &'a mut V);

    fn next(&mut self) -> Option<(&'a K, &'a mut V)> { self.inner.next() }
    fn size_hint(&self) -> (uint, Option<uint>) { self.inner.size_hint() }
}
#[stable]
impl<'a, K, V> DoubleEndedIterator for IterMut<'a, K, V> {
    fn next_back(&mut self) -> Option<(&'a K, &'a mut V)> { self.inner.next_back() }
}
#[stable]
impl<'a, K, V> ExactSizeIterator for IterMut<'a, K, V> {}

#[stable]
impl<K, V> Iterator for IntoIter<K, V> {
    type Item = (K, V);

    fn next(&mut self) -> Option<(K, V)> { self.inner.next() }
    fn size_hint(&self) -> (uint, Option<uint>) { self.inner.size_hint() }
}
#[stable]
impl<K, V> DoubleEndedIterator for IntoIter<K, V> {
    fn next_back(&mut self) -> Option<(K, V)> { self.inner.next_back() }
}
#[stable]
impl<K, V> ExactSizeIterator for IntoIter<K, V> {}

#[stable]
impl<'a, K, V> Iterator for Keys<'a, K, V> {
    type Item = &'a K;

    fn next(&mut self) -> Option<(&'a K)> { self.inner.next() }
    fn size_hint(&self) -> (uint, Option<uint>) { self.inner.size_hint() }
}
#[stable]
impl<'a, K, V> DoubleEndedIterator for Keys<'a, K, V> {
    fn next_back(&mut self) -> Option<(&'a K)> { self.inner.next_back() }
}
#[stable]
impl<'a, K, V> ExactSizeIterator for Keys<'a, K, V> {}


#[stable]
impl<'a, K, V> Iterator for Values<'a, K, V> {
    type Item = &'a V;

    fn next(&mut self) -> Option<(&'a V)> { self.inner.next() }
    fn size_hint(&self) -> (uint, Option<uint>) { self.inner.size_hint() }
}
#[stable]
impl<'a, K, V> DoubleEndedIterator for Values<'a, K, V> {
    fn next_back(&mut self) -> Option<(&'a V)> { self.inner.next_back() }
}
#[stable]
impl<'a, K, V> ExactSizeIterator for Values<'a, K, V> {}

impl<'a, K: Ord, V> Entry<'a, K, V> {
    #[unstable = "matches collection reform v2 specification, waiting for dust to settle"]
    /// Returns a mutable reference to the entry if occupied, or the VacantEntry if vacant
    pub fn get(self) -> Result<&'a mut V, VacantEntry<'a, K, V>> {
        match self {
            Occupied(entry) => Ok(entry.into_mut()),
            Vacant(entry) => Err(entry),
        }
    }
}

impl<'a, K: Ord, V> VacantEntry<'a, K, V> {
    /// Sets the value of the entry with the VacantEntry's key,
    /// and returns a mutable reference to it.
    #[unstable = "matches collection reform v2 specification, waiting for dust to settle"]
    pub fn insert(self, value: V) -> &'a mut V {
        self.stack.insert(self.key, value)
    }
}

impl<'a, K: Ord, V> OccupiedEntry<'a, K, V> {
    /// Gets a reference to the value in the entry.
    #[unstable = "matches collection reform v2 specification, waiting for dust to settle"]
    pub fn get(&self) -> &V {
        self.stack.peek()
    }

    /// Gets a mutable reference to the value in the entry.
    #[unstable = "matches collection reform v2 specification, waiting for dust to settle"]
    pub fn get_mut(&mut self) -> &mut V {
        self.stack.peek_mut()
    }

    /// Converts the entry into a mutable reference to its value.
    #[unstable = "matches collection reform v2 specification, waiting for dust to settle"]
    pub fn into_mut(self) -> &'a mut V {
        self.stack.into_top()
    }

    /// Sets the value of the entry with the OccupiedEntry's key,
    /// and returns the entry's old value.
    #[unstable = "matches collection reform v2 specification, waiting for dust to settle"]
    pub fn insert(&mut self, mut value: V) -> V {
        mem::swap(self.stack.peek_mut(), &mut value);
        value
    }

    /// Takes the value of the entry out of the map, and returns it.
    #[unstable = "matches collection reform v2 specification, waiting for dust to settle"]
    pub fn remove(self) -> V {
        self.stack.remove()
    }
}

impl<K, V> BTreeMap<K, V> {
    /// Gets an iterator over the entries of the map.
    ///
    /// # Example
    ///
    /// ```
    /// use std::collections::BTreeMap;
    ///
    /// let mut map = BTreeMap::new();
    /// map.insert(1u, "a");
    /// map.insert(2u, "b");
    /// map.insert(3u, "c");
    ///
    /// for (key, value) in map.iter() {
    ///     println!("{}: {}", key, value);
    /// }
    ///
    /// let (first_key, first_value) = map.iter().next().unwrap();
    /// assert_eq!((*first_key, *first_value), (1u, "a"));
    /// ```
    #[stable]
    pub fn iter(&self) -> Iter<K, V> {
        let len = self.len();
        Iter {
            inner: AbsIter {
                lca: Traverse::traverse(&self.root),
                left: RingBuf::new(),
                right: RingBuf::new(),
                size: len,
            }
        }
    }

    /// Gets a mutable iterator over the entries of the map.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::collections::BTreeMap;
    ///
    /// let mut map = BTreeMap::new();
    /// map.insert("a", 1u);
    /// map.insert("b", 2u);
    /// map.insert("c", 3u);
    ///
    /// // add 10 to the value if the key isn't "a"
    /// for (key, value) in map.iter_mut() {
    ///     if key != &"a" {
    ///         *value += 10;
    ///     }
    /// }
    /// ```
    #[stable]
    pub fn iter_mut(&mut self) -> IterMut<K, V> {
        let len = self.len();
        IterMut {
            inner: AbsIter {
                lca: Traverse::traverse(&mut self.root),
                left: RingBuf::new(),
                right: RingBuf::new(),
                size: len,
            }
        }
    }

    /// Gets an owning iterator over the entries of the map.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::collections::BTreeMap;
    ///
    /// let mut map = BTreeMap::new();
    /// map.insert(1u, "a");
    /// map.insert(2u, "b");
    /// map.insert(3u, "c");
    ///
    /// for (key, value) in map.into_iter() {
    ///     println!("{}: {}", key, value);
    /// }
    /// ```
    #[stable]
    pub fn into_iter(self) -> IntoIter<K, V> {
        let len = self.len();
        IntoIter {
            inner: AbsIter {
                lca: Traverse::traverse(self.root),
                left: RingBuf::new(),
                right: RingBuf::new(),
                size: len,
            }
        }
    }

    /// Gets an iterator over the keys of the map.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::collections::BTreeMap;
    ///
    /// let mut a = BTreeMap::new();
    /// a.insert(1u, "a");
    /// a.insert(2u, "b");
    ///
    /// let keys: Vec<uint> = a.keys().cloned().collect();
    /// assert_eq!(keys, vec![1u,2,]);
    /// ```
    #[stable]
    pub fn keys<'a>(&'a self) -> Keys<'a, K, V> {
        fn first<A, B>((a, _): (A, B)) -> A { a }
        let first: fn((&'a K, &'a V)) -> &'a K = first; // coerce to fn pointer

        Keys { inner: self.iter().map(first) }
    }

    /// Gets an iterator over the values of the map.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::collections::BTreeMap;
    ///
    /// let mut a = BTreeMap::new();
    /// a.insert(1u, "a");
    /// a.insert(2u, "b");
    ///
    /// let values: Vec<&str> = a.values().cloned().collect();
    /// assert_eq!(values, vec!["a","b"]);
    /// ```
    #[stable]
    pub fn values<'a>(&'a self) -> Values<'a, K, V> {
        fn second<A, B>((_, b): (A, B)) -> B { b }
        let second: fn((&'a K, &'a V)) -> &'a V = second; // coerce to fn pointer

        Values { inner: self.iter().map(second) }
    }

    /// Return the number of elements in the map.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::collections::BTreeMap;
    ///
    /// let mut a = BTreeMap::new();
    /// assert_eq!(a.len(), 0);
    /// a.insert(1u, "a");
    /// assert_eq!(a.len(), 1);
    /// ```
    #[stable]
    pub fn len(&self) -> uint { self.length }

    /// Return true if the map contains no elements.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::collections::BTreeMap;
    ///
    /// let mut a = BTreeMap::new();
    /// assert!(a.is_empty());
    /// a.insert(1u, "a");
    /// assert!(!a.is_empty());
    /// ```
    #[stable]
    pub fn is_empty(&self) -> bool { self.len() == 0 }
}

impl<K: Ord, V> BTreeMap<K, V> {
    /// Gets the given key's corresponding entry in the map for in-place manipulation.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::collections::BTreeMap;
    /// use std::collections::btree_map::Entry;
    ///
    /// let mut count: BTreeMap<&str, uint> = BTreeMap::new();
    ///
    /// // count the number of occurrences of letters in the vec
    /// for x in vec!["a","b","a","c","a","b"].iter() {
    ///     match count.entry(*x) {
    ///         Entry::Vacant(view) => {
    ///             view.insert(1);
    ///         },
    ///         Entry::Occupied(mut view) => {
    ///             let v = view.get_mut();
    ///             *v += 1;
    ///         },
    ///     }
    /// }
    ///
    /// assert_eq!(count["a"], 3u);
    /// ```
    /// The key must have the same ordering before or after `.to_owned()` is called.
    #[unstable = "precise API still under development"]
    pub fn entry<'a>(&'a mut self, mut key: K) -> Entry<'a, K, V> {
        // same basic logic of `swap` and `pop`, blended together
        let mut stack = stack::PartialSearchStack::new(self);
        loop {
            let result = stack.with(move |pusher, node| {
                return match Node::search(node, &key) {
                    Found(handle) => {
                        // Perfect match
                        Finished(Occupied(OccupiedEntry {
                            stack: pusher.seal(handle)
                        }))
                    },
                    GoDown(handle) => {
                        match handle.force() {
                            Leaf(leaf_handle) => {
                                Finished(Vacant(VacantEntry {
                                    stack: pusher.seal(leaf_handle),
                                    key: key,
                                }))
                            },
                            Internal(internal_handle) => {
                                Continue((
                                    pusher.push(internal_handle),
                                    key
                                ))
                            }
                        }
                    }
                }
            });
            match result {
                Finished(finished) => return finished,
                Continue((new_stack, renewed_key)) => {
                    stack = new_stack;
                    key = renewed_key;
                }
            }
        }
    }
}





#[cfg(test)]
mod test {
    use prelude::*;
    use std::borrow::BorrowFrom;

    use super::{BTreeMap, Occupied, Vacant};

    #[test]
    fn test_basic_large() {
        let mut map = BTreeMap::new();
        let size = 10000u;
        assert_eq!(map.len(), 0);

        for i in range(0, size) {
            assert_eq!(map.insert(i, 10*i), None);
            assert_eq!(map.len(), i + 1);
        }

        for i in range(0, size) {
            assert_eq!(map.get(&i).unwrap(), &(i*10));
        }

        for i in range(size, size*2) {
            assert_eq!(map.get(&i), None);
        }

        for i in range(0, size) {
            assert_eq!(map.insert(i, 100*i), Some(10*i));
            assert_eq!(map.len(), size);
        }

        for i in range(0, size) {
            assert_eq!(map.get(&i).unwrap(), &(i*100));
        }

        for i in range(0, size/2) {
            assert_eq!(map.remove(&(i*2)), Some(i*200));
            assert_eq!(map.len(), size - i - 1);
        }

        for i in range(0, size/2) {
            assert_eq!(map.get(&(2*i)), None);
            assert_eq!(map.get(&(2*i+1)).unwrap(), &(i*200 + 100));
        }

        for i in range(0, size/2) {
            assert_eq!(map.remove(&(2*i)), None);
            assert_eq!(map.remove(&(2*i+1)), Some(i*200 + 100));
            assert_eq!(map.len(), size/2 - i - 1);
        }
    }

    #[test]
    fn test_basic_small() {
        let mut map = BTreeMap::new();
        assert_eq!(map.remove(&1), None);
        assert_eq!(map.get(&1), None);
        assert_eq!(map.insert(1u, 1u), None);
        assert_eq!(map.get(&1), Some(&1));
        assert_eq!(map.insert(1, 2), Some(1));
        assert_eq!(map.get(&1), Some(&2));
        assert_eq!(map.insert(2, 4), None);
        assert_eq!(map.get(&2), Some(&4));
        assert_eq!(map.remove(&1), Some(2));
        assert_eq!(map.remove(&2), Some(4));
        assert_eq!(map.remove(&1), None);
    }

    #[test]
    fn test_iter() {
        let size = 10000u;

        // Forwards
        let mut map: BTreeMap<uint, uint> = range(0, size).map(|i| (i, i)).collect();

        {
            let mut iter = map.iter();
            for i in range(0, size) {
                assert_eq!(iter.size_hint(), (size - i, Some(size - i)));
                assert_eq!(iter.next().unwrap(), (&i, &i));
            }
            assert_eq!(iter.size_hint(), (0, Some(0)));
            assert_eq!(iter.next(), None);
        }

        {
            let mut iter = map.iter_mut();
            for i in range(0, size) {
                assert_eq!(iter.size_hint(), (size - i, Some(size - i)));
                assert_eq!(iter.next().unwrap(), (&i, &mut (i + 0)));
            }
            assert_eq!(iter.size_hint(), (0, Some(0)));
            assert_eq!(iter.next(), None);
        }

        {
            let mut iter = map.into_iter();
            for i in range(0, size) {
                assert_eq!(iter.size_hint(), (size - i, Some(size - i)));
                assert_eq!(iter.next().unwrap(), (i, i));
            }
            assert_eq!(iter.size_hint(), (0, Some(0)));
            assert_eq!(iter.next(), None);
        }

    }

    #[test]
    fn test_iter_rev() {
        let size = 10000u;

        // Forwards
        let mut map: BTreeMap<uint, uint> = range(0, size).map(|i| (i, i)).collect();

        {
            let mut iter = map.iter().rev();
            for i in range(0, size) {
                assert_eq!(iter.size_hint(), (size - i, Some(size - i)));
                assert_eq!(iter.next().unwrap(), (&(size - i - 1), &(size - i - 1)));
            }
            assert_eq!(iter.size_hint(), (0, Some(0)));
            assert_eq!(iter.next(), None);
        }

        {
            let mut iter = map.iter_mut().rev();
            for i in range(0, size) {
                assert_eq!(iter.size_hint(), (size - i, Some(size - i)));
                assert_eq!(iter.next().unwrap(), (&(size - i - 1), &mut(size - i - 1)));
            }
            assert_eq!(iter.size_hint(), (0, Some(0)));
            assert_eq!(iter.next(), None);
        }

        {
            let mut iter = map.into_iter().rev();
            for i in range(0, size) {
                assert_eq!(iter.size_hint(), (size - i, Some(size - i)));
                assert_eq!(iter.next().unwrap(), (size - i - 1, size - i - 1));
            }
            assert_eq!(iter.size_hint(), (0, Some(0)));
            assert_eq!(iter.next(), None);
        }

    }

    #[test]
    fn test_entry(){
        let xs = [(1i, 10i), (2, 20), (3, 30), (4, 40), (5, 50), (6, 60)];

        let mut map: BTreeMap<int, int> = xs.iter().map(|&x| x).collect();

        // Existing key (insert)
        match map.entry(1) {
            Vacant(_) => unreachable!(),
            Occupied(mut view) => {
                assert_eq!(view.get(), &10);
                assert_eq!(view.insert(100), 10);
            }
        }
        assert_eq!(map.get(&1).unwrap(), &100);
        assert_eq!(map.len(), 6);


        // Existing key (update)
        match map.entry(2) {
            Vacant(_) => unreachable!(),
            Occupied(mut view) => {
                let v = view.get_mut();
                *v *= 10;
            }
        }
        assert_eq!(map.get(&2).unwrap(), &200);
        assert_eq!(map.len(), 6);

        // Existing key (take)
        match map.entry(3) {
            Vacant(_) => unreachable!(),
            Occupied(view) => {
                assert_eq!(view.remove(), 30);
            }
        }
        assert_eq!(map.get(&3), None);
        assert_eq!(map.len(), 5);


        // Inexistent key (insert)
        match map.entry(10) {
            Occupied(_) => unreachable!(),
            Vacant(view) => {
                assert_eq!(*view.insert(1000), 1000);
            }
        }
        assert_eq!(map.get(&10).unwrap(), &1000);
        assert_eq!(map.len(), 6);
    }
}






#[cfg(test)]
mod bench {
    use prelude::*;
    use std::rand::{weak_rng, Rng};
    use test::{Bencher, black_box};

    use super::BTreeMap;
    use bench::{insert_rand_n, insert_seq_n, find_rand_n, find_seq_n};

    #[bench]
    pub fn insert_rand_100(b: &mut Bencher) {
        let mut m : BTreeMap<uint,uint> = BTreeMap::new();
        insert_rand_n(100, &mut m, b,
                      |m, i| { m.insert(i, 1); },
                      |m, i| { m.remove(&i); });
    }

    #[bench]
    pub fn insert_rand_10_000(b: &mut Bencher) {
        let mut m : BTreeMap<uint,uint> = BTreeMap::new();
        insert_rand_n(10_000, &mut m, b,
                      |m, i| { m.insert(i, 1); },
                      |m, i| { m.remove(&i); });
    }

    // Insert seq
    #[bench]
    pub fn insert_seq_100(b: &mut Bencher) {
        let mut m : BTreeMap<uint,uint> = BTreeMap::new();
        insert_seq_n(100, &mut m, b,
                     |m, i| { m.insert(i, 1); },
                     |m, i| { m.remove(&i); });
    }

    #[bench]
    pub fn insert_seq_10_000(b: &mut Bencher) {
        let mut m : BTreeMap<uint,uint> = BTreeMap::new();
        insert_seq_n(10_000, &mut m, b,
                     |m, i| { m.insert(i, 1); },
                     |m, i| { m.remove(&i); });
    }

    // Find rand
    #[bench]
    pub fn find_rand_100(b: &mut Bencher) {
        let mut m : BTreeMap<uint,uint> = BTreeMap::new();
        find_rand_n(100, &mut m, b,
                    |m, i| { m.insert(i, 1); },
                    |m, i| { m.get(&i); });
    }

    #[bench]
    pub fn find_rand_10_000(b: &mut Bencher) {
        let mut m : BTreeMap<uint,uint> = BTreeMap::new();
        find_rand_n(10_000, &mut m, b,
                    |m, i| { m.insert(i, 1); },
                    |m, i| { m.get(&i); });
    }

    // Find seq
    #[bench]
    pub fn find_seq_100(b: &mut Bencher) {
        let mut m : BTreeMap<uint,uint> = BTreeMap::new();
        find_seq_n(100, &mut m, b,
                   |m, i| { m.insert(i, 1); },
                   |m, i| { m.get(&i); });
    }

    #[bench]
    pub fn find_seq_10_000(b: &mut Bencher) {
        let mut m : BTreeMap<uint,uint> = BTreeMap::new();
        find_seq_n(10_000, &mut m, b,
                   |m, i| { m.insert(i, 1); },
                   |m, i| { m.get(&i); });
    }

    fn bench_iter(b: &mut Bencher, size: uint) {
        let mut map = BTreeMap::<uint, uint>::new();
        let mut rng = weak_rng();

        for _ in range(0, size) {
            map.insert(rng.gen(), rng.gen());
        }

        b.iter(|| {
            for entry in map.iter() {
                black_box(entry);
            }
        });
    }

    #[bench]
    pub fn iter_20(b: &mut Bencher) {
        bench_iter(b, 20);
    }

    #[bench]
    pub fn iter_1000(b: &mut Bencher) {
        bench_iter(b, 1000);
    }

    #[bench]
    pub fn iter_100000(b: &mut Bencher) {
        bench_iter(b, 100000);
    }
}
