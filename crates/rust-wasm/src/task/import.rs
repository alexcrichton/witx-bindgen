use super::ExportTask;
use once_cell::sync::OnceCell;
use std::collections::HashMap;
use std::convert::TryFrom;
use std::future::Future;
use std::marker::{PhantomData, PhantomPinned, Unpin};
use std::mem;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};

const CAUSE_CHILD_WRITE: u32 = 0;
const CAUSE_CHILD_RETURN: u32 = 1;
const CAUSE_CHILD_DONE: u32 = 2;

#[link(wasm_import_module = "canonical_abi")]
extern "C" {
    fn task_drop(task: u32);
    fn task_adopt(task: u32);
    fn task_listen_future(task: u32, ret: usize);
    fn task_listen_stream(task: u32, buf: usize, len: usize, retptr: usize);
    fn task_unlisten(task: u32);
}

pub fn on_event_abi(cause: u32, idx: u32) {
    // Process `cause` and `x` by updating internal references to this
    // task's state.
    let state = GlobalImportState::get();
    let mut children = state.children.lock().unwrap();
    if cause & 0xf == CAUSE_CHILD_WRITE {
        let amt = cause >> 4;
        children
            .get_mut(&idx)
            .unwrap()
            .update_to(ChildState::Wrote(usize::try_from(amt).unwrap()));
    } else {
        match cause {
            // When a child returns that means we're actively listening for
            // the return value and the return value is now filled in. The
            // internal state here is updated to indicate the child has
            // returned, and this also asserts that the previous state was a
            // waiting state.
            CAUSE_CHILD_RETURN => children
                .get_mut(&idx)
                .unwrap()
                .update_to(ChildState::Returned),

            // When a child is done it's largely the same as returning but
            // this means that the child has finished all its post-return
            // work as well. Here the state is updated and asserts that
            // the previous state is `Returned`.
            CAUSE_CHILD_DONE => children.get_mut(&idx).unwrap().update_to(ChildState::Done),

            cause => {
                if cfg!(debug_assertions) {
                    panic!("unkonwn cause in on-event: {}", cause);
                }
            }
        }
    }
}

#[derive(Default)]
struct GlobalImportState {
    children: Mutex<HashMap<u32, ChildState>>,
}

impl GlobalImportState {
    fn get() -> &'static GlobalImportState {
        static GLOBAL: OnceCell<GlobalImportState> = OnceCell::new();
        GLOBAL.get_or_init(|| GlobalImportState::default())
    }
}

#[derive(Debug, Clone, PartialEq)]
enum ChildState {
    Waiting,
    Wrote(usize),
    Returned,
    Done,
}

impl ChildState {
    fn update_to(&mut self, new_state: ChildState) {
        let prev = mem::replace(self, new_state.clone());
        if cfg!(debug_assertions) {
            let expected_prev = match new_state {
                ChildState::Done => ChildState::Returned,
                _ => ChildState::Waiting,
            };
            assert_eq!(
                prev, expected_prev,
                "invalid state to transition to {:?}",
                new_state
            );
        }
    }
}

/// Helper structure used to manage a child task.
struct WasmTask {
    id: u32,
    parent: u32,
}

impl WasmTask {
    /// Takes ownership of the `task` identifier specified and returns an owned
    /// representation of the task which will drop the task on drop of
    /// `WasmTask`.
    fn new(id: u32) -> WasmTask {
        // Insert state about our new child `task` which will get updated when
        // events happen.
        let state = GlobalImportState::get();
        let prev = state
            .children
            .lock()
            .unwrap()
            .insert(id, ChildState::Waiting);
        debug_assert!(prev.is_none());

        WasmTask {
            id,
            parent: ExportTask::current_id(),
        }
    }

    /// Asynchronously awaits the for the `test` of this task's state to return
    /// `true`.
    ///
    /// This will return a future that doesn't resolve until `test` returns
    /// `true`, and then `test`'s `ChildState` is returned.
    async fn await_child_state<T>(
        &mut self,
        test: impl FnMut(&mut ChildState) -> Poll<T> + Unpin,
    ) -> T {
        return AwaitState { task: self, test }.await;

        struct AwaitState<'a, F> {
            task: &'a mut WasmTask,
            test: F,
        }

