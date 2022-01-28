use self::cond::Cond;
use std::collections::HashMap;
use std::convert::TryFrom;
use std::future::Future;
use std::marker::{PhantomData, Unpin};
use std::mem;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};

const ACTION_SELECT: u32 = 0;
const ACTION_RETURN: u32 = 2;

const CAUSE_CHILD_WRITE: u32 = 1;
const CAUSE_CHILD_RETURN: u32 = 2;
const CAUSE_SHARED_WRITE: u32 = 4;
const CAUSE_SHARED_RETURN: u32 = 5;
const CAUSE_COND_AWOKEN: u32 = 6;
const CAUSE_COND_BROKEN: u32 = 7;

pub mod sample {
    use super::WriteStream;

    pub mod generated_abi_glue_for_exports {
        use super::super::{TaskAbi, WriteStream};

        #[no_mangle]
        pub unsafe extern "C" fn the_export(a: usize, b: usize) -> usize {
            let (a, b, c) = TaskAbi::run(async move {
                // deserialize canonical abi
                let arg = String::from_utf8_unchecked(Vec::from_raw_parts(a as *mut u8, b, b));

                // call the actual user-defined function
                let ret = super::the_export(arg).await;

                // serialize into the canonical abi
                let ret = ret.into_bytes().into_boxed_slice();
                let ret_0 = ret.as_ptr() as u64;
                let ret_1 = ret.len() as u64;
                std::mem::forget(ret);

                // communicate return value
                FUTURE_RET_AREA[0] = ret_0;
                FUTURE_RET_AREA[1] = ret_1;
                FUTURE_RET_AREA.as_ptr() as usize
            });

            // "return multiple values" through memory
            RET_AREA[0] = a as u64;
            RET_AREA[1] = b as u64;
            RET_AREA[2] = c as u64;
            RET_AREA.as_ptr() as usize
        }

        #[no_mangle]
        pub unsafe extern "C" fn the_export_cancel(a: usize) {
            TaskAbi::cancel_abi(a);
        }

        #[no_mangle]
        pub unsafe extern "C" fn stream_export(a: usize, b: usize) -> usize {
            let (a, b, c) = TaskAbi::run(async move {
                // deserialize canonical abi
                let arg = String::from_utf8_unchecked(Vec::from_raw_parts(a as *mut u8, b, b));

                let mut stream = WriteStream::new();

                // call the actual user-defined function
                let ret = super::stream_export(arg, &mut stream).await;

                // serialize into the canonical abi
                let ret = ret.into_bytes().into_boxed_slice();
                let ret_0 = ret.as_ptr() as u64;
                let ret_1 = ret.len() as u64;
                std::mem::forget(ret);

                // communicate return value
                FUTURE_RET_AREA[0] = ret_0;
                FUTURE_RET_AREA[1] = ret_1;
                FUTURE_RET_AREA.as_ptr() as usize
            });

            // "return multiple values" through memory
            RET_AREA[0] = a as u64;
            RET_AREA[1] = b as u64;
            RET_AREA[2] = c as u64;
            RET_AREA.as_ptr() as usize
        }

        #[no_mangle]
        pub unsafe extern "C" fn stream_export_post_write(a: usize, b: usize) -> usize {
            let (a, b) = TaskAbi::post_write_abi(a, b);

            // "return multiple values" through memory
            RET_AREA[0] = a as u64;
            RET_AREA[1] = b as u64;
            RET_AREA.as_ptr() as usize
        }

        // Note that these are 2 separate areas, one for the multi-value returns
        // and one for futures. Both areas are expected to be immediatetly read
        // by the canonical ABI upon signalling them.
        static mut RET_AREA: [u64; 8] = [0; 8];
        static mut FUTURE_RET_AREA: [u64; 8] = [0; 8];
    }

    pub async fn the_export(a: String) -> String {
        let ret = imports::some_import(&a).await;

        let mut some_stream = imports::stream_import(&a);

        let mut buf: Vec<u32> = Vec::new();
        let mut read1 = some_stream.with_buffer(&mut buf);
        let amt = read1.read().await + read1.read().await;
        drop(read1);
        println!("read {} items", amt);

        let mut read2 = some_stream.with_buffer(&mut buf);
        let amt = read2.read().await + read2.read().await;
        drop(read2);
        println!("read {} items afterwards", amt);

        return ret;
    }

