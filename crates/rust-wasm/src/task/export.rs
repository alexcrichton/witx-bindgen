use std::cell::Cell;
use std::collections::HashMap;
use std::convert::TryFrom;
use std::future::Future;
use std::marker::{PhantomData, Unpin};
use std::mem;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};

use super::import;

const ACTION_WAIT: u32 = 0;
const ACTION_FINISH: u32 = 1;

#[link(wasm_import_module = "canonical_abi")]
extern "C" {
    fn task_write(ptr: usize, len: usize);
    fn task_unwrite() -> usize;
    fn task_return(ptr: usize);
    fn task_unreturn();
    fn task_self() -> u32;
    fn task_notify(task: u32);
}

pub struct ExportTask {
    future: Option<Pin<Box<dyn Future<Output = ()>>>>,
    write_state: Cell<WriteState>,
    return_state: Cell<ReturnState>,
    waker: Arc<ExportWaker>,
}

struct ExportWaker {
    id_self: u32,
    notified: AtomicBool,
}

#[derive(PartialEq, Debug, Clone, Copy)]
enum WriteState {
    Wrote { amt: usize },
    Writing,
    None,
}

#[derive(PartialEq, Debug, Clone, Copy)]
enum ReturnState {
    NotStarted,
    Started,
    Acknowledged,
}

enum Action {
    Wait(Box<ExportTask>),
    Finish,
}

impl Action {
    fn encode(self, prev_ptr: usize) -> u32 {
        match self {
            Action::Wait(task) => {
                let task = Box::into_raw(task);
                debug_assert_eq!(task as usize, prev_ptr);
                ACTION_WAIT
            }
            Action::Finish => ACTION_FINISH,
        }
    }
}

impl ExportTask {
    /// Returns the identifier used for the current task in the canonical ABI.
    #[doc(hidden)]
    pub fn current_id() -> u32 {
        unsafe { task_self() }
    }

    /// ABI entry point for the `callee` callback.
    ///
    /// The actual callee is intended to be represented with the `future`
    /// provided.
    #[doc(hidden)]
    pub fn run(id_self: u32, future: impl Future<Output = ()> + 'static) -> (usize, u32) {
        let task = Box::new(ExportTask {
            future: Some(Box::pin(future)),
            write_state: Cell::new(WriteState::None),
            return_state: Cell::new(ReturnState::NotStarted),
            waker: Arc::new(ExportWaker {
                id_self,
                notified: AtomicBool::new(false),
            }),
        });

        // Initiate the first poll here-and-now, serializing the result
        // depending on the outcome.
        match task.poll() {
            Action::Wait(task) => (Box::into_raw(task) as usize, ACTION_WAIT),
            Action::Finish => (0, ACTION_FINISH),
        }
    }

    /// ABI entry point for the `on-event` callback.
    #[doc(hidden)]
    pub unsafe fn on_event_abi(ptr: usize, cause: u32, idx: u32) -> u32 {
        let task = Self::raw_to_box(ptr);

        // Update the state of child tasks, if necessary.
        import::on_event_abi(cause, idx);

        task.poll().encode(ptr)
    }

    /// ABI entry point for the `post-write` callback.
    #[doc(hidden)]
    pub unsafe fn post_write_abi(ptr: usize, amt: usize) -> u32 {
        let task = Self::raw_to_box(ptr);

        // Update our task's write state, which should always be coming from the
        // `Writing` state since otherwise this callback shouldn't be called.
        let prev = task.write_state.replace(WriteState::Wrote { amt });
        debug_assert_eq!(prev, WriteState::Writing);

        task.poll().encode(ptr)
    }

    /// ABI entry point for the `post-return` callback.
    #[doc(hidden)]
    pub unsafe fn post_return_abi(ptr: usize) -> u32 {
        let task = Self::raw_to_box(ptr);

        // Indicate that our request to return has been acknowledged, and then
        // afterwards a normal poll happens as usual. When the future returned
        // by `TaskAbi::await_return` is re-polled eventually it will pick up
        // this new return state value and resolve itself.
        let prev = task.return_state.replace(ReturnState::Acknowledged);
        debug_assert_eq!(prev, ReturnState::Started);

        task.poll().encode(ptr)
    }