        impl<F, T> Future for AwaitState<'_, F>
        where
            F: FnMut(&mut ChildState) -> Poll<T> + Unpin,
        {
            type Output = T;

            fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<T> {
                // Note that `cx.waker()` is not used here. To ensure that this
                // Rust-task is re-polled we ensure, though, that state changes
                // for this import are routed to the current wasm-task via the
                // `maybe_reparent`. That ensures that this calling wasm task
                // will receive notification for state changes, completing the
                // contract for `Future` without using `cx.waker()` (or so we
                // hope).
                let me = self.get_mut();
                me.task.maybe_reparent();
                me.task.with_state(&mut me.test)
            }
        }
    }

    fn id(&self) -> u32 {
        self.id
    }

    fn with_state<R>(&self, with: impl FnOnce(&mut ChildState) -> R) -> R {
        let state = GlobalImportState::get();
        let mut children = state.children.lock().unwrap();
        let state = children
            .get_mut(&self.id())
            .expect("cannot use a wasm task on a sibling task");
        with(&mut *state)
    }

    fn maybe_reparent(&mut self) {
        let current_id = ExportTask::current_id();
        if current_id != self.parent {
            unsafe {
                task_adopt(self.id);
            }
            self.parent = current_id;
        }
    }

    fn cancel(self) -> Option<WasmTask> {
        panic!()
    }
}

impl Drop for WasmTask {
    fn drop(&mut self) {
        // First remove the internal state associated with this child in the
        // current task.
        let state = GlobalImportState::get();
        let state = state.children.lock().unwrap().remove(&self.id());
        debug_assert!(state.is_some());

        // Next the task can be fully dropped from the canonical ABI. Note that
        // this will cancel the task if it's still in flight.
        unsafe {
            task_drop(self.id());
        }
    }
}

pub struct Stream<T, U, const N: usize> {
    _wat: fn() -> T,
    import: WasmImport<U, N>,
}

impl<T, U, const N: usize> Stream<T, U, N> {
    // Internal constructor only meant for glue
    #[doc(hidden)]
    pub fn new(import: WasmImport<U, N>) -> Self
    where
        T: 'static,
    {
        // TODO: this is a temporary restriction for development, but this
        // restriction only works when `T` is a copy type that has the same
        // representation in Rust as the canonical ABI, so pick `u8` for now.
        assert_eq!(std::any::TypeId::of::<T>(), std::any::TypeId::of::<u8>());

        Stream {
            _wat: || panic!(),
            import,
        }
    }

    pub fn with_buffer<'a>(&mut self, buf: &'a mut [T]) -> StreamWithBuffer<'a, '_, T, U, N> {
        StreamWithBuffer::new(self, buf)
    }

    pub async fn read(&mut self, buf: &mut [T]) -> usize {
        self.with_buffer(buf).read().await
    }

    // Hypothetically something that could be added to the canonical abi in the
    // future.
    // async fn ready(&mut self);

    /// Returns the result of this stream and a handle to the task representing
    /// post-return work.
    ///
    /// # Panics
    ///
    /// This function will panic if this stream has not received the result yet.
    /// The `read` function must have returned 0 previously to indicate that the
    /// return value was received.
    pub fn result(self) -> (U, WasmImportTask) {
        if let Some(task) = &self.import.task {
            task.with_state(|s| match s {
                ChildState::Returned | ChildState::Done => {}
                ChildState::Wrote(_) | ChildState::Waiting => {
                    panic!("task has not finished yet");
                }
            });
        }
        let Self { mut import, .. } = self;
        let import = unsafe { Pin::new_unchecked(&mut import) };
        import.take_result()
    }
}

pub struct StreamWithBuffer<'buf, 'stream, T, U, const N: usize> {
    stream: &'stream mut Stream<T, U, N>,
    buf: &'buf mut [T],
    read: usize,
}

impl<'buf, 'stream, T, U, const N: usize> StreamWithBuffer<'buf, 'stream, T, U, N> {
    fn new(stream: &'stream mut Stream<T, U, N>, buf: &'buf mut [T]) -> Self {
        StreamWithBuffer {
            stream,
            buf,
            read: 0,
        }
    }