    pub async fn stream_export(a: String, stream: &mut WriteStream<u8>) -> String {
        let ret = imports::some_import(&a).await;

        stream.write(&[]).await;
        stream.with_buffer(&[]).write().await;

        return ret;
    }

    pub mod imports {
        use super::super::{Stream, TaskAbi};

        mod abi {
            #[link(wasm_import_module = "the_module_to_import_from")]
            extern "C" {
                pub fn some_import(a: usize, b: usize, ret: usize) -> u32;
                pub fn stream_import(
                    str_ptr: usize,
                    str_len: usize,
                    buf: usize,
                    buf_len: usize,
                    ret: usize,
                ) -> u32;
            }
        }

        pub async fn some_import(a: &str) -> String {
            unsafe {
                let mut ret = [0usize; 2];
                let ret_ptr = ret.as_mut_ptr() as usize;
                let task = abi::some_import(a.as_ptr() as usize, a.len(), ret_ptr);
                TaskAbi::finish_import(task, ret_ptr, || {
                    let ptr = ret[0] as *mut u8;
                    let len = ret[1] as usize;
                    String::from_utf8_unchecked(Vec::from_raw_parts(ptr, len, len))
                })
                .await
            }
        }

        pub fn stream_import(a: &str) -> Stream<u32> {
            unsafe {
                let task = abi::stream_import(a.as_ptr() as usize, a.len(), 0, 0, 0);
                Stream::new(task)
            }
        }
    }
}

#[link(wasm_import_module = "canonical_abi")]
extern "C" {
    fn cond_new() -> u32;
    fn cond_destroy(x: u32);
    fn cond_wait(x: u32);
    fn cond_unwait(x: u32);
    fn cond_notify(x: u32, n: u32) -> u32;

    fn task_listen(task: u32, ret: usize);
    fn stream_listen(task: u32, buf: usize, len: usize);
    fn task_ignore(task: u32);
    fn task_drop(task: u32);
    fn task_start_write(ptr: usize, len: usize);
    fn task_stop_write() -> usize;
}

pub struct TaskAbi {
    future: Box<dyn Future<Output = usize>>,
    state: Arc<TaskState>,
}

pub struct TaskState {
    cond: Cond,
    children: Mutex<HashMap<u32, ChildState>>,
    write: Mutex<WriteState>,
}

enum WriteState {
    Wrote { amt: usize },
    None,
}

#[derive(Debug, Clone)]
enum ChildState {
    Waiting,
    Wrote(usize),
    Returned,
}

impl ChildState {
    fn update_to(&mut self, new_state: ChildState) {
        match self {
            ChildState::Waiting => *self = new_state,
            other => panic!("update to state while not waiting {:?}", other),
        }
    }
}

enum Action {
    Select(Box<TaskAbi>),
    Return(usize),
}

impl TaskAbi {
    /// ABI entry point for the `callee` callback.
    ///
    /// The actual callee is intended to be represented with the `future`
    /// provided.
    pub fn run(future: impl Future<Output = usize> + 'static) -> (usize, u32, usize) {
        // Create the initial state for this task, which involves no children
        // but a default condvar to support cross-task wakeups.
        let task = Box::new(TaskAbi {
            future: Box::new(future),
            state: Arc::new(TaskState {
                cond: Cond::new(),
                children: Mutex::default(),
                write: Mutex::new(WriteState::None),
            }),
        });

        // Flag initial interest in any wakeups on this `Cond` since they may
        // come from sibling tasks.
        task.state.cond.wait();

        // Perform the initial `poll` to determine what state this task is now
        // in, and then return the results.
        match task.poll() {
            Action::Select(task) => (Box::into_raw(task) as usize, ACTION_SELECT, 0),
            Action::Return(val) => (0, ACTION_RETURN, val),
        }
    }

