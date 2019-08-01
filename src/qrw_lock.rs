use std::{
  cell::UnsafeCell,
  future::Future,
  ops::{Deref, DerefMut},
  pin::Pin,
  sync::{Arc, atomic::{AtomicUsize, fence, Ordering}},
  task::{Context, Poll},
  thread,
};

use crossbeam::queue::SegQueue;
use futures::channel::oneshot::{self, Canceled, Receiver, Sender};


const WRITE_LOCKED: usize = 1 << 24;
const CONTENDED: usize = 1 << 25;

/// Very forgettable guards.
trait Guard<T>
where
    Self: ::std::marker::Sized,
{
    fn lock(&self) -> &QrwLock<T>;

    unsafe fn forget(self) {
        ::std::mem::forget(self);
    }
}

/// Allows read or write access to the data contained within a lock.
#[derive(Debug)]
pub struct WriteGuard<T> {
    lock: QrwLock<T>,
}

impl<T> Deref for WriteGuard<T> {
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { &*self.lock.inner.cell.get() }
    }
}

impl<T> DerefMut for WriteGuard<T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.inner.cell.get() }
    }
}

impl<T> Drop for WriteGuard<T> {
    fn drop(&mut self) {
        unsafe { self.lock.release_write_lock() }
    }
}

impl<T> Guard<T> for WriteGuard<T> {
    fn lock(&self) -> &QrwLock<T> {
        &self.lock
    }
}

/// A future which resolves to a `WriteGuard`.
#[must_use = "futures do nothing unless polled"]
#[derive(Debug)]
pub struct FutureWriteGuard<T> {
    lock: Option<QrwLock<T>>,
    rx: Receiver<()>,
}

impl<T> FutureWriteGuard<T> {
    /// Returns a new `FutureWriteGuard`.
    fn new(lock: QrwLock<T>, rx: Receiver<()>) -> FutureWriteGuard<T> {
        FutureWriteGuard {
            lock: Some(lock),
            rx,
        }
    }
}

impl<T> Future for FutureWriteGuard<T> {
    type Output = Result<WriteGuard<T>, Canceled>;

    #[inline]
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        if self.lock.is_some() {
            unsafe { self.lock.as_ref().unwrap().process_queues() }
            Pin::new(&mut self.rx).poll(cx).map(|res| {
                res.map(|_| {
                    WriteGuard {
                        lock: self.lock.take().unwrap(),
                    }
                })
            })
        } else {
            panic!("FutureWriteGuard::poll: Task already completed.");
        }
    }
}

impl<T> Drop for FutureWriteGuard<T> {
    /// Gracefully unlock if this guard has a lock acquired but has not yet
    /// been polled to completion.
    fn drop(&mut self) {
        if let Some(lock) = self.lock.take() {
            self.rx.close();
            match self.rx.try_recv() {
                Ok(status) => {
                    if status.is_some() {
                        unsafe { lock.release_write_lock() }
                    }
                }
                Err(_) => (),
            }
        }
    }
}

/// Specifies whether a `QrwRequest` is a read or write request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestKind {
    Read,
    Write,
}

/// A request to lock the lock for either read or write access.
#[derive(Debug)]
pub struct QrwRequest {
    tx: Sender<()>,
    kind: RequestKind,
}

impl QrwRequest {
    /// Returns a new `QrwRequest`.
    pub fn new(tx: Sender<()>, kind: RequestKind) -> QrwRequest {
        QrwRequest { tx: tx, kind: kind }
    }
}

/// The guts of a `QrwLock`.
#[derive(Debug)]
struct Inner<T> {
    // TODO: Convert to `AtomicBool` if no additional states are needed:
    state: AtomicUsize,
    cell: UnsafeCell<T>,
    queue: SegQueue<QrwRequest>,
    tip: UnsafeCell<Option<QrwRequest>>,
    upgrade_queue: SegQueue<Sender<()>>,
}

impl<T> From<T> for Inner<T> {
    #[inline]
    fn from(val: T) -> Inner<T> {
        Inner {
            state: AtomicUsize::new(0),
            cell: UnsafeCell::new(val),
            queue: SegQueue::new(),
            tip: UnsafeCell::new(None),
            upgrade_queue: SegQueue::new(),
        }
    }
}

unsafe impl<T: Send> Send for Inner<T> {}
unsafe impl<T: Send> Sync for Inner<T> {}

/// A queue-backed read/write data lock.
///
/// As with any queue-backed system, deadlocks must be carefully avoided when
/// interoperating with other queues.
///
#[derive(Debug)]
pub struct QrwLock<T> {
    inner: Arc<Inner<T>>,
}

impl<T> QrwLock<T> {
    /// Creates and returns a new `QrwLock`.
    #[inline]
    pub fn new(val: T) -> QrwLock<T> {
        QrwLock {
            inner: Arc::new(Inner::from(val)),
        }
    }

    /// Returns a new `FutureWriteGuard` which can be used as a future and will
    /// resolve into a `WriteGuard`.
    #[inline]
    pub fn write(self) -> FutureWriteGuard<T> {
        let (tx, rx) = oneshot::channel();
        unsafe {
            self.enqueue_lock_request(QrwRequest::new(tx, RequestKind::Write));
        }
        FutureWriteGuard::new(self, rx)
    }