    /// Registers the stream for listening (on the contained buffer),
    /// unregisters it when dropped.
    ///
    /// Blocks until some data is read into the buffer.
    ///
    /// Returns the number of items read into the buffer.
    pub async fn read(&mut self) -> usize {
        // This is a sanity-check which is possible to hit in user code but
        // indicates a user-level bug. This assert will trip if the `read`
        // function is called, the read completes, the `read` future is dropped,
        // and then this function is called again.
        //
        // In that situation the user requested a read, some data was read but
        // raced with Rust-level cancellation, and then the user didn't realize
        // that data was ready by calling `self.count()`. This isn't a great
        // assert to have but can figure out how to better structure this to
        // avoid this assert in the future perhaps.
        assert_eq!(self.read, 0);

        // Streams currently always have a task since we don't generate bindings
        // that take advantage of the fast-path.
        let task = self.stream.import.task.as_mut().unwrap();

        // TODO: is this necessary? Will `canon.task.listen` require that you're
        // the parent? Or maybe `canon.task.listen` will also reparent?
        //
        // In any case need to handle the situation where this currently
        // executing task is taking ownership of the stream for now so the
        // reparenting needs to happen one way or another.
        task.maybe_reparent();

        // Start the listening operation on the stream. Note that this
        // specifically has a destructor so when we're done here in this
        // function we're no longer listening to the stream. This means that the
        // `self.buf` only has to live as long as this function, which it
        // should.
        let unlisten_on_drop = unsafe {
            task_listen_stream(
                task.id(),
                self.buf.as_ptr() as usize,
                self.buf.len() as usize,
                self.stream.import.space.as_ptr() as usize,
            );

            struct Unlisten(u32);

            impl Drop for Unlisten {
                fn drop(&mut self) {
                    unsafe {
                        task_unlisten(self.0);
                    }
                }
            }

            Unlisten(task.id())
        };

        // Create the in-memory state to track the active state of this read.
        // Note that this has a destructor which is significant in accounting
        // for items read as part of this call if the future is dropped at the
        // cancellation point below.
        //
        // Note that this is a cancellation point which we need to handle here
        // as well. To handle cancellation there's two components. One is that
        // the `Unlisten` will be dropped, un-registering interest
        // in this stream. The second is that the `ActiveRead` in the stack
        // frame here will ensure that any written amount signaled via the
        // `on-event` callback makes its way into the `StreamWithBuffer`
        // instance instead of losing it by accident.
        ActiveRead::new(task, &mut self.read).complete().await;

        // Once making it this far there's no need to un-listen because a read
        // completed or the future returned, both of which automatically
        // un-listen for us.
        mem::forget(unlisten_on_drop);

        // Consume the internal count variable to be returned which was updated
        // by `ActiveRead`.
        self.count()
    }

    /// Returns how many items were read into the buffer.
    ///
    /// Will return 0 most of the time. If an in-progress-read was cancelled by
    /// dropping the `read` future then this will return anything read into the
    /// buffer, if any.
    pub fn count(&mut self) -> usize {
        mem::take(&mut self.read)
    }
}

struct ActiveRead<'a, 'b> {
    task: &'a mut WasmTask,
    read: &'b mut usize,
}

impl<'a, 'b> ActiveRead<'a, 'b> {
    fn new(task: &'a mut WasmTask, read: &'b mut usize) -> Self {
        if cfg!(debug_assertions) {
            task.with_state(|state| match *state {
                // Once a child task returns it'll never write again, so
                // this is a valid state to be in when a read is
                // started, and this means that the await on the `read`
                // future will resolve immediately.
                ChildState::Returned | ChildState::Done => {}

                // This means that the child doesn't have any active
                // reported statuses, which is typically what we'd
                // expected.
                ChildState::Waiting => {}

                // This means there's a bug in this structure or `Drop`
                // for this structure, it's an invariant that when
                // `ActiveRead` is dropped the state is never `Wrote`
                // for the task referenced here.
                ChildState::Wrote(_) => {
                    panic!("reading while prevous read un-consumed")
                }
            });
        }
        ActiveRead { task, read }
    }

    /// Awaits completion of the read.
    async fn complete(self) {
        self.task
            .await_child_state(|state| ActiveRead::consume_read(self.read, state))
            .await;

        // Upon getting here there's no need to re-consume the read amount on
        // drop as we already did, so the destructor is cancelled.
        mem::forget(self);
    }

