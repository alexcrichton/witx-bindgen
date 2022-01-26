// Open questions:
//
// * Almost all `Future` implementations in this file use `tls::with` meaning
//   that they only work in the context of the "one executor". Additionally they
//   don't follow the `Future`-trait-protocol of "arrange for the `Waker` to get
//   woken when ready if you return `Pending`". This all works in the context of
//   the one executor we have but that seems like it's going to break down in
//   the Rust ecosystem, I'm just not sure how.
//
// * `CondBroken` simply panics, unclear how an embedder might configure this.
//
// * There's a pervasive assumption that "the current tls" is always available
//   and corresponds to the current task. This is not correct if wires are
//   crossed and sibling tasks get access to each others children and such.
//   Unclear what sort of corruption here is possible, if any, and what the bad
//   scenarios are. Ideally I guess they should all trap?
//
// * Unclear what to expose for imports. As a pretty opaque `impl Future` thing
//   it's not so bad but if we want to actually expose the underlying task id
//   and/or primitives the exact API and implementation of `WasmTask` is pretty
//   bad. One bad thing right now is that if embedders had access to it then
//   there's a situation where when you're waiting on the `returned` future it
//   might get cancelled but then re-constructed later. That could mean our task
//   actually received the return value and then discarded it due to the way in
//   which the futures were cancelled. Pretty bad? Also programs must wait for
//   the return value to happen first before the done value can be waited on,
//   also pretty awkward.

use self::cond::Cond;
use std::collections::HashMap;
use std::future::Future;
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

    pub mod generated_abi_glue_for_exports {
        use super::super::TaskAbi;

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

        // Note that these are 2 separate areas, one for the multi-value returns
        // and one for futures. Both areas are expected to be immediatetly read
        // by the canonical ABI upon signalling them.
        static mut RET_AREA: [u64; 8] = [0; 8];
        static mut FUTURE_RET_AREA: [u64; 8] = [0; 8];
    }

    pub async fn the_export(a: String) -> String {
        imports::some_import(&a).await
    }

    pub mod imports {
        use super::super::TaskAbi;

        mod abi {
            #[link(wasm_import_module = "the_module_to_import_from")]
            extern "C" {
                pub fn some_import(a: usize, b: usize, ret: usize) -> u32;
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
    fn task_ignore(task: u32);
    fn task_drop(task: u32);
}

pub struct TaskAbi {
    future: Box<dyn Future<Output = usize>>,
    state: Arc<TaskState>,
}

pub struct TaskState {
    cond: Cond,
    // return_state: AtomicUsize,
    // return_area: AtomicUsize,
    children: Mutex<HashMap<u32, ChildState>>,
}

#[derive(Debug, Default)]
struct ChildState {
    returned: bool,
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
            unimplemented!();
            // let amt = cause >> 4;
            // children
            //     .get_mut(&x)
            //     .unwrap()
            //     .update_to(ChildState::Wrote(amt));
        } else {
            match cause {
                // Child completion is signaled through the `children` array
                // which will subsequently resolve futures that represent
                // calls to imported functions. When re-polled these futures
                // will consult this internal state.
                CAUSE_CHILD_RETURN => children.get_mut(&x).unwrap().returned = true,

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
        mut deserialize: impl FnMut() -> T,
    ) -> T {
        // The child task actually completed immediately and no task was
        // created, so the deserialize can happen immediately as `retptr` is
        // already filled in.
        if task == 0 {
            return deserialize();
        }

        // Take ownership of the `task` identifier and schedule a destructor for
        // when this goes out of scope with the `Drop for WasmTask`
        // implementation.
        let task = WasmTask(task);

        // Insert state about our new child `task` which will get updated when a
        // `ChildReturn` event happens.
        tls::with(|state| {
            // TODO: is this tls-always-here assumption valid?
            let state = state.expect("async work attempted outside of `Task::on_event`");
            let prev = state
                .children
                .lock()
                .unwrap()
                .insert(task.0, ChildState::default());
            debug_assert!(prev.is_none());
        });

        // Start listening for the return. The listener here is registered with
        // a pointer to write into, then use a custom future to do the actual
        // waiting.
        task_listen(task.0, retptr);
        return WaitForReturn {
            task,
            deserialize: Some(deserialize),
        }
        .await;

        struct WasmTask(u32);

        impl Drop for WasmTask {
            fn drop(&mut self) {
                // First remove the internal state associated with this child in the
                // current task.
                tls::with(|state| {
                    if let Some(state) = state {
                        state.children.lock().unwrap().remove(&self.0);
                    }
                });

                // Next the task can be fully dropped from the canonical ABI. Note that
                // this will cancel the task if it's still in flight.
                unsafe {
                    task_drop(self.0);
                }
            }
        }

        // Helper future to wait for `WasmTask` to return, using the closure
        // specified to deserialize the return value.
        struct WaitForReturn<F, T>
        where
            F: FnMut() -> T,
        {
            task: WasmTask,
            deserialize: Option<F>,
        }

        impl<F, T> Future for WaitForReturn<F, T>
        where
            F: FnMut() -> T,
        {
            type Output = T;

            fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<T> {
                let (task, deserialize) = unsafe {
                    let me = self.get_unchecked_mut();
                    (&mut me.task, &mut me.deserialize)
                };
                tls::with(|state| {
                    let children = state.unwrap().children.lock().unwrap();

                    // If the child task has not entered the returned state yet
                    // then we're still pending.
                    if !children[&task.0].returned {
                        return Poll::Pending;
                    }

                    Poll::Ready(deserialize.take().unwrap()())
                })
            }
        }

        impl<F, T> Drop for WaitForReturn<F, T>
        where
            F: FnMut() -> T,
        {
            fn drop(&mut self) {
                // When this future is returned the first thing we do is
                // un-register the listen that was done when this was created
                // since we're no longer interested in any events.
                unsafe {
                    task_ignore(self.task.0);
                }

                // Next check to see if the task actually returned and we didn't
                // get it picked up as part of the future. If so then the `ptr`
                // we listened with in `task_listen` was filled in, possibly
                // with owned values, so they need to be cleaned up. The result
                // is deserialized here and then destroyed.
                //
                // TODO: should the result be boxed up and buffered somewhere?
                // Just because this `returned` future is getting ignored
                // doesn't mean that it won't get ignored in the future.
                tls::with(|state| {
                    let children = state.unwrap().children.lock().unwrap();
                    if children[&self.task.0].returned {
                        if let Some(mut deserialize) = self.deserialize.take() {
                            deserialize();
                        }
                    }
                })
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

    pub fn with<R>(f: impl FnOnce(Option<&TaskState>) -> R) -> R {
        CURRENT.with(|p| {
            let cur = p.get();
            let task = unsafe {
                if cur == 0 {
                    None
                } else {
                    Some(&*(cur as *const TaskState))
                }
            };
            f(task)
        })
    }
}
