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
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};

const ACTION_SELECT: u32 = 0;
const ACTION_RETURN: u32 = 2;
const ACTION_DONE: u32 = 3;

const CAUSE_CHILD_WRITE: u32 = 1;
const CAUSE_CHILD_RETURN: u32 = 2;
const CAUSE_CHILD_DONE: u32 = 3;
const CAUSE_SHARED_WRITE: u32 = 4;
const CAUSE_SHARED_RETURN: u32 = 5;
const CAUSE_COND_AWOKEN: u32 = 6;
const CAUSE_COND_BROKEN: u32 = 7;

const RETURN_NOT_HAPPENED: usize = 0;
const RETURN_IN_PROGRESS: usize = 1;
const RETURN_COMPLETE: usize = 2;

pub mod sample {
    use std::future::Future;

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
                let mut ret_area = [0; 2];
                ret_area[0] = ret_0;
                ret_area[1] = ret_1;
                TaskAbi::signal_return(ret_area.as_ptr() as usize, || unsafe {
                    // cleanup return value since it was never received
                    let ptr = ret_area[0] as *mut u8;
                    let len = ret_area[1] as usize;
                    drop(String::from_utf8_unchecked(Vec::from_raw_parts(
                        ptr, len, len,
                    )));
                })
                .await
            });

            // "return multiple values" through memory
            RET_AREA[0] = a as u64;
            RET_AREA[1] = b as u64;
            RET_AREA[2] = c as u64;
            RET_AREA.as_ptr() as usize
        }

        #[no_mangle]
        pub unsafe extern "C" fn export_with_cleanup(a: usize, b: usize) -> usize {
            let (a, b, c) = TaskAbi::run(async move {
                // deserialize canonical abi
                let arg = String::from_utf8_unchecked(Vec::from_raw_parts(a as *mut u8, b, b));

                // call the actual user-defined function
                let (ret, cleanup) = super::export_with_cleanup(arg).await;

                // serialize into the canonical abi
                let ret = ret.into_bytes().into_boxed_slice();
                let ret_0 = ret.as_ptr() as u64;
                let ret_1 = ret.len() as u64;
                std::mem::forget(ret);

                // communicate return value
                let mut ret_area = [0; 2];
                ret_area[0] = ret_0;
                ret_area[1] = ret_1;
                let ret = TaskAbi::signal_return(ret_area.as_ptr() as usize, || unsafe {
                    // cleanup return value since it was never received
                    let ptr = ret_area[0] as *mut u8;
                    let len = ret_area[1] as usize;
                    drop(String::from_utf8_unchecked(Vec::from_raw_parts(
                        ptr, len, len,
                    )));
                });

                // execute both the return communication as well as the
                // user-defined cleanup.
                TaskAbi::join2(cleanup, ret).await
            });

            // "return multiple values" through memory
            RET_AREA[0] = a as u64;
            RET_AREA[1] = b as u64;
            RET_AREA[2] = c as u64;
            RET_AREA.as_ptr() as usize
        }

        static mut RET_AREA: [u64; 8] = [0; 8];
    }

    pub async fn the_export(a: String) -> String {
        imports::some_import(&a).await
    }

    pub async fn export_with_cleanup(a: String) -> (String, impl Future<Output = ()>) {
        let (result, cleanup) = imports::import_with_cleanup(&a).await;
        let cleanup = async move {
            println!("this is some cleanup");
            cleanup.await;
        };

        (result, cleanup)
    }

    pub mod imports {
        use super::super::WasmTask;
        use std::future::Future;

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
                let deserialize = || {
                    let ptr = ret[0] as *mut u8;
                    let len = ret[1] as usize;
                    String::from_utf8_unchecked(Vec::from_raw_parts(ptr, len, len))
                };
                match WasmTask::new(task) {
                    Some(mut task) => {
                        let ret = task
                            .returned(ret_ptr, deserialize)
                            .await
                            .expect("task shouldn't be done previously");
                        task.done().await;
                        ret
                    }
                    None => deserialize(),
                }
            }
        }

        pub async fn import_with_cleanup(a: &str) -> (String, impl Future<Output = ()>) {
            unsafe {
                let mut ret = [0usize; 2];
                let ret_ptr = ret.as_mut_ptr() as usize;
                let task = abi::some_import(a.as_ptr() as usize, a.len(), ret_ptr);
                let deserialize = || {
                    let ptr = ret[0] as *mut u8;
                    let len = ret[1] as usize;
                    String::from_utf8_unchecked(Vec::from_raw_parts(ptr, len, len))
                };
                let (ret, task) = match WasmTask::new(task) {
                    Some(mut task) => {
                        let ret = task
                            .returned(ret_ptr, deserialize)
                            .await
                            .expect("task shouldn't be done previously");
                        (ret, Some(task))
                    }
                    None => (deserialize(), None),
                };

                (ret, async move {
                    if let Some(mut task) = task {
                        task.done().await;
                    }
                })
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
    future: Box<dyn Future<Output = ()>>,
    state: Arc<TaskState>,
}

pub struct TaskState {
    cond: Cond,
    return_state: AtomicUsize,
    return_area: AtomicUsize,
    children: Mutex<HashMap<u32, ChildState>>,
}

#[derive(Debug, Default)]
struct ChildState {
    returned: bool,
    done: bool,
}

impl TaskAbi {
    /// ABI entry point for the `callee` callback.
    ///
    /// The actual callee is intended to be represented with the `future`
    /// provided.
    pub fn run(future: impl Future<Output = ()> + 'static) -> (usize, u32, usize) {
        // Create the initial state for this task, which involves no children
        // but a default condvar to support cross-task wakeups.
        let mut task = Box::new(TaskAbi {
            future: Box::new(future),
            state: Arc::new(TaskState {
                cond: Cond::new(),
                return_state: AtomicUsize::new(RETURN_NOT_HAPPENED),
                return_area: AtomicUsize::new(0),
                children: Mutex::default(),
            }),
        });

        // Flag initial interest in any wakeups on this `Cond` since they may
        // come from sibling tasks.
        task.state.cond.wait();

        // Perform the initial `poll` to determine what state this task is now
        // in, and then return the results.
        let (action, y) = task.poll();
        (Box::into_raw(task) as usize, action, y)
    }

    /// Rust helper function for yielding the return value of this task.
    ///
    /// This is an `async` function which resolves when the returned value is
    /// received. If this async function is interrupted then the `cleanup`
    /// callback provided is invoked to clean up the return value which is
    /// stored in `ret_area` because it was never received.
    pub async fn signal_return(ret_area: usize, cleanup: impl FnMut()) {
        // First update our task's state to indicate that the return area has
        // been provided and that the return operation is ready to be returned
        // from the on-event callback.
        tls::with(|state| {
            let state = state.expect("return outside of task context");
            state.return_area.store(ret_area, SeqCst);
            let prev = state.return_state.swap(RETURN_IN_PROGRESS, SeqCst);
            assert_eq!(prev, RETURN_NOT_HAPPENED);
        });

        SignalReturn(cleanup).await;

        struct SignalReturn<F: FnMut()>(F);

        impl<F> Future for SignalReturn<F>
        where
            F: FnMut(),
        {
            type Output = ();

            fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
                tls::with(|state| {
                    let state = state.expect("return outside of task context");
                    match state.return_state.load(SeqCst) {
                        // If the status of the return is complete, meaning it's
                        // been acknowledged via the post-return callback, then
                        // resolve the future.
                        RETURN_COMPLETE => Poll::Ready(()),

                        // If the return is still incomplete (in-progress) then
                        // the return hasn't been acknowledged and we're not
                        // done yet.
                        //
                        // TODO: like below in `Future for Child` this doesn't
                        // pull out the waker from `Context` because it's
                        // assumed this will get re-polled. Unsure if this is
                        // actually ok or not.
                        RETURN_IN_PROGRESS => Poll::Pending,

                        s => panic!("invalid return state {}", s),
                    }
                })
            }
        }

        impl<F> Drop for SignalReturn<F>
        where
            F: FnMut(),
        {
            fn drop(&mut self) {
                let state = tls::with(|state| state.map(|s| s.return_state.load(SeqCst)));
                if state == Some(RETURN_IN_PROGRESS) {
                    (self.0)()
                }
            }
        }
    }

    /// Helper function to join on `a` and `b`, basically simultaneously
    /// awaiting both of their completions.
    pub async fn join2(a: impl Future<Output = ()>, b: impl Future<Output = ()>) {
        Join2 {
            a,
            b,
            a_done: false,
            b_done: false,
        }
        .await;

        struct Join2<A, B> {
            a: A,
            a_done: bool,
            b: B,
            b_done: bool,
        }

        impl<A, B> Future for Join2<A, B>
        where
            A: Future<Output = ()>,
            B: Future<Output = ()>,
        {
            type Output = ();

            fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
                let (a, b, a_done, b_done) = unsafe {
                    let me = self.get_unchecked_mut();
                    (
                        Pin::new_unchecked(&mut me.a),
                        Pin::new_unchecked(&mut me.b),
                        &mut me.a_done,
                        &mut me.b_done,
                    )
                };

                if !*a_done {
                    if let Poll::Ready(()) = a.poll(cx) {
                        *a_done = true;
                    }
                }
                if !*b_done {
                    if let Poll::Ready(()) = b.poll(cx) {
                        *b_done = true;
                    }
                }

                if *a_done && *b_done {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            }
        }
    }

    /// ABI entry point for the `on-event` callback.
    pub unsafe fn on_event_abi(ptr: usize, cause: u32, x: u32) -> (u32, usize) {
        Self::with_raw(ptr, |task| {
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
                    CAUSE_CHILD_DONE => children.get_mut(&x).unwrap().done = true,

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
            task.poll()
        })
    }

    /// ABI entry point for the `post-return` callback.
    pub unsafe fn post_return_abi(ptr: usize) -> (u32, usize) {
        Self::with_raw(ptr, |task| {
            // Flag the internal return state as being complete now. This will
            // resolve the future returned by `TaskAbi::signal_return` and allow
            // further destructors/completion work to continue.
            let prev = task.state.return_state.swap(RETURN_COMPLETE, SeqCst);
            assert_eq!(prev, RETURN_IN_PROGRESS);

            task.poll()
        })
    }

    /// Performs a `poll` on the internal future to determine its current
    /// status. Returns an appropriate action representing what to do next with
    /// this future in the canonical abi.
    fn poll(&mut self) -> (u32, usize) {
        let prev_return_state = self.state.return_state.load(SeqCst);
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
            // to do and we're 100% done.
            Poll::Ready(()) => (ACTION_DONE, 0),

            // If the future hasn't resolved yet then we check the state of the
            // return value of this future. If our return state has changed then
            // that should mean that the state is now `RETURN_IN_PROGRESS`. THe
            // only stores to this state are in `signal_return` and
            // `post_return_abi` below, and the only one of those that can
            // happen while polling is `signal_return`.
            Poll::Pending => {
                if prev_return_state != self.state.return_state.load(SeqCst) {
                    assert_eq!(prev_return_state, RETURN_NOT_HAPPENED);
                    (ACTION_RETURN, self.state.return_area.load(SeqCst))
                } else {
                    (ACTION_SELECT, 0)
                }
            }
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

    /// Executes the `closure` provided with a temporary reference to the
    /// `TaskAbi`.
    ///
    /// If the closure returns `ACTION_DONE` then the state is cleaned up and
    /// dropped as well.
    unsafe fn with_raw(
        ptr: usize,
        closure: impl FnOnce(&mut TaskAbi) -> (u32, usize),
    ) -> (u32, usize) {
        let (action, y) = closure(&mut *(ptr as *mut TaskAbi));
        if action == ACTION_DONE {
            drop(TaskAbi::raw_to_box(ptr));
        }
        (action, y)
    }
}

impl Wake for TaskState {
    fn wake(self: Arc<Self>) {
        self.cond.notify(1);
    }
}

pub struct WasmTask {
    id: u32,
}

impl WasmTask {
    /// Creates a new wrapper `Child` around the `child` index owned by the
    /// current task.
    ///
    /// The `retptr` argument is where to write the results of this child and is
    /// the reason why this function is `unsafe`.
    ///
    /// Registers the current task to start listening for events on `child`.
    unsafe fn new(id: u32) -> Option<WasmTask> {
        if id == 0 {
            return None;
        }
        // as bookeeping we update the list of children we're waiting on
        // in the current task
        tls::with(|state| {
            // TODO: is this tls-always-here assumption valid?
            let state = state.expect("async work attempted outside of `Task::on_event`");
            let prev = state
                .children
                .lock()
                .unwrap()
                .insert(id, ChildState::default());
            debug_assert!(prev.is_none());
        });

        Some(WasmTask { id })
    }

    /// Waits for this task to generate its returned value.
    ///
    /// The `ptr` provided is where the canonical abi representation of the
    /// result is written, and `deserialize` is used to deserialize that
    /// location into an actual type.
    ///
    /// Returns `None` if the task has already returned, and otherwise returns
    /// `Some` with the result of the task when it's available.
    async unsafe fn returned<T>(
        &mut self,
        ptr: usize,
        deserialize: impl FnMut() -> T,
    ) -> Option<T> {
        // Check first to see if the child has already returned, meaning that we
        // can't wait and `None` must be returned.
        let already_returned = tls::with(|state| {
            let children = state.unwrap().children.lock().unwrap();
            children[&self.id].returned
        });
        if already_returned {
            return None;
        }

        // The child hasn't returned yet so we're going to start listening for
        // the return. Register the listener here with a pointer to write into,
        // then use a custom future to do the actual waiting.
        task_listen(self.id, ptr);
        return Some(WaitForReturn(self, Some(deserialize)).await);

        // Helper future to wait for `WasmTask` to return, using the closure
        // specified to deserialize the return value.
        struct WaitForReturn<'a, F, T>(&'a mut WasmTask, Option<F>)
        where
            F: FnMut() -> T;

        impl<F, T> Future for WaitForReturn<'_, F, T>
        where
            F: FnMut() -> T,
        {
            type Output = T;

            fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<T> {
                let (task, deserialize) = unsafe {
                    let me = self.get_unchecked_mut();
                    (&mut me.0, &mut me.1)
                };
                tls::with(|state| {
                    let children = state.unwrap().children.lock().unwrap();

                    // If the child task has not entered the returned state yet
                    // then we're still pending.
                    if !children[&task.id].returned {
                        return Poll::Pending;
                    }

                    Poll::Ready(deserialize.take().unwrap()())
                })
            }
        }

        impl<F, T> Drop for WaitForReturn<'_, F, T>
        where
            F: FnMut() -> T,
        {
            fn drop(&mut self) {
                // When this future is returned the first thing we do is
                // un-register the listen that was done when this was created
                // since we're no longer interested in any events.
                unsafe {
                    task_ignore(self.0.id);
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
                    if children[&self.0.id].returned {
                        if let Some(mut deserialize) = self.1.take() {
                            deserialize();
                        }
                    }
                })
            }
        }
    }

    async fn done(&mut self) {
        let already_done = tls::with(|state| {
            let children = state.unwrap().children.lock().unwrap();
            let child = &children[&self.id];
            // TODO: this is a weird API, maybe a type-state thing can help
            // here? Unsure otherwise what to do where we're interested in the
            // done signal but not the returned signal. The `task_listen`
            // function takes a pointer to write into and we otherwise don't
            // have that available here.
            assert!(child.returned);

            child.done
        });
        if already_done {
            return;
        }

        unsafe {
            task_listen(self.id, 0);
        }
        WaitForDone(self).await;

        struct WaitForDone<'a>(&'a mut WasmTask);

        impl Future for WaitForDone<'_> {
            type Output = ();

            fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
                tls::with(|state| {
                    let children = state.unwrap().children.lock().unwrap();
                    if children[&self.0.id].returned {
                        Poll::Ready(())
                    } else {
                        Poll::Pending
                    }
                })
            }
        }

        impl Drop for WaitForDone<'_> {
            fn drop(&mut self) {
                unsafe {
                    task_ignore(self.0.id);
                }
            }
        }
    }
}

impl Drop for WasmTask {
    fn drop(&mut self) {
        // First remove the internal state associated with this child in the
        // current task.
        tls::with(|state| {
            if let Some(state) = state {
                state.children.lock().unwrap().remove(&self.id);
            }
        });

        // Next the task can be fully dropped from the canonical ABI. Note that
        // this will cancel the task if it's still in flight.
        unsafe {
            task_drop(self.id);
        }
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
