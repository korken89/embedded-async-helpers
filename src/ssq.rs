//! A single slot queue.
//!
//! Especially useful for sharing wakers between async HAL drivers and their futures.

use atomic_polyfill::{AtomicBool, Ordering};
use core::{cell::UnsafeCell, mem::MaybeUninit, ptr, task::Waker};

/// Short-hand for a single waker queue.
pub type WakerQueue = SingleSlotQueue<Waker>;

/// Short-hand for a single waker queue's consumer.
pub type WakerConsumer = Consumer<'static, Waker>;

/// Short-hand for a single waker queue's producer.
pub type WakerProducer = Consumer<'static, Waker>;

/// Single slot queue.
pub struct SingleSlotQueue<T> {
    full: AtomicBool,
    val: UnsafeCell<MaybeUninit<T>>,
}

impl<T> SingleSlotQueue<T> {
    /// Create a new SSQ.
    pub const fn new() -> Self {
        SingleSlotQueue {
            full: AtomicBool::new(false),
            val: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }

    /// Split the queue into producer and comsumer.
    pub fn split<'a>(&'a mut self) -> (Consumer<'a, T>, Producer<'a, T>) {
        (Consumer { ssq: self }, Producer { ssq: self })
    }
}

impl<T> Drop for SingleSlotQueue<T> {
    fn drop(&mut self) {
        if self.full.load(Ordering::Relaxed) {
            unsafe {
                ptr::drop_in_place(self.val.get() as *mut T);
            }
        }
    }
}

/// Read handle to a single slot queue.
pub struct Consumer<'a, T> {
    ssq: &'a SingleSlotQueue<T>,
}

impl<'a, T> Consumer<'a, T> {
    /// Try reading a value from the queue.
    #[inline]
    pub fn dequeue(&mut self) -> Option<T> {
        if self.ssq.full.load(Ordering::Acquire) {
            let r = Some(unsafe { ptr::read(self.ssq.val.get().cast()) });
            self.ssq.full.store(false, Ordering::Release);
            r
        } else {
            None
        }
    }

    /// Check if there is a value in the queue.
    #[inline]
    pub fn is_empty(&self) -> bool {
        !self.ssq.full.load(Ordering::Relaxed)
    }
}

/// Safety: We gurarantee the safety using an `AtomicBool` to gate the read of the `UnsafeCell`.
unsafe impl<'a, T> Send for Consumer<'a, T> {}

/// Write handle to a single slot queue.
pub struct Producer<'a, T> {
    ssq: &'a SingleSlotQueue<T>,
}

impl<'a, T> Producer<'a, T> {
    /// Write a value into the queue. If there is a value already in the queue this will
    /// return the value given to this method.
    #[inline]
    pub fn enqueue(&mut self, val: T) -> Option<T> {
        if !self.ssq.full.load(Ordering::Acquire) {
            unsafe { ptr::write(self.ssq.val.get().cast(), val) };
            self.ssq.full.store(true, Ordering::Release);
            None
        } else {
            Some(val)
        }
    }

    /// Check if there is a value in the queue.
    #[inline]
    pub fn is_empty(&self) -> bool {
        !self.ssq.full.load(Ordering::Relaxed)
    }
}

/// Safety: We gurarantee the safety using an `AtomicBool` to gate the write of the
/// `UnsafeCell`.
unsafe impl<'a, T> Send for Producer<'a, T> {}