    fn poll(mut self: Box<Self>) -> Action {
        if let Some(mut future) = self.future.take() {
            // reset the notification flag so future calls to `wake` will send
            // us a notification.
            self.waker.notified.store(false, SeqCst);

            // "do a little dance" and create the `Context` through which we'll
            // poll the internal future.
            let waker = Waker::from(self.waker.clone());
            let mut context = Context::from_waker(&waker);

            // Poll the internal future, releasing it when we're done and
            // preserving it if it's not done. Note that this poll is done
            // within the context of `tls::set` to give the future access to the
            // various methods like `ExportTask::await_return` and such.
            self.future = tls::set(&self, || match future.as_mut().poll(&mut context) {
                Poll::Ready(()) => None,
                Poll::Pending => Some(future),
            });
        }

        if self.future.is_some() {
            return Action::Wait(self);
        }

        // TODO: confirm with aturon this is the right time to return finished
        Action::Finish
    }

    /// ABI entry point for the `cancel` callback.
    #[doc(hidden)]
    pub unsafe fn cancel_abi(ptr: usize) -> u32 {
        let mut me = Self::raw_to_box(ptr);

        // Currently cancellation is modelled by dropping the Rust-based future.
        // Note that in the future this might initiate an async drop or similar.
        // Also note that this explicitly drops the future within a `tls::set`
        // context to give access to the internals, if necessary, as part of
        // destructors (used in this module).
        let future = me.future.take();
        if future.is_some() {
            tls::set(&me, || drop(future));
        }

        me.poll().encode(ptr)
    }

    /// Takes ownership of the `ptr` and returns it as an owned task.
    unsafe fn raw_to_box(ptr: usize) -> Box<ExportTask> {
        Box::from_raw(ptr as *mut ExportTask)
    }

    /// Internal function to represent signalling a return value.
    ///
    /// # Panics
    ///
    /// Panics if this future runs on a task other than `id_self`, which
    /// shouldn't be possible due to the way glue is constructed and how it
    /// calls this function.
    #[doc(hidden)]
    pub async fn await_return(id_self: u32, retptr: usize) {
        // Setup state indicating we're "returning"
        let ret = TaskReturn::new(id_self, retptr);

        // Wait for the return to get acknowledged through the `post-return`
        // callback. Note that this `.await` point is cancel-able, hence the
        // `Drop for TaskReturn` which un-sets the returning state if it's not
        // acknowledged.
        //
        // In practice, however, this `async` function is never cancelled due to
        // how it's called in the generated glue code.
        Self::await_state(id_self, |state| match state.return_state.get() {
            ReturnState::Acknowledged => Poll::Ready(()),
            other => {
                debug_assert_eq!(other, ReturnState::Started);
                Poll::Pending
            }
        })
        .await;

        // The return has been acknowledged, so there's nothing else to clean
        // up.
        return ret.forget();

        struct TaskReturn {
            id_self: u32,
        }

        impl TaskReturn {
            fn new(id_self: u32, ptr: usize) -> TaskReturn {
                tls::with(id_self, |task| {
                    let prev = task.return_state.replace(ReturnState::Started);
                    debug_assert_eq!(prev, ReturnState::NotStarted);
                });
                unsafe {
                    task_return(ptr);
                }

                TaskReturn { id_self }
            }

            fn forget(self) {
                if cfg!(debug_assertions) {
                    tls::with(self.id_self, |task| {
                        let state = task.return_state.get();
                        assert_eq!(state, ReturnState::Acknowledged);
                    });
                }
                mem::forget(self)
            }
        }

        // Note that technically this shouldn't ever run due to the way glue
        // code is constructed, but this is provided for completeness.
        impl Drop for TaskReturn {
            fn drop(&mut self) {
                tls::with(self.id_self, |task| {
                    match task.return_state.get() {
                        // If the return value was acknowledged and this value
                        // is dropped then that means that the `await_return`
                        // future was dropped, despite the return being
                        // acknowledged. In this case there's no cleanup for us
                        // to perform.
                        ReturnState::Acknowledged => {}

                        // Otherwise this return state was dropped in the
                        // process of returning, so undo the return state
                        // associated with this task.
                        other => {
                            debug_assert_eq!(other, ReturnState::Started);
                            task.return_state.set(ReturnState::NotStarted);
                            unsafe {
                                task_unreturn();
                            }
                        }
                    }
                });
            }
        }
    }