    /// ABI entry point for the `on-event` callback.
    pub unsafe fn on_event_abi(ptr: usize, cause: u32, x: u32) -> (u32, usize) {
        let task = Self::raw_to_box(ptr);

        // Process `cause` and `x` by updating internal references to this
        // task's state.
        let mut children = task.state.children.lock().unwrap();
        if cause & 0xf == CAUSE_CHILD_WRITE {
            let amt = cause >> 4;
            children
                .get_mut(&x)
                .unwrap()
                .update_to(ChildState::Wrote(usize::try_from(amt).unwrap()));
        } else {
            match cause {
                // Child completion is signaled through the `children` array
                // which will subsequently resolve futures that represent
                // calls to imported functions. When re-polled these futures
                // will consult this internal state.
                CAUSE_CHILD_RETURN => children
                    .get_mut(&x)
                    .unwrap()
                    .update_to(ChildState::Returned),

                // Not implemented yet.
                CAUSE_SHARED_WRITE => unimplemented!(),
                CAUSE_SHARED_RETURN => unimplemented!(),

                // For now this means that our internal condvar was awoken
                // We're interested in future wakeups in this condvar so we
                // re-wait.
                CAUSE_COND_AWOKEN => task.state.cond.wait(),

                // TODO: how would this be exposed to Rust in a way that it can
                // act upon it?
                CAUSE_COND_BROKEN => {
                    panic!("received `CondBroken` and cannot recover");
                }

                cause => {
                    if cfg!(debug_assertions) {
                        panic!("unkonwn cause in on-event: {}", cause);
                    }
                }
            }
        }
        drop(children);

        // Perform an internal future poll to determine what to do next
        match task.poll() {
            Action::Select(task) => {
                drop(Box::into_raw(task)); // cancel the dtor
                (ACTION_SELECT, 0)
            }
            Action::Return(val) => (ACTION_RETURN, val),
        }
    }

    /// ABI entry point for the `post-write` callback.
    pub unsafe fn post_write_abi(ptr: usize, amt: usize) -> (u32, usize) {
        let task = Self::raw_to_box(ptr);
        let mut write = task.state.write.lock().unwrap();
        match mem::replace(&mut *write, WriteState::Wrote { amt }) {
            WriteState::None => {}
            WriteState::Wrote { .. } => panic!("overwrote wrote state"),
        }
        drop(write);

        // Perform an internal future poll to determine what to do next
        match task.poll() {
            Action::Select(task) => {
                drop(Box::into_raw(task)); // cancel the dtor
                (ACTION_SELECT, 0)
            }
            Action::Return(val) => (ACTION_RETURN, val),
        }
    }

    /// Performs a `poll` on the internal future to determine its current
    /// status. Returns an appropriate action representing what to do next with
    /// this future in the canonical abi.
    fn poll(mut self: Box<Self>) -> Action {
        let waker = Waker::from(self.state.clone());
        let mut context = Context::from_waker(&waker);

        // Honestly I barely understand `Pin`. I can't figure out how to work
        // with it that requires `unsafe` everywhere and dilutes the purpose of
        // having it.
        //
        // For now `self.future` field is never moved and `self` is also boxed
        // up in memory so thte lack of movement of pinned futures should be
        // upheld here. Perhaps someone can later figure out a better way to
        // model this.
        let future = unsafe { Pin::new_unchecked(&mut *self.future) };

        match tls::set(&self.state, || future.poll(&mut context)) {
            // If the future has completely resolved then there's nothing else
            // to do and we're 100% done. Destroy `self` since it's no longer
            // needed and the retptr should be in a static area. Then return the
            // retptr.
            Poll::Ready(retptr) => Action::Return(retptr),

            // If the future is still running then we're still in the "Select"
            // state, so signal as such and we preserve our allocation.
            Poll::Pending => Action::Select(self),
        }
    }

    /// ABI entry point for the `cancel` callback.
    pub unsafe fn cancel_abi(ptr: usize) {
        // Cancellation in Rust at this point is simply a `drop`. This will
        // clean up all linear memory state associated with this task.
        // Additionally this will clean up any in-flight writes/returns that
        // weren't otherwise acknowledged.
        //
        // Finally this will also clean up the per-task `Cond` which should
        // automatically un-listen this task from that `Cond`, meaning that
        // after this destruction completes nothing else should be waited on.
        drop(Self::raw_to_box(ptr));
    }

    unsafe fn raw_to_box(ptr: usize) -> Box<TaskAbi> {
        Box::from_raw(ptr as *mut TaskAbi)
    }