    /// Extracts the `Wrote` amount from `state` and moves it into
    /// `read`, if that state is found.
    ///
    /// Returns whether the child state is either `Wrote`, `Returned`, or `Done`
    /// meaning that either a write has finished or will never again finish.
    fn consume_read(read: &mut usize, state: &mut ChildState) -> Poll<()> {
        match *state {
            ChildState::Waiting => Poll::Pending,
            ChildState::Wrote(n) => {
                *read += n;
                *state = ChildState::Waiting;
                Poll::Ready(())
            }
            ChildState::Returned | ChildState::Done => Poll::Ready(()),
        }
    }
}

// This state is responsible for never leaving the task in the
// `ChildState::Wrote` state. In this destructor the amount written is
// pulled out and stored in `self.read` which can later be used with
// `StreamWithBuffer::count` to learn about a partial read that raced
// with a Rust-level cancellation of a future.
impl Drop for ActiveRead<'_, '_> {
    fn drop(&mut self) {
        drop(
            self.task
                .with_state(|state| ActiveRead::consume_read(self.read, state)),
        );
    }
}

pub struct WasmImport<T, const N: usize> {
    /// Storage space for the return value.
    space: [usize; N],

    /// Indirect function used to deserialize the `space` of the return value.
    ///
    /// This is set to `None` once it's called.
    deserialize: Option<fn(&[usize; N]) -> T>,

    /// Optionally-present child task which this import owns.
    ///
    /// Note that this may not be present for two reasons:
    ///
    /// * On construction the import may have already finished and a child task
    ///   may not have been created.
    /// * When this future finishes it yields ownership of the task here, if
    ///   present, to the caller.
    task: Option<WasmTask>,

    /// To the best of my ability I believe that this is correct. The `space` is
    /// used as part of `task_listen` which is a self-pointer, so this structure
    /// can't be moved around, so I think this is correct... right?.... right?
    _marker: PhantomPinned,
}

impl<T, const N: usize> WasmImport<T, N> {
    pub unsafe fn new(
        task: u32,
        space: [usize; N],
        deserialize: fn(&[usize; N]) -> T,
    ) -> WasmImport<T, N> {
        WasmImport {
            space,
            deserialize: Some(deserialize),
            // If the child task completed immediately then we'll get 0 as the
            // task identifier meaning that there's no task created and the
            // `space` can be immediately deserialized. That's flagged here with
            // a lack of `WasmTask` which is handled in the future
            // implementation.
            task: if task == 0 {
                None
            } else {
                Some(WasmTask::new(task))
            },
            _marker: PhantomPinned,
        }
    }

    pub fn and_task(self) -> WasmImportAndTask<T, N> {
        WasmImportAndTask(self)
    }

    fn project(
        self: Pin<&mut Self>,
    ) -> (
        Pin<&mut [usize; N]>,
        &mut Option<fn(&[usize; N]) -> T>,
        &mut Option<WasmTask>,
    ) {
        unsafe {
            let me = self.get_unchecked_mut();
            (
                Pin::new_unchecked(&mut me.space),
                &mut me.deserialize,
                &mut me.task,
            )
        }
    }

