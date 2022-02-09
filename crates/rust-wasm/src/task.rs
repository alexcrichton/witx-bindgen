use self::cond::Cond;
use std::collections::HashMap;
use std::convert::TryFrom;
use std::future::Future;
use std::marker::{PhantomData, Unpin};
use std::mem;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};

const ACTION_WAIT: u32 = 0;
const ACTION_FINISH: u32 = 1;

const CAUSE_CHILD_WRITE: u32 = 0;
const CAUSE_CHILD_RETURN: u32 = 1;
const CAUSE_CHILD_DONE: u32 = 2;
const CAUSE_SHARED_WRITE: u32 = 3;
const CAUSE_SHARED_RETURN: u32 = 4;
const CAUSE_COND_AWOKEN: u32 = 5;
const CAUSE_COND_BROKEN: u32 = 6;

const RETURN_NOT_STARTED: usize = 0;
const RETURN_STARTED: usize = 1;
const RETURN_ACKNOWLEDGED: usize = 2;

pub mod sample {
    use super::WriteStream;

    pub mod generated_abi_glue_for_exports {
        use super::super::{TaskAbi, WriteStream};

        #[no_mangle]
        pub unsafe extern "C" fn the_export(a: usize, b: usize) -> usize {
            let (a, b) = TaskAbi::run(async move {
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
                let mut ret_area = [0; 2];
                ret_area[0] = ret_0;
                ret_area[1] = ret_1;
                TaskAbi::await_return(ret_area.as_ptr() as usize).await;
            });

            // "return multiple values" through memory
            RET_AREA[0] = a as u64;
            RET_AREA[1] = b as u64;
            RET_AREA.as_ptr() as usize
        }

        #[no_mangle]
        pub unsafe extern "C" fn the_export_on_event(a: usize, b: u32, c: u32) -> u32 {
            TaskAbi::on_event_abi(a, b, c)
        }

        #[no_mangle]
        pub unsafe extern "C" fn the_export_post_return(a: usize) -> u32 {
            TaskAbi::post_return_abi(a)
        }

        #[no_mangle]
        pub unsafe extern "C" fn the_export_cancel(a: usize) -> u32 {
            TaskAbi::cancel_abi(a)
        }

        #[no_mangle]
        pub unsafe extern "C" fn stream_export(a: usize, b: usize) -> usize {
            let (a, b) = TaskAbi::run(async move {
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
                let mut ret_area = [0; 2];
                ret_area[0] = ret_0;
                ret_area[1] = ret_1;
                TaskAbi::await_return(ret_area.as_ptr() as usize).await;
            });

            // "return multiple values" through memory
            RET_AREA[0] = a as u64;
            RET_AREA[1] = b as u64;
            RET_AREA.as_ptr() as usize
        }

        #[no_mangle]
        pub unsafe extern "C" fn stream_export_post_write(a: usize, b: usize) -> u32 {
            TaskAbi::post_write_abi(a, b)
        }

        static mut RET_AREA: [u64; 8] = [0; 8];
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
        use super::super::{Stream, WasmImport};

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

        pub fn some_import(a: &str) -> WasmImport<String, 2> {
            unsafe {
                let mut ret = [0usize; 2];
                let ret_ptr = ret.as_mut_ptr() as usize;
                let task = abi::some_import(a.as_ptr() as usize, a.len(), ret_ptr);
                WasmImport::new(task, ret, |ret| {
                    let ptr = ret[0] as *mut u8;
                    let len = ret[1] as usize;
                    String::from_utf8_unchecked(Vec::from_raw_parts(ptr, len, len))
                })
            }
        }

        pub fn stream_import(a: &str) -> Stream<u32, (), 0> {
            unsafe {
                let task = abi::stream_import(a.as_ptr() as usize, a.len(), 0, 0, 0);
                let import = WasmImport::new(task, [], |_| {});
                Stream::new(import)
            }
        }
    }
}

pub struct TaskAbi {
    future: Option<Pin<Box<dyn Future<Output = ()>>>>,
    state: Arc<TaskState>,
}