    /// Helper function used by generated code for imports to await the
    /// completion of `task`.
    pub async unsafe fn finish_import<T>(
        task: u32,
        retptr: usize,
        deserialize: impl FnOnce() -> T,
    ) -> T {
        // The child task actually completed immediately and no task was
        // created, so the deserialize can happen immediately as `retptr` is
        // already filled in.
        if task == 0 {
            return deserialize();
        }

        // Take ownership of the `task` identifier. This creates a new
        // `WasmTask` helper which will automatically cleanup the task when this
        // is dropped.
        let task = WasmTask::new(task);

        // Schedule the `deserialize` closure to get called if this future is
        // dropped and the child task actually returned. It's possible to get a
        // notification of the future being returned but during the subsequent
        // poll we may not actually pull out the value, but it may still have
        // allocations which need to be cleaned up.
        let mut cleanup = DeserializeOnDrop {
            task,
            deserialize: Some(deserialize),
        };

        // Block waiting for the `Returned` state.
        cleanup
            .task
            .await_state(
                |task| unsafe { task.listen_future(retptr) },
                |s| match s {
                    ChildState::Returned => Poll::Ready(()),
                    ChildState::Waiting => Poll::Pending,
                    ChildState::Wrote(_) => unreachable!(),
                },
            )
            .await;

        // If we got this far then that means the child is in the `Returned`
        // state which means that our `retptr` should have been filled in. That
        // means we can run the deserialize closure, and by taking the closure
        // here it will cancel the cleanup in the destructor of `cleanup`.
        return cleanup.deserialize.take().unwrap()();

        // Helper used above to run `deserialize` on drop if it wasn't otherwise
        // executed by the `return` statement.
        struct DeserializeOnDrop<F, T>
        where
            F: FnOnce() -> T,
        {
            task: WasmTask,
            deserialize: Option<F>,
        }

        impl<F, T> Drop for DeserializeOnDrop<F, T>
        where
            F: FnOnce() -> T,
        {
            fn drop(&mut self) {
                if let Some(deserialize) = self.deserialize.take() {
                    if let ChildState::Returned = self.task.with_state(|s| s.clone()) {
                        deserialize();
                    }
                }
            }
        }
    }
}

impl Wake for TaskState {
    fn wake(self: Arc<Self>) {
        self.cond.notify(1);
    }
}

mod cond {
    pub struct Cond(u32);

    impl Cond {
        pub fn new() -> Cond {
            Cond(unsafe { super::cond_new() })
        }

        pub fn wait(&self) {
            unsafe {
                super::cond_wait(self.0);
            }
        }

        pub fn unwait(&self) {
            unsafe {
                super::cond_unwait(self.0);
            }
        }

        pub fn notify(&self, n: u32) -> u32 {
            unsafe { super::cond_notify(self.0, n) }
        }
    }

    impl Drop for Cond {
        fn drop(&mut self) {
            unsafe {
                super::cond_destroy(self.0);
            }
        }
    }
}

mod tls {
    use super::TaskState;
    use std::cell::Cell;

    thread_local!(static CURRENT: Cell<usize> = Cell::new(0));

    pub fn set<R>(ptr: &TaskState, f: impl FnOnce() -> R) -> R {
        CURRENT.with(|p| {
            let prev = p.get();
            p.set(ptr as *const TaskState as usize);
            let ret = f();
            p.set(prev);
            ret
        })
    }

    pub fn with<R>(f: impl FnOnce(&TaskState) -> R) -> R {
        CURRENT.with(|p| {
            let cur = p.get();
            if cur == 0 {
                panic!("no task context found, is this future polled outside of an async wasm task context?");
            }
            let task = unsafe { &*(cur as *const TaskState) };
            f(task)
        })
    }
}

/// Helper structure used to manage a child task.
struct WasmTask(u32);

impl WasmTask {
    /// Takes ownership of the `task` identifier specified and returns an owned
    /// representation of the task which will drop the task on drop of
    /// `WasmTask`.
    fn new(task: u32) -> WasmTask {
        // Insert state about our new child `task` which will get updated when
        // events happen.
        tls::with(|state| {
            let prev = state
                .children
                .lock()
                .unwrap()
                .insert(task, ChildState::Waiting);
            debug_assert!(prev.is_none());
        });

        WasmTask(task)
    }