    /// Awaits the current task to satisfy the `f` closure.
    ///
    /// # Panics
    ///
    /// Panics if `id_self` doesn't match the id of the current task.
    async fn await_state<T>(id_self: u32, f: impl FnMut(&ExportTask) -> Poll<T> + Unpin) -> T {
        return AwaitState(id_self, f).await;

        struct AwaitState<F>(u32, F);

        impl<F, T> Future for AwaitState<F>
        where
            F: FnMut(&ExportTask) -> Poll<T> + Unpin,
        {
            type Output = T;

            fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<T> {
                // NB: note how `cx.waker()` isn't used at all here. This is
                // intentional. The `tls::with` function asserts that `self.0`,
                // the expected self-id of this task, matches the actual running
                // task. This means any state transition that this function is
                // interested in is guaranteed to be independently woken up and
                // eventually cause a re-poll of this future (so long as this
                // future is what's still interesting.
                //
                // This means that the future returned by `await_state` cannot
                // move between exported tasks, and that's intentional. This
                // future must remain pinned to the original exported task
                // because it's the only one actually guaranteed to receive
                // notifications for state changes.
                //
                // It might be theoretically possible to set up a channel and
                // allow movement of this future but until such a need arises it
                // seems best to keep things simple here if we can.
                tls::with(self.0, &mut self.as_mut().1)
            }
        }
    }

    /// Returns the identifier in the canonical abi for the task that this
    /// represents.
    fn id_self(&self) -> u32 {
        self.waker.id_self
    }
}

impl Wake for ExportWaker {
    fn wake(self: Arc<Self>) {
        // If this task is already notified, no need to re-notify. Otherwise
        // fall through and request that the canonical abi sends the target task
        // a wakeup notification.
        if self.notified.swap(true, SeqCst) {
            return;
        }
        unsafe {
            task_notify(self.id_self);
        }
    }
}

pub struct WriteStream<T> {
    _marker: PhantomData<fn() -> T>,
    id_self: u32,
}

impl<T> WriteStream<T> {
    // internal constructor only used for glue
    #[doc(hidden)]
    pub fn new(id_self: u32) -> WriteStream<T>
    where
        T: 'static,
    {
        // TODO: this is a temporary restriction for development, but this
        // restriction only works when `T` is a copy type that has the same
        // representation in Rust as the canonical ABI, so pick `u8` for now.
        assert_eq!(std::any::TypeId::of::<T>(), std::any::TypeId::of::<u8>());

        WriteStream {
            _marker: PhantomData,
            id_self,
        }
    }

    /// Performs an asynchronous write of `data` into this stream, returning how
    /// many items were written into the stream.
    ///
    /// Note that partial writes may successfully happen and are not reported
    /// immediately. If this future is cancelled this method provides no way of
    /// learning how many items, if any, were written. If partial writes matter
    /// it's recommended to use the `with_buffer` method to determine, after the
    /// write future is cancelled, how many items were written.
    ///
    /// # Panics
    ///
    /// Panics if this is called from a wasm task that is not the same as the
    /// original task given access to the original `WriteStream`.
    pub async fn write(&mut self, data: &[T]) -> usize {
        self.with_buffer(data).write().await
    }

