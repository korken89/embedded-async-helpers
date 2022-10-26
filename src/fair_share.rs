//! A mutex which internally uses an intrusive linked list to store nodes in the waiting `Futures`.
//! Read/write access is guaranteed fair, with FIFO semantics.
//!
//! **WARNING:** Don't `mem::forget` the access future, or there will be dragons.

use core::{
    cell::UnsafeCell,
    future::Future,
    ops::{Deref, DerefMut},
    pin::Pin,
    ptr::NonNull,
    task::{Context, Poll, Waker},
};

// Wrapper type of queue placement.
#[derive(Clone, Copy, PartialEq, PartialOrd, Eq, Ord)]
struct FairSharePlace(usize);

struct FairShareManagement {
    idx_in: FairSharePlace,
    idx_out: FairSharePlace,
    queue: IntrusiveSinglyLinkedList,
}

struct IntrusiveWakerNode {
    waker: Waker,
    place: FairSharePlace,
    next: Option<NonNull<IntrusiveWakerNode>>,
}

impl IntrusiveWakerNode {
    const fn new(waker: Waker) -> Self {
        IntrusiveWakerNode {
            waker,
            place: FairSharePlace(0),
            next: None,
        }
    }
}

struct IntrusiveSinglyLinkedList {
    head: Option<NonNull<IntrusiveWakerNode>>,
    tail: Option<NonNull<IntrusiveWakerNode>>,
}

unsafe impl Send for IntrusiveSinglyLinkedList {}

impl IntrusiveSinglyLinkedList {
    const fn new() -> Self {
        IntrusiveSinglyLinkedList {
            head: None,
            tail: None,
        }
    }

    fn is_empty(&self) -> bool {
        self.head.is_none()
    }

    fn peek(&self) -> Option<&IntrusiveWakerNode> {
        self.head.map(|v| unsafe { v.as_ref() })
    }

    fn push_back(&mut self, val: NonNull<IntrusiveWakerNode>) {
        if self.head.is_some() {
            if let Some(mut tail) = self.tail {
                unsafe { tail.as_mut() }.next = Some(val);
                self.tail = Some(val);
            } else {
                // If head is some, tail is some
                unreachable!()
            }
        } else {
            self.head = Some(val);
            self.tail = self.head;
        }
    }

    fn pop_head(&mut self) -> Option<NonNull<IntrusiveWakerNode>> {
        if let Some(head) = self.head {
            if self.head != self.tail {
                let ret = self.head;
                self.head = unsafe { head.as_ref() }.next;
                return ret;
            } else {
                // The list is empty
                self.head = None;
                self.tail = None;
            }
        }

        None
    }

    // Return the index if it was first in queue to update the counters
    fn pop_idx(&mut self, idx: FairSharePlace) -> Option<FairSharePlace> {
        // 1. Check if head should be replaced
        if let Some(head) = self.head {
            let head = unsafe { head.as_ref() };

            if head.place == idx {
                if self.head != self.tail {
                    // There are more than 1 element in the list
                    self.head = head.next;
                } else {
                    // There is only 1 element in the list
                    self.head = None;
                    self.tail = None;
                }

                return Some(head.place);
            }
        } else {
            // The list is empty
            return None;
        }

        // 2. It was not the first element, search for it
        let mut head = self.head;

        while let Some(mut h) = head {
            let h = unsafe { h.as_mut() };

            // Check if the next node is the one that should be removed
            if let Some(next) = h.next {
                let next = unsafe { next.as_ref() };
                if next.place == idx {
                    // Replace with what's after next
                    h.next = next.next;
                    break;
                }
            }

            head = h.next;
        }

        None
    }

    fn print(&self) {
        defmt::debug!(
            "LinkedList (head = {:x}, tail = {:x}):",
            self.head.map(|v| v.as_ptr() as u32),
            self.tail.map(|v| v.as_ptr() as u32)
        );

        let mut head = self.head;

        while let Some(h) = head {
            let h2 = unsafe { h.as_ref() };

            defmt::debug!(
                "    {} ({:x}): next = {:x}",
                h2.place.0,
                h.as_ptr() as u32,
                h2.next.map(|v| v.as_ptr() as u32)
            );

            head = h2.next;
        }
    }
}

impl FairShareManagement {
    fn enqueue(&mut self, node: &mut IntrusiveWakerNode) -> FairSharePlace {
        defmt::debug!("    running enqueue (pre)");
        self.queue.print();

        let current = self.idx_in;
        self.idx_in = FairSharePlace(current.0.wrapping_add(1));

        node.place = current;

        defmt::debug!("Enqueueing waker at place {}", current.0);

        self.queue.push_back(node.into());

        defmt::debug!("    running enqueue (post)");
        self.queue.print();

        current
    }

    fn dequeue(&mut self) {
        defmt::debug!("    running dequeue (pre)");
        self.queue.print();

        if let Some(node) = self.queue.pop_head() {
            let node = unsafe { node.as_ref() };
            defmt::debug!("    running dequeue place = {}", node.place.0);
        }

        defmt::debug!("    running dequeue (post)");
        self.queue.print();
    }

    fn wake_next_in_queue(&mut self) -> Option<Waker> {
        if let Some(node) = self.queue.peek() {
            defmt::debug!("Waking waker at place {}", node.place.0);
            self.idx_out = node.place;

            Some(node.waker.clone())
        } else {
            self.idx_out = self.idx_in;
            defmt::debug!("Waking no waker");

            None
        }
    }