    /// Asynchronously awaits the for the `test` of this task's state to return
    /// `true`.
    ///
    /// This will return a future that doesn't resolve until `test` returns
    /// `true`, and then `test`'s `ChildState` is returned.
    ///
    /// The `start_listening` closure specified here should call
    /// `WasmTask::{listen_future,listen_stream}` to activate listening for
    /// state changes in the child.
    async fn await_state<F, T>(&self, start_listening: impl FnOnce(&Self), test: F) -> T
    where
        F: FnMut(&mut ChildState) -> Poll<T> + Unpin,
    {
        // Start the listening process to get notifications to this wasm task
        // about state changes.
        //
        // Note that the paired "ignore" happens in `Drop for AwaitState` below.
        start_listening(self);
        return AwaitState { task: self, test }.await;

        struct AwaitState<'a, F, T>
        where
            F: FnMut(&mut ChildState) -> Poll<T> + Unpin,
        {
            task: &'a WasmTask,
            test: F,
        }

        impl<F, T> Future for AwaitState<'_, F, T>
        where
            F: FnMut(&mut ChildState) -> Poll<T> + Unpin,
        {
            type Output = T;

            fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<T> {
                // NB: note how `cx.waker()` isn't used at all here. This is
                // intentional. The general idea is that this task is bound to
                // the original async wasm task it's executing within, and any
                // events which would otherwise cause this future to get woken
                // up will already poll the top-level future. This effectively
                // means that so long as this future stays within the wasm task
                // that created this `WasmTask` wakeup notifications will work.
                //
                // This means, however, if the future returned by `await_state`
                // is moved to a separate wasm task. If that happens then the
                // notification will come in on the original wasm task that this
                // future is ready, but the sibling wasm task won't be woken up.
                // For now that's intentional. The consequence of this is that
                // if this future is polled on a sibling wasm task the call to
                // `self.task.state()` will panic since the sibling task won't
                // have metadata about this `WasmTask`. It's expected that this
                // situation is a programmer error that should be fixed with
                // some form of explicit transfer or sharing.
                self.task.with_state(|state| (self.test)(state))
            }
        }

        impl<F, T> Drop for AwaitState<'_, F, T>
        where
            F: FnMut(&mut ChildState) -> Poll<T> + Unpin,
        {
            fn drop(&mut self) {
                self.task.ignore();
            }
        }
    }

    fn id(&self) -> u32 {
        self.0
    }

    unsafe fn listen_future(&self, ptr: usize) {
        task_listen(self.id(), ptr);
    }

    unsafe fn listen_stream(&self, ptr: usize, len: usize) {
        stream_listen(self.id(), ptr, len);
    }

    fn ignore(&self) {
        unsafe {
            task_ignore(self.id());
        }
    }

    /// NB: panics if called on a task which doesn't own this `WasmTask`.
    fn with_state<R>(&self, mut with: impl FnMut(&mut ChildState) -> R) -> R {
        tls::with(|state| {
            let mut children = state.children.lock().unwrap();
            let state = children
                .get_mut(&self.id())
                .expect("cannot use a wasm task on a sibling task");
            with(state)
        })
    }
}

impl Drop for WasmTask {
    fn drop(&mut self) {
        // First remove the internal state associated with this child in the
        // current task.
        tls::with(|state| {
            state.children.lock().unwrap().remove(&self.id());
        });

        // Next the task can be fully dropped from the canonical ABI. Note that
        // this will cancel the task if it's still in flight.
        unsafe {
            task_drop(self.id());
        }
    }
}

pub struct Stream<T> {
    _wat: fn() -> T,
    task: WasmTask,
}

impl<T> Stream<T> {
    fn new(task: u32) -> Stream<T>
    where
        T: 'static,
    {
        // TODO: this is a temporary restriction for development, but this
        // restriction only works when `T` is a copy type that has the same
        // representation in Rust as the canonical ABI, so pick `u8` for now.
        assert_eq!(std::any::TypeId::of::<T>(), std::any::TypeId::of::<u8>());

        debug_assert!(task != 0);
        Stream {
            _wat: || panic!(),
            task: WasmTask::new(task),
        }
    }

    pub fn with_buffer<'a>(&mut self, buf: &'a mut [T]) -> StreamWithBuffer<'a, '_, T> {
        StreamWithBuffer::new(self, buf)
    }

    pub async fn read(&mut self, buf: &mut [T]) -> usize {
        self.with_buffer(buf).read().await
    }

    // Hypothetically something that could be added to the canonical abi in the
    // future.
    // async fn ready(&mut self);
}

pub struct StreamWithBuffer<'buf, 'stream, T> {
    stream: &'stream mut Stream<T>,
    buf: &'buf mut [T],
    read_in_progress: bool,
}

impl<'buf, 'stream, T> StreamWithBuffer<'buf, 'stream, T> {
    fn new(stream: &'stream mut Stream<T>, buf: &'buf mut [T]) -> Self {
        StreamWithBuffer {
            stream,
            buf,
            read_in_progress: false,
        }
    }