    pub fn with_buffer<'a>(&mut self, buf: &'a [T]) -> WriteStreamWithBuffer<'a, '_, T> {
        WriteStreamWithBuffer::new(self, buf)
    }

    // Hypothetically something that could be added to the canonical abi in the
    // future.
    // async fn ready(&mut self);
}

pub struct WriteStreamWithBuffer<'buf, 'stream, T> {
    stream: &'stream mut WriteStream<T>,
    buf: &'buf [T],
    written: usize,
}

impl<'buf, 'stream, T> WriteStreamWithBuffer<'buf, 'stream, T> {
    fn new(stream: &'stream mut WriteStream<T>, buf: &'buf [T]) -> Self {
        WriteStreamWithBuffer {
            stream,
            buf,
            written: 0,
        }
    }

    /// Executes a write with the buffer and stream attached to this value.
    ///
    /// This will return how many items were written from the buffer. If this
    /// future is dropped (cancelled) then the `count` method can be used to
    /// determine how many items, if any, were written.
    ///
    /// # Panics
    ///
    /// Panics if this is called from a wasm task that is not the same as the
    /// original task given access to the original `WriteStream`.
    pub async fn write(&mut self) -> usize {
        // This is a guard against overwriting prevoiusly written elements. If
        // `self.written` is not zero then it means that `write` was called,
        // some data was written, the future was dropped, and the `count()`
        // method wdasn't used to learn how much was written. This means that a
        // previous write partially finished but hasn't been discovered, so
        // re-writing the same data is unlikely to be what's desired here.
        assert_eq!(self.written, 0);

        // Construct state associated with this active write. Note that this is
        // where the `canon.task.write` intrinsic is called. This is active
        // state that must be maintained, though, so it's packaged up in a value
        // with a destructor to clean up later.
        let mut in_progress = ActiveWrite::new(self);

        // Wait for our task to receive the post-write callback notification.
        // This will happen when our writing state is updated to
        // `WriteState::Wrote`, which is checked in `consume_write`.
        //
        // Also note that this is a cancellation point. If this future is
        // dropped at this point then it could mean:
        //
        // * The post-write callback was never called. This means that we need
        //   to `canon.task.unwrite` which may actually report some data
        //   written. This is available through `Self::count` after the future
        //   returned here is dropped.
        //
        // * The post-write callback was called, but during the `poll`
        //   afterwards this future returned here is dropped. That means we
        //   don't call `canon.task.unwrite` but still update `self.written`
        //   with the data passed in the post-write callback.
        //
        // Cancellation is all handled through the `Drop` implementation for
        // `ActiveWrite`.
        ExportTask::await_state(in_progress.buf.stream.id_self, |state| {
            in_progress.consume_write(state)
        })
        .await;

        // If we made it this far then nothing was cancelled. Making it this far
        // also signfies that `consume_write` was called and `Wrote` was
        // actually consumed. At this point our task's state is reset back to
        // `WriteState::None` and there's no need to call `canon.task.unwrite`,
        // which means we forget this in-progress write as it's no longer
        // needed.
        in_progress.forget();

        // The return value here is "consumed" by reading out of the
        // `self.written` field, which is updated by the
        // `in_progress.consume_write` method above.
        return self.count();

        struct ActiveWrite<'a, 'buf, 'stream, T> {
            buf: &'a mut WriteStreamWithBuffer<'buf, 'stream, T>,
        }

        impl<'a, 'buf, 'stream, T> ActiveWrite<'a, 'buf, 'stream, T> {
            fn new(buf: &'a mut WriteStreamWithBuffer<'buf, 'stream, T>) -> Self {
                // Indicate that our task is now in a writing state.
                // Sanity-check that previously no write was happening. Note
                // that this sanity check should be upheld because on dropping
                // `ActiveWrite` the state should always be `WriteState::None`
                // and each task gets at most one `WriteStream` meaning that if
                // we made it this far with a mutable borrow it's the only
                // active one.
                tls::with(buf.stream.id_self, |state| {
                    let prev = state.write_state.replace(WriteState::Writing);
                    debug_assert_eq!(prev, WriteState::None);
                });

                // Use the `canon.task.write` intrinsic to inform the canonical
                // ABI that we're entering the writing state.
                unsafe {
                    task_write(buf.buf.as_ptr() as usize, buf.buf.len() as usize);
                }

                ActiveWrite { buf }
            }