    fn poll_result(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<(T, WasmImportTask)> {
        // If a child task is associated with this import then the first thing
        // we need to do is await the completion of the task itself.
        let (space, _deserialize, task) = self.as_mut().project();
        if let Some(task) = task {
            let state = task.with_state(|s| s.clone());
            match state {
                // The child hasn't completed and we're still waiting on the
                // return value, meaning this future isn't done yet.
                //
                // Note that this doesn't use `cx.waker()`. The reason for that
                // is that the call to `canon.task.listen` here automatically
                // reparents the child task to the currently running wasm task
                // that's polling this future. That means that notifications for
                // the child task's readiness will be routed to our task
                // correctly and we'll get woken up to get re-polled.
                //
                // Also note, though, that there is no corresponding call to
                // `canon.task.unlisten` for this call. That should be ok
                // because once we've started listening the `Pin` here ensures
                // that we'll be kept around in memory until we're dropped.
                // Memory-safety-wise that means we're fine and it means that
                // this future can track the return value until it's dropped.
                //
                // On drop this future will also drop the wasm task import. That
                // means that we'll automatically un-listen anyway, so we should
                // be able to safely avoid `canon.task.unlisten` anywhere.
                ChildState::Waiting => {
                    unsafe {
                        task_listen_future(task.id(), space.as_ptr() as usize);
                    }
                    return Poll::Pending;
                }

                // The child has returned, meaning the return value is filled
                // in and we can get down below to process it.
                ChildState::Returned => {}

                // The child is completely done, meaning that the return value
                // is also available. Note that this is possible when this
                // future isn't polled immediately in reaction to the "returned"
                // notification and the future then reaches the done state,
                // finally resulting in polling the future here.
                ChildState::Done => {}

                // This is only used for futures right now, not for child tasks
                // which are streams.
                ChildState::Wrote(_) => unreachable!(),
            }
        }
        Poll::Ready(self.take_result())
    }

    fn take_result(self: Pin<&mut Self>) -> (T, WasmImportTask) {
        let (space, deserialize, task) = self.project();
        if cfg!(debug_assertions) {
            if let Some(task) = task {
                task.with_state(|s| match s {
                    ChildState::Returned | ChildState::Done => {}
                    ChildState::Wrote(_) | ChildState::Waiting => {
                        panic!("task has not finished yet");
                    }
                });
            }
        }
        (
            deserialize.take().expect("already finished")(space.as_ref().get_ref()),
            WasmImportTask { task: task.take() },
        )
    }
}

impl<T, const N: usize> Future for WasmImport<T, N> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        match self.poll_result(cx) {
            Poll::Ready((result, _task)) => Poll::Ready(result),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<T, const N: usize> Drop for WasmImport<T, N> {
    fn drop(&mut self) {
        // NB: One thing this destructor might be inclined to do is to
        // `task_unlisten` from the future if we're listening to it. This
        // shouldn't be necessary, though, since when `WasmImport` is fully
        // dropped it'll drop the task anyway. This means that the task will
        // automatically be un-listened regardless.

        // The return value `T` here may have a significant destructor and/or
        // memory owned behind it. If the return value is in `self.space` then
        // that means the destructor needs to run because the value was never
        // read from the future. This is manually done here if necessary and
        // `self.deserialized` is `false`.
        if std::mem::needs_drop::<T>() {
            if let Some(deserialize) = self.deserialize.take() {
                let needs_deserialize = match &self.task {
                    // With a task present manual deserialization here is only
                    // necessary if the child is in the returned state. That means
                    // that we got the notification that the return value is in our
                    // module but this future was dropped before being polled to
                    // find the value.
                    Some(task) => task.with_state(|s| matches!(s, ChildState::Returned)),

                    // With a task not present then that means that the original
                    // import finished immediately. This future was never polled so
                    // we need to clean up the allocations, if any, here.
                    None => true,
                };
                if needs_deserialize {
                    drop(deserialize(&self.space));
                }
            }
        }
    }
}

pub struct WasmImportAndTask<T, const N: usize>(WasmImport<T, N>);

impl<T, const N: usize> WasmImportAndTask<T, N> {
    pub fn cancel(self) -> Option<WasmImportTask> {
        self.0.cancel()
    }
}

impl<T, const N: usize> Future for WasmImportAndTask<T, N> {
    type Output = (T, WasmImportTask);

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<(T, WasmImportTask)> {
        unsafe { self.map_unchecked_mut(|t| &mut t.0).poll_result(cx) }
    }
}

pub struct WasmImportTask {
    task: Option<WasmTask>,
}

impl WasmImportTask {
    /// Awaits for the full completion of this child task where all of its
    /// post-return work has also completed.
    pub async fn done(&mut self) {
        let task = match &mut self.task {
            Some(task) => task,
            None => return,
        };

        // Note that `await_child_state` will automatically reparent the task to
        // the caller if necessary, meaning that we're guaranteed to get
        // notifications for the child's state changing.
        task.await_child_state(|state| match state {
            ChildState::Done => Poll::Ready(()),
            _ => Poll::Pending,
        })
        .await
    }

    pub fn cancel(&mut self) {
        if let Some(task) = self.task.take() {
            self.task = task.cancel();
        }
    }
}