    /// Registers the stream for listening (on the contained buffer),
    /// unregisters it when dropped.
    ///
    /// Blocks until some data is read into the buffer.
    ///
    /// Returns the number of items read into the buffer.
    pub async fn read(&mut self) -> usize {
        self.read_in_progress = true;
        let ret = self
            .stream
            .task
            .await_state(
                |task| unsafe {
                    task.listen_stream(self.buf.as_ptr() as usize, self.buf.len() as usize)
                },
                |state| match self.take_size(state) {
                    Some(n) => Poll::Ready(n),
                    None => Poll::Pending,
                },
            )
            .await;
        self.read_in_progress = false;
        return ret;
    }

    /// Returns how many items were read into the buffer.
    ///
    /// Will return 0 most of the time. If an in-progress-read was cancelled by
    /// dropping the `read` future then this will return anything read into the
    /// buffer, if any.
    pub fn count(&mut self) -> usize {
        if self.read_in_progress {
            self.stream
                .task
                .with_state(|state| self.take_size(state))
                .unwrap_or(0)
        } else {
            0
        }
    }

    fn take_size(&self, state: &mut ChildState) -> Option<usize> {
        match *state {
            ChildState::Waiting => None,
            ChildState::Wrote(n) => {
                *state = ChildState::Waiting;
                Some(n)
            }
            ChildState::Returned => Some(0),
        }
    }
}

impl<T> Drop for StreamWithBuffer<'_, '_, T> {
    fn drop(&mut self) {
        // Drop this task's internal state about amount-written. Otherwise it
        // might be possible to leak this to the next construction of the type.
        self.count();
    }
}

pub struct WriteStream<T> {
    _marker: PhantomData<fn() -> T>,
}

impl<T> WriteStream<T> {
    fn new() -> WriteStream<T>
    where
        T: 'static,
    {
        // TODO: this is a temporary restriction for development, but this
        // restriction only works when `T` is a copy type that has the same
        // representation in Rust as the canonical ABI, so pick `u8` for now.
        assert_eq!(std::any::TypeId::of::<T>(), std::any::TypeId::of::<u8>());

        WriteStream {
            _marker: PhantomData,
        }
    }

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
    _stream: &'stream mut WriteStream<T>,
    buf: &'buf [T],
    written: usize,
}

impl<'buf, 'stream, T> WriteStreamWithBuffer<'buf, 'stream, T> {
    fn new(stream: &'stream mut WriteStream<T>, buf: &'buf [T]) -> Self {
        WriteStreamWithBuffer {
            _stream: stream,
            buf,
            written: 0,
        }
    }

    pub async fn write(&mut self) -> usize {
        if cfg!(debug_assertions) {
            tls::with(|state| {
                let write = state.write.lock().unwrap();
                match *write {
                    WriteState::None => {}
                    WriteState::Wrote { .. } => panic!("writing while wrote"),
                }
            });
        }

        unsafe {
            task_start_write(self.buf.as_ptr() as usize, self.buf.len() as usize);
        }

        return AwaitWrite {
            finished: false,
            // This closure is invoked if the future is dropped before it's
            // completed, aka if this function's returned future is dropped. In
            // that case we want to stop the active write if it's still active
            // because `self.buf` may no longer be valid in the future.
            // Additionally this allows us to see how many items were written if
            // the post-write callback was called but we didn't make it into
            // dealing with this future. This state is saved into `self.written`
            // to be read from `self.count()` optionally.
            unfinished: || {
                self.written = match unsafe { task_stop_write() } {
                    0 => Self::take_size().unwrap_or(0),
                    n => n,
                };
            },
        }
        .await;

        struct AwaitWrite<F: FnMut()> {
            finished: bool,
            unfinished: F,
        }

        impl<F: FnMut() + Unpin> Future for AwaitWrite<F> {
            type Output = usize;

            fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<usize> {
                match WriteStreamWithBuffer::<u8>::take_size() {
                    Some(size) => {
                        self.finished = true;
                        Poll::Ready(size)
                    }
                    None => Poll::Pending,
                }
            }
        }

        impl<F: FnMut()> Drop for AwaitWrite<F> {
            fn drop(&mut self) {
                if !self.finished {
                    (self.unfinished)();
                }
            }
        }
    }

    pub fn count(&mut self) -> usize {
        mem::replace(&mut self.written, 0)
    }

    fn take_size() -> Option<usize> {
        tls::with(|state| {
            let mut state = state.write.lock().unwrap();
            match *state {
                WriteState::None => None,
                WriteState::Wrote { amt } => {
                    *state = WriteState::None;
                    Some(amt)
                }
            }
        })
    }
}