    fn try_direct_access(&mut self) -> bool {
        if self.queue.is_empty() && self.idx_in == self.idx_out {
            // Update current counters to not get races
            let current = self.idx_in;
            self.idx_in = FairSharePlace(current.0.wrapping_add(1));

            defmt::debug!("Direct access granted");

            true
        } else {
            defmt::debug!("Direct access denied");

            false
        }
    }
}

/// Async fair sharing of an underlying value.
pub struct FairShare<T> {
    /// Holds the underying type, this can only safely be accessed from `FairShareExclusiveAccess`.
    storage: UnsafeCell<T>,
    /// Holds queue handling, this is guarded with critical section tokens.
    management: UnsafeCell<FairShareManagement>,
}

unsafe impl<T> Sync for FairShare<T> {}

impl<T> FairShare<T> {
    /// Create a new fair share, generally place this in static storage and pass around references.
    pub const fn new(val: T) -> Self {
        FairShare {
            storage: UnsafeCell::new(val),
            management: UnsafeCell::new(FairShareManagement {
                idx_in: FairSharePlace(0),
                idx_out: FairSharePlace(0),
                // queue_head: None,
                // queue_tail: None,
                queue: IntrusiveSinglyLinkedList::new(),
            }),
        }
    }

    fn get_management<'a>(
        &self,
        _token: &'a mut critical_section::CriticalSection,
    ) -> &'a mut FairShareManagement {
        // Safety: Get the underlying storage if we are in a critical section
        unsafe { &mut *(self.management.get()) }
    }

    /// Request access, await the returned future to be woken when its available.
    pub fn access<'a>(&'a self) -> FairShareAccessFuture<'a, T> {
        FairShareAccessFuture {
            fs: self,
            node: UnsafeCell::new(None),
            place: None,
        }
    }
}

/// Access future.
pub struct FairShareAccessFuture<'a, T> {
    fs: &'a FairShare<T>,
    node: UnsafeCell<Option<IntrusiveWakerNode>>,
    place: Option<FairSharePlace>,
}

impl<'a, T> Drop for FairShareAccessFuture<'a, T> {
    fn drop(&mut self) {
        defmt::warn!("Dropping access future");
        if let Some(place) = self.place {
            critical_section::with(|mut token| {
                let fs = self.fs.get_management(&mut token);

                // Remove this from the queue

                defmt::debug!("Removing index");

                // TODO: Test this, see if we need to fix indexes somehow
                if let Some(head_idx) = fs.queue.pop_idx(place) {
                    defmt::debug!("Removing removed head = {}", head_idx.0);
                    // fs.idx_out = head_idx;
                }
            });
        }
    }
}

impl<'a, T> Future for FairShareAccessFuture<'a, T> {
    type Output = FairShareExclusiveAccess<'a, T>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        critical_section::with(|mut token| {
            let fs = self.fs.get_management(&mut token);
            defmt::debug!(
                "poll place = {}, idx in = {}, idx out = {}",
                self.place.map(|v| v.0),
                fs.idx_in.0,
                fs.idx_out.0
            );

            if let Some(place) = self.place {
                if fs.idx_out == place {
                    // Our turn
                    fs.dequeue();
                    defmt::debug!("{}: Exclusive access granted", place.0);
                    self.place = None;
                    Poll::Ready(FairShareExclusiveAccess { fs: self.fs })
                } else {
                    // Continue waiting
                    defmt::debug!("{}: Waiting for exclusive access", place.0);
                    Poll::Pending
                }
            } else {
                // Check if the queue is empty, then we don't need to wait
                if fs.try_direct_access() {
                    Poll::Ready(FairShareExclusiveAccess { fs: self.fs })
                } else {
                    let node = self
                        .node
                        .get_mut()
                        .insert(IntrusiveWakerNode::new(cx.waker().clone()));

                    // We are not in the queue yet, enqueue our waker
                    self.place = Some(fs.enqueue(node));

                    // self.place = Some(fs.enqueue());
                    defmt::debug!("{}: Waiting for exclusive access", self.place.unwrap().0);
                    Poll::Pending
                }
            }
        })
    }
}

/// Excluseive access to the underlying storage until released or dropped.
pub struct FairShareExclusiveAccess<'a, T> {
    fs: &'a FairShare<T>,
}

impl<'a, T> Deref for FairShareExclusiveAccess<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // Safety: We can generate mulitple immutable references to the underlying type.
        // And if any mutable reference is generated we are protected via `&self`.
        unsafe { &*(self.fs.storage.get()) }
    }
}

impl<'a, T> DerefMut for FairShareExclusiveAccess<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // Safety: We can generate a single mutable references to the underlying type.
        // And if any immutable reference is generated we are protected via `&mut self`.
        unsafe { &mut *(self.fs.storage.get()) }
    }
}

impl<T> FairShareExclusiveAccess<'_, T> {
    /// Release exclusive access, equates to a drop.
    pub fn release(self) {
        // Run drop
    }
}

impl<T> Drop for FairShareExclusiveAccess<'_, T> {
    fn drop(&mut self) {
        let waker = critical_section::with(|mut token| {
            let fs = self.fs.get_management(&mut token);
            defmt::debug!("Returning exclusive access");
            fs.wake_next_in_queue()
        });

        // Run the waker outside of the critical section to minimize its size
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}