pub struct TaskState {
    cond: Cond,
    children: Mutex<HashMap<u32, ChildState>>,
    write: Mutex<WriteState>,
    return_state: AtomicUsize,
}

#[derive(PartialEq, Debug)]
enum WriteState {
    Wrote { amt: usize },
    Writing,
    None,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ChildState {
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

enum Action {
    Wait(Box<TaskAbi>),
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

impl TaskAbi {
    /// ABI entry point for the `callee` callback.
    ///
    /// The actual callee is intended to be represented with the `future`
    /// provided.
    pub fn run(future: impl Future<Output = ()> + 'static) -> (usize, u32) {
        // Create the initial state for this task, which involves no children
        // but a default condvar to support cross-task wakeups.
        let task = Box::new(TaskAbi {
            future: Some(Box::pin(future)),
            state: Arc::new(TaskState {
                cond: Cond::new(),
                children: Mutex::default(),
                write: Mutex::new(WriteState::None),
                return_state: AtomicUsize::new(RETURN_NOT_STARTED),
            }),
        });

        // Flag initial interest in any wakeups on this `Cond` since they may
        // come from sibling tasks.
        task.state.cond.wait();

        // Perform the initial `poll` to determine what state this task is now
        // in, and then return the results.
        match task.poll() {
            Action::Wait(task) => (Box::into_raw(task) as usize, ACTION_WAIT),
            Action::Finish => (0, ACTION_FINISH),
        }
    }

    /// ABI entry point for the `on-event` callback.
    pub unsafe fn on_event_abi(ptr: usize, cause: u32, idx: u32) -> u32 {
        let task = Self::raw_to_box(ptr);

        // Process `cause` and `x` by updating internal references to this
        // task's state.
        let mut children = task.state.children.lock().unwrap();
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
        task.poll().encode(ptr)
    }

    /// ABI entry point for the `post-write` callback.
    pub unsafe fn post_write_abi(ptr: usize, amt: usize) -> u32 {
        let task = Self::raw_to_box(ptr);
        let mut write = task.state.write.lock().unwrap();
        let prev = mem::replace(&mut *write, WriteState::Wrote { amt });
        debug_assert_eq!(prev, WriteState::Writing);
        drop(write);

        // Perform an internal future poll to determine what to do next
        task.poll().encode(ptr)
    }

    /// ABI entry point for the `post-return` callback.
    pub unsafe fn post_return_abi(ptr: usize) -> u32 {
        let task = Self::raw_to_box(ptr);

        // Indicate that our request to return has been acknowledged, and then
        // afterwards a normal poll happens as usual. When the future returned
        // by `TaskAbi::await_return` is re-polled eventually it will pick up
        // this new return state value and resolve itself.
        let prev = task.state.return_state.swap(RETURN_ACKNOWLEDGED, SeqCst);
        debug_assert_eq!(prev, RETURN_STARTED);
        task.poll().encode(ptr)
    }

    fn poll(mut self: Box<Self>) -> Action {
        if let Some(mut future) = self.future.take() {
            let waker = Waker::from(self.state.clone());
            let mut context = Context::from_waker(&waker);
            self.future = tls::set(&self.state, || match future.as_mut().poll(&mut context) {
                Poll::Ready(()) => Some(future),
                Poll::Pending => None,
            });
        }

        if self.future.is_some() || self.state.children.lock().unwrap().len() > 0 {
            return Action::Wait(self);
        }

        Action::Finish
    }

    /// ABI entry point for the `cancel` callback.
    pub unsafe fn cancel_abi(ptr: usize) -> u32 {
        let mut me = Self::raw_to_box(ptr);
        let future = me.future.take();
        if future.is_some() {
            tls::set(&me.state, || drop(future));
        }
        me.poll().encode(ptr)
    }

    unsafe fn raw_to_box(ptr: usize) -> Box<TaskAbi> {
        Box::from_raw(ptr as *mut TaskAbi)
    }

    /// Internal function to represent signalling a return value.
    //
    // TODO: need to figure out the ownership story around `retptr` and who's
    // responsible for cleaning up what depending on if anything gets cancelled
    // and stuff like that.
    pub async fn await_return(retptr: usize) {
        // Setup state indicating we're "returning"
        let ret = TaskReturn::new(retptr);

        // Wait for the return to get acknowledged through the `post-return`
        // callback. Note that this `.await` point is cancel-able, hence the
        // `Drop for TaskReturn` which un-sets the returning state if it's not
        // acknowledged.
        await_state(|state| match state.return_state.load(SeqCst) {
            RETURN_ACKNOWLEDGED => Poll::Ready(()),
            other => {
                debug_assert_eq!(other, RETURN_STARTED);
                Poll::Pending
            }
        })
        .await;

        // The return has been acknowledged, so there's nothing else to clean
        // up.
        return ret.forget();

        struct TaskReturn;

        #[link(wasm_import_module = "canonical_abi")]
        extern "C" {
            fn task_return(ptr: usize);
            fn task_unreturn();
        }

        impl TaskReturn {
            fn new(ptr: usize) -> TaskReturn {
                tls::with(|task| {
                    let prev = task.return_state.swap(RETURN_STARTED, SeqCst);
                    debug_assert_eq!(prev, RETURN_NOT_STARTED);
                });
                unsafe {
                    task_return(ptr);
                }

                TaskReturn
            }

            fn forget(self) {
                if cfg!(debug_assertions) {
                    tls::with(|task| {
                        let state = task.return_state.load(SeqCst);
                        assert_eq!(state, RETURN_ACKNOWLEDGED);
                    });
                }
                mem::forget(self)
            }
        }

        impl Drop for TaskReturn {
            fn drop(&mut self) {
                tls::with(|task| {
                    match task.return_state.load(SeqCst) {
                        // If the return value was acknowledged and this value
                        // is dropped then that means that the `await_return`
                        // future was dropped, despite the return being
                        // acknowledged. In this case there's no cleanup for us
                        // to perform.
                        RETURN_ACKNOWLEDGED => {}

                        // Otherwise this return state was dropped in the
                        // process of returning, so undo the return state
                        // associated with this task.
                        other => {
                            debug_assert_eq!(other, RETURN_STARTED);
                            task.return_state.store(RETURN_NOT_STARTED, SeqCst);
                            unsafe {
                                task_unreturn();
                            }
                        }
                    }
                });
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

    #[link(wasm_import_module = "canonical_abi")]
    extern "C" {
        fn cond_new() -> u32;
        fn cond_destroy(x: u32);
        fn cond_wait(x: u32);
        fn cond_unwait(x: u32);
        fn cond_notify(x: u32, n: u32) -> u32;
    }

    impl Cond {
        pub fn new() -> Cond {
            Cond(unsafe { cond_new() })
        }

        pub fn wait(&self) {
            unsafe {
                cond_wait(self.0);
            }
        }

        pub fn unwait(&self) {
            unsafe {
                cond_unwait(self.0);
            }
        }

        pub fn notify(&self, n: u32) -> u32 {
            unsafe { cond_notify(self.0, n) }
        }
    }

    impl Drop for Cond {
        fn drop(&mut self) {
            unsafe {
                cond_destroy(self.0);
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

use self::wasm_task::TaskListen;
pub use self::wasm_task::WasmTask;

mod wasm_task {
    use super::{tls, ChildState};
    use std::task::Poll;

    /// Helper structure used to manage a child task.
    pub struct WasmTask(u32);

    #[link(wasm_import_module = "canonical_abi")]
    extern "C" {
        fn task_drop(task: u32);
    }

    impl WasmTask {
        /// Takes ownership of the `task` identifier specified and returns an owned
        /// representation of the task which will drop the task on drop of
        /// `WasmTask`.
        pub fn new(task: u32) -> WasmTask {
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
        pub async fn await_child_state<F, T>(
            &self,
            start_listening: impl FnOnce(&Self) -> TaskListen<'_>,
            mut test: F,
        ) -> T
        where
            F: FnMut(&mut ChildState) -> Poll<T> + Unpin,
        {
            // Avoid the `start_listening` callback which calls
            // `canon.task.listen` if we're already ready.
            if let Poll::Ready(result) = self.with_state(&mut test) {
                return result;
            }
            let _listen = start_listening(self);
            super::await_state(|state| self.process_state(state, &mut test)).await
        }

        pub fn id(&self) -> u32 {
            self.0
        }

        /// NB: panics outside of an exported task
        pub fn with_state<R>(&self, with: impl FnMut(&mut ChildState) -> R) -> R {
            tls::with(|state| self.process_state(state, with))
        }

        pub fn process_state<R>(
            &self,
            state: &super::TaskState,
            mut with: impl FnMut(&mut ChildState) -> R,
        ) -> R {
            let mut children = state.children.lock().unwrap();
            let state = children
                .get_mut(&self.id())
                .expect("cannot use a wasm task on a sibling task");
            with(state)
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

    pub use self::listen::TaskListen;
    mod listen {
        use super::WasmTask;

        #[link(wasm_import_module = "canonical_abi")]
        extern "C" {
            fn future_listen(task: u32, ret: usize);
            fn stream_listen(task: u32, buf: usize, len: usize, retptr: usize);
            fn task_unlisten(task: u32);
        }

        /// RAII guard for staring a listen operation on a task which finishes
        /// the listen operation when this is dropped.
        pub struct TaskListen<'a>(&'a WasmTask);

        impl<'a> TaskListen<'a> {
            pub unsafe fn new_future(task: &'a WasmTask, retptr: usize) -> TaskListen<'a> {
                future_listen(task.id(), retptr);
                TaskListen(task)
            }

            pub unsafe fn new_stream(
                task: &'a WasmTask,
                buf: usize,
                len: usize,
                retptr: usize,
            ) -> TaskListen<'a> {
                stream_listen(task.id(), buf, len, retptr);
                TaskListen(task)
            }

            pub fn forget(self) {
                std::mem::forget(self);
            }
        }

        impl Drop for TaskListen<'_> {
            fn drop(&mut self) {
                unsafe {
                    task_unlisten(self.0.id());
                }
            }
        }
    }
}

pub struct Stream<T, U, const N: usize> {
    _wat: fn() -> T,
    import: WasmImport<U, N>,
}

impl<T, U, const N: usize> Stream<T, U, N> {
    fn new(import: WasmImport<U, N>) -> Self
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
    pub fn result(mut self) -> (U, WasmImportTask) {
        if let Some(task) = &self.import.task {
            task.with_state(|s| match s {
                ChildState::Returned | ChildState::Done => {}
                ChildState::Wrote(_) | ChildState::Waiting => {
                    panic!("task has not finished yet");
                }
            });
        }
        self.import.take_result()
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
        let task = self.stream.import.task.as_ref().unwrap();

        // Create the in-memory state to track the active state of this read.
        // Note that this has a destructor which is significant in accounting
        // for items read as part of this call if the future is dropped at the
        // cancellation point below.
        let mut in_progress = ActiveRead::new(task, &mut self.read);

        // Await the child task to enter a state which indicates that the read
        // is finished. This will start out by calling the `canon.task.listen`
        // intrinsic (the `TaskListen` structure) with the buffer provided to
        // this instance. We'll then await the child task to either enter a
        // `Wrote` state (indicating data was written) or the `Finished` state
        // (indicating data will never be written).
        //
        // Note that this is a cancellation point which we need to handle here
        // as well. To handle cancellation there's two components. One is that
        // internally the `TaskListen` will be dropped, un-registering interest
        // in this stream. The second is that the `ActiveRead` in the stack
        // frame here will ensure that any written amount signaled via the
        // `on-event` callback makes its way into the `StreamWithBuffer`
        // instance instead of losing it by accident.
        task.await_child_state(
            |task| unsafe {
                TaskListen::new_stream(
                    task,
                    self.buf.as_ptr() as usize,
                    self.buf.len() as usize,
                    self.stream.import.space.as_ptr() as usize,
                )
            },
            |state| in_progress.consume_read(state),
        )
        .await;

        // After making it this far the read is fully complete so there's no
        // need for the cleanup logic of `ActiveRead` since the state is already
        // in a good spot again. The actual return value from this function is
        // pulled out of `self.read` which should have been appropriately filled
        // in by `ActiveRead`.
        in_progress.forget();
        return self.count();

        struct ActiveRead<'a, 'b> {
            task: &'a WasmTask,
            read: &'b mut usize,
        }

        impl<'a, 'b> ActiveRead<'a, 'b> {
            fn new(task: &'a WasmTask, read: &'b mut usize) -> Self {
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

            /// Extracts the `Wrote` amount from `state` and moves it into
            /// `self.read`, if that state is found.
            ///
            /// Returns whether the child state is either `Wrote` or `Returned`,
            /// meaning that either a write has finished or will never again
            /// finish.
            fn consume_read(&mut self, state: &mut ChildState) -> Poll<()> {
                match *state {
                    ChildState::Waiting => Poll::Pending,
                    ChildState::Wrote(n) => {
                        *self.read += n;
                        *state = ChildState::Waiting;
                        Poll::Ready(())
                    }
                    ChildState::Returned | ChildState::Done => Poll::Ready(()),
                }
            }

            fn forget(self) {
                // It's an invariant that we never leave the state of the child
                // in `ChildState::Wrote` so double-check that here.
                if cfg!(debug_assertions) {
                    self.task.with_state(|state| {
                        match state {
                            ChildState::Wrote(_) => {
                                panic!("dropping an active read without consuming previously-written amount");
                            }

                            // These are valid states for when this active read
                            // is dropped.
                            ChildState::Waiting | ChildState::Returned | ChildState::Done=> {}
                        }
                    });
                }

                mem::forget(self);
            }
        }

        // This state is responsible for never leaving the task in the
        // `ChildState::Wrote` state. In this destructor the amount written is
        // pulled out and stored in `self.read` which can later be used with
        // `StreamWithBuffer::count` to learn about a partial read that raced
        // with a Rust-level cancellation of a future.
        impl Drop for ActiveRead<'_, '_> {
            fn drop(&mut self) {
                drop(self.task.with_state(|state| self.consume_read(state)));
            }
        }
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

    /// Performs an asynchronous write of `data` into this stream, returning how
    /// many items were written into the stream.
    ///
    /// Note that partial writes may successfully happen and are not reported
    /// immediately. If this future is cancelled this method provides no way of
    /// learning how many items, if any, were written. If partial writes matter
    /// it's recommended to use the `with_buffer` method to determine, after the
    /// write future is cancelled, how many items were written.
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

    /// Executes a write with the buffer and stream attached to this value.
    ///
    /// This will return how many items were written from the buffer. If this
    /// future is dropped (cancelled) then the `count` method can be used to
    /// determine how many items, if any, were written.
    pub async fn write(&mut self) -> usize {
        #[link(wasm_import_module = "canonical_abi")]
        extern "C" {
            fn task_write(ptr: usize, len: usize);
            fn task_unwrite() -> usize;
        }

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
        await_state(|state| in_progress.consume_write(state)).await;

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
                tls::with(|state| {
                    let mut write = state.write.lock().unwrap();
                    let prev = mem::replace(&mut *write, WriteState::Writing);
                    debug_assert_eq!(prev, WriteState::None);
                });

                // Use the `canon.task.write` intrinsic to inform the canonical
                // ABI that we're entering the writing state.
                unsafe {
                    task_write(buf.buf.as_ptr() as usize, buf.buf.len() as usize);
                }

                ActiveWrite { buf }
            }

            fn consume_write(&mut self, state: &TaskState) -> Poll<()> {
                let mut write = state.write.lock().unwrap();
                match &*write {
                    WriteState::Wrote { amt } => {
                        self.buf.written += amt;
                        *write = WriteState::None;
                        Poll::Ready(())
                    }
                    other => {
                        debug_assert_eq!(*other, WriteState::Writing);
                        Poll::Pending
                    }
                }
            }

            fn forget(self) {
                // It's an invariant that the task is back in `WriteState::None`
                // when this type is dropped, so double-check that here if we're
                // going to skip the destructor.
                if cfg!(debug_assertions) {
                    tls::with(|state| {
                        let state = state.write.lock().unwrap();
                        assert_eq!(*state, WriteState::None);
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
                tls::with(|state| {
                    let mut write = state.write.lock().unwrap();
                    let prev = mem::replace(&mut *write, WriteState::None);
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

async fn await_state<T>(f: impl FnMut(&TaskState) -> Poll<T> + Unpin) -> T {
    return AwaitState(f).await;

    struct AwaitState<F>(F);

    impl<F, T> Future for AwaitState<F>
    where
        F: FnMut(&TaskState) -> Poll<T> + Unpin,
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
            tls::with(&mut self.as_mut().0)
        }
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

    /// Listening state of the `WasmTask` above. This is created the first time
    /// the future is polled to register that we're interested in events coming
    /// in from the `WasmTask`. Note that the `'static` here is actually a
    /// self-borrow of `task`.
    listen: Option<TaskListen<'static>>,
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
            listen: None,
        }
    }

    pub fn and_task(self) -> WasmImportAndTask<T, N> {
        WasmImportAndTask(self)
    }

    fn poll_result(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<(T, WasmImportTask)> {
        // If a child task is associated with this import then the first thing
        // we need to do is await the completion of the task itself.
        if let Some(task) = &self.task {
            let state = task.with_state(|s| s.clone());
            match state {
                // The child hasn't completed and we're still waiting on the
                // return value, meaning this future isn't done yet.
                //
                // Note that this doesn't use `cx.waker()`, see the comment in
                // `await_state` for rationale on that.
                //
                // If, however, the `canon.task.listen` intrinsic has not been
                // invoked then now's the time to do that to ensure that the
                // on-event callback is called with the return value becoming
                // available.
                ChildState::Waiting => {
                    if self.listen.is_none() {
                        // TODO: the unsafety with `transmute` here probably
                        // isn't worth it and the intrinsics should probably
                        // just be called directly to avoid having to deal with
                        // RAII weirdness.
                        unsafe {
                            let listen = TaskListen::new_future(task, self.space.as_ptr() as usize);
                            self.listen = Some(
                                mem::transmute::<TaskListen<'_>, TaskListen<'static>>(listen),
                            );
                        }
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

            // Upon reaching this point the return value is available, meaning
            // that the listening state no longer needs to be tracked.
            if let Some(listen) = self.listen.take() {
                listen.forget();
            }
        }
        Poll::Ready(self.take_result())
    }

    fn take_result(&mut self) -> (T, WasmImportTask) {
        if cfg!(debug_assertions) {
            if let Some(task) = &self.task {
                task.with_state(|s| match s {
                    ChildState::Returned | ChildState::Done => {}
                    ChildState::Wrote(_) | ChildState::Waiting => {
                        panic!("task has not finished yet");
                    }
                });
            }
        }
        (
            self.deserialize.take().expect("already finished")(&self.space),
            WasmImportTask {
                task: self.task.take(),
            },
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
        // Note that this comes first because `listen` internally borrows
        // `self.task`, so its destructor, if any, needs to be run before the
        // destructor of `WasmTask`.
        //
        // If we're listening for events on the child task then we're about to
        // drop the child task here so there's no need to track the listening
        // state any more, so forget it if present.
        if let Some(listen) = self.listen.take() {
            listen.forget();
        }

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
    pub async fn done(&self) {
        let task = match &self.task {
            Some(task) => task,
            None => return,
        };

        await_state(|state| {
            task.process_state(state, |s| match s {
                ChildState::Done => Poll::Ready(()),
                _ => Poll::Pending,
            })
        })
        .await
    }
}