            fn consume_write(&mut self, state: &ExportTask) -> Poll<()> {
                match state.write_state.get() {
                    WriteState::Wrote { amt } => {
                        self.buf.written += amt;
                        state.write_state.set(WriteState::None);
                        Poll::Ready(())
                    }
                    other => {
                        debug_assert_eq!(other, WriteState::Writing);
                        Poll::Pending
                    }
                }
            }

            fn forget(self) {
                // It's an invariant that the task is back in `WriteState::None`
                // when this type is dropped, so double-check that here if we're
                // going to skip the destructor.
                if cfg!(debug_assertions) {
                    tls::with(self.buf.stream.id_self, |state| {
                        assert_eq!(state.write_state.get(), WriteState::None);
                    });
                }
                mem::forget(self);
            }
        }

        impl<T> Drop for ActiveWrite<'_, '_, '_, T> {
            fn drop(&mut self) {
                // Handle the two cancellation cases described above in the
                // comment on `await_state`, notably:
                //
                // * If post-write was never called, our state will still be
                //   `WriteState::Writing`. The `canon.task.unwrite` intrinsic
                //   is used here to stop the write and we update our written
                //   amount with the partial write amount, if any, reported.
                //
                //* If post-write was called then we reset back to
                //  `WriteState::None` and update the amount written, but
                //  `canon.task.unwrite` is not called.
                tls::with(self.buf.stream.id_self, |state| {
                    let prev = state.write_state.replace(WriteState::None);
                    self.buf.written += match prev {
                        WriteState::Wrote { amt } => amt,
                        other => {
                            debug_assert_eq!(other, WriteState::Writing);
                            unsafe { task_unwrite() }
                        }
                    };
                })
            }
        }
    }

    /// Used to determine if any items in the buffer were written when the
    /// future of `write` is dropped.
    ///
    /// When the `write` future is dropped there's a race as to whether the
    /// write actually completed or not or whether the drop happened first. This
    /// method can be used in these situations to resolve the race and determine
    /// how many items were written.
    ///
    /// Note that this method "consumes" the internal write count. If the
    /// `write` method successfully finished and produced a value then this will
    /// return zero. Call this a second time will also return zero. This will
    /// only return nonzero in the case that a `write` future was dropped and
    /// the first time `count` is called afterwards it will return any partially
    /// written items done in `count`.
    pub fn count(&mut self) -> usize {
        mem::take(&mut self.written)
    }
}

mod tls {
    use super::ExportTask;
    use std::cell::Cell;

    thread_local!(static CURRENT: Cell<usize> = Cell::new(0));

    pub fn set<R>(ptr: &ExportTask, f: impl FnOnce() -> R) -> R {
        CURRENT.with(|p| {
            let prev = p.get();
            p.set(ptr as *const ExportTask as usize);
            let ret = f();
            p.set(prev);
            ret
        })
    }

    /// Gets a reference to the state of the current exported task being polled.
    ///
    /// Panics if `expected_id_self` does not match the id of the task found, or
    /// if there is no task to be found.
    pub fn with<R>(expected_id_self: u32, f: impl FnOnce(&ExportTask) -> R) -> R {
        CURRENT.with(|p| {
            let cur = p.get();
            if cur == 0 {
                panic!("no task context found, is this future polled outside of an async wasm task context?");
            }
            let task = unsafe { &*(cur as *const ExportTask) };
            assert_eq!(task.id_self(), expected_id_self, "future has moved between tasks and is in an invalid state");
            debug_assert_eq!(expected_id_self, ExportTask::current_id());
            f(task)
        })
    }
}