    /// Pushes a lock request onto the queue.
    ///
    //
    // TODO: Evaluate unsafe-ness (appears unlikely this can be misused except
    // to deadlock the queue which is fine).
    //
    #[inline]
    pub unsafe fn enqueue_lock_request(&self, req: QrwRequest) {
        self.inner.queue.push(req);
    }

    /// Pops the next read or write lock request and returns it or `None` if the queue is empty.
    #[inline]
    fn pop_request(&self) -> Option<QrwRequest> {
        unsafe {
            // Pop twice if the tip was `None` but the queue was not empty.
            ::std::mem::replace(&mut *self.inner.tip.get(), self.inner.queue.pop().ok()).or_else(
                || {
                    if (*self.inner.tip.get()).is_some() {
                        self.pop_request()
                    } else {
                        None
                    }
                },
            )
        }
    }

    /// Returns the `RequestKind` for the next pending read or write lock request.
    #[inline]
    fn peek_request_kind(&self) -> Option<RequestKind> {
        unsafe {
            if (*self.inner.tip.get()).is_none() {
                ::std::mem::replace(&mut *self.inner.tip.get(), self.inner.queue.pop().ok());
            }
            (*self.inner.tip.get()).as_ref().map(|req| req.kind)
        }
    }

    /// Fulfill a request if possible.
    #[inline]
    fn fulfill_request(&self, mut state: usize) -> usize {
        loop {
            if let Some(req) = self.pop_request() {
                // If there is a send error, a requester has dropped its
                // receiver so just go to the next, otherwise process.
                if req.tx.send(()).is_ok() {
                    match req.kind {
                        RequestKind::Read => {
                            state += 1;
                        }
                        RequestKind::Write => {
                            state = WRITE_LOCKED;
                            break;
                        }
                    }
                }

                if let Some(RequestKind::Read) = self.peek_request_kind() {
                    continue;
                } else {
                    break;
                }
            } else {
                break;
            }
        }

        state
    }

    /// Acquires exclusive access to the lock state and returns it.
    #[inline(always)]
    fn contend(&self) -> usize {
        let mut spins: u32 = 0;

        loop {
            let state = self.inner.state.fetch_or(CONTENDED, Ordering::SeqCst);
            if state & CONTENDED != 0 {
                if spins >= 16 {
                    thread::yield_now();
                } else {
                    for _ in 0..(2 << spins) {
                        fence(Ordering::SeqCst);
                    }
                }
                spins += 1;
            } else {
                return state;
            }
        }
    }

    /// Fulfills an upgrade request.
    fn process_upgrade_queue(&self) -> bool {
        loop {
            match self.inner.upgrade_queue.pop().ok() {
                Some(tx) => match tx.send(()) {
                    Ok(_) => {
                        return true;
                    }
                    Err(()) => {
                        continue;
                    }
                },
                None => break,
            }
        }
        false
    }

    /// Pops the next lock request in the queue if possible.
    ///
    //
    // TODO: Clarify the following (or remove):
    //
    // If this (the caller's?) lock is released, read or write-locks this lock
    // and unparks the next requester task in the queue.
    //
    // If this (the caller's?) lock is write-locked, this function does
    // nothing.
    //
    // If this (the caller's?) lock is read-locked and the next request or consecutive
    // requests in the queue are read requests, those requests will be
    // fulfilled, unparking their respective tasks and incrementing the
    // read-lock count appropriately.
    //
    //
    // TODO:
    // * This is currently public due to 'derivers' (aka. sub-types). Evaluate.
    // * Consider removing unsafe qualifier (should be fine, this fn assumes
    //   no particular state).
    // * Return proper error type.
    //
    // pub unsafe fn process_queues(&self) -> Result<(), ()> {
    #[inline]
    pub unsafe fn process_queues(&self) {
        match self.contend() {
            // Unlocked:
            0 => {
                let new_state = if self.process_upgrade_queue() {
                    WRITE_LOCKED
                } else {
                    self.fulfill_request(0)
                };

                self.inner.state.store(new_state, Ordering::SeqCst);
            }
            // Write locked, unset CONTENDED flag:
            WRITE_LOCKED => {
                self.inner.state.store(WRITE_LOCKED, Ordering::SeqCst);
            }
            // Either read locked or already being processed:
            state => {
                if self.peek_request_kind() == Some(RequestKind::Read) {
                    // We are read locked and the next request is a read.
                    let new_state = self.fulfill_request(state);
                    self.inner.state.store(new_state, Ordering::SeqCst);
                } else {
                    // Either the next request is empty or a write and
                    // we are already read locked. Leave the request
                    // there and restore our original state, removing
                    // the CONTENDED flag.
                    self.inner.state.store(state, Ordering::SeqCst);
                }
            }
        }
    }

    /// Unlocks this (the caller's) lock and unparks the next requester task
    /// in the queue if possible.
    //
    // TODO: Consider using `Ordering::Release`.
    #[inline]
    pub unsafe fn release_write_lock(&self) {
        match self.contend() {
            0 => unreachable!(),
            WRITE_LOCKED => {
                self.inner.state.store(0, Ordering::SeqCst);
                self.process_queues();
            }
            _state => unreachable!(),
        }
    }
}

impl<T> From<T> for QrwLock<T> {
    #[inline]
    fn from(val: T) -> QrwLock<T> {
        QrwLock::new(val)
    }
}

// Avoids needing `T: Clone`.
impl<T> Clone for QrwLock<T> {
    #[inline]
    fn clone(&self) -> QrwLock<T> {
        QrwLock {
            inner: self.inner.clone(),
        }
    }
}
