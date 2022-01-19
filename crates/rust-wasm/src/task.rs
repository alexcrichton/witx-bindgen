use self::cond::Cond;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
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

#[link(wasm_import_module = "canonical_abi")]
extern "C" {
    fn cond_new() -> u32;
    fn cond_destroy(x: u32);
    fn cond_wait(x: u32);
    fn cond_unwait(x: u32);
    fn cond_notify(x: u32, n: u32) -> u32;

    fn task_listen(task: u32, ret: usize);
    // fn task_ignore(task: u32);
    fn task_cancel(task: u32);
}

pub struct TaskAbi {
    future: Box<dyn Future<Output = usize>>,
    state: Arc<TaskState>,
}

pub struct TaskState {
    cond: Cond,
    children: Mutex<HashMap<u32, ChildState>>,
}

#[derive(Debug)]
enum ChildState {
    Waiting,
    Wrote(u32),
    Returned,
    Done,
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

        unsafe {
            let task = Box::into_raw(task) as usize;
            let (action, y) = TaskAbi::on_event_abi(task, CAUSE_COND_AWOKEN, 0);
            (task, action, y)
        }
    }

    /// ABI entry point for the `on-event` callback.
    pub unsafe fn on_event_abi(ptr: usize, cause: u32, x: u32) -> (u32, usize) {
        Self::with_raw(ptr, |task| task.on_event(cause, x))
    }

    fn on_event(&mut self, cause: u32, x: u32) -> (u32, usize) {
        let mut children = self.state.children.lock().unwrap();
        if cause & 0xf == CAUSE_CHILD_WRITE {
            let amt = cause >> 4;
            children
                .get_mut(&x)
                .unwrap()
                .update_to(ChildState::Wrote(amt));
        } else {
            match cause {
                CAUSE_CHILD_RETURN => children
                    .get_mut(&x)
                    .unwrap()
                    .update_to(ChildState::Returned),

                CAUSE_CHILD_DONE => children.get_mut(&x).unwrap().update_to(ChildState::Done),

                CAUSE_SHARED_WRITE => unimplemented!(),

                CAUSE_SHARED_RETURN => unimplemented!(),

                CAUSE_COND_AWOKEN => {
                    // For now this means that our internal condvar was awoken
                    // (or this is the first `poll`). In either case we're
                    // interested in future wakeups in this condvar so we
                    // re-wait.
                    self.state.cond.wait();
                }

                // TODO: how would this be exposed to Rust in a way that it can act
                // upon it?
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

        let waker = Waker::from(self.state.clone());
        let mut context = Context::from_waker(&waker);
        let future = unsafe { Pin::new_unchecked(&mut *self.future) };
        match tls::set(&self.state, || future.poll(&mut context)) {
            Poll::Ready(abi) => (ACTION_RETURN, abi),
            Poll::Pending => (ACTION_SELECT, 0),
        }
    }

    /// ABI entry point for the `post-return` callback.
    pub unsafe fn post_return_abi(ptr: usize) -> (u32, usize) {
        // TODO: this is the location we'd map to `AsyncDrop` if it existed.
        // That doesn't exist in Rust right now so there's not really any great
        // way to model what to do after a task returns other than simply
        // destroying everything internally.
        drop(Self::raw_to_box(ptr));

        // Theoretically this could return `ACTION_SELECT` to wait on some
        // long-running destruction work but that's not modeled right now so
        // this is always "done".
        (ACTION_DONE, 0)
    }

    /// ABI entry point for the `cancel` callback.
    pub unsafe fn cancel_abi(ptr: usize) {
        Self::raw_to_box(ptr).cancel();
    }

    fn cancel(self: Box<Self>) {
        // FIXME: Needs to clean up a pending `Return`

        // TODO: docs
        self.state.cond.unwait();

        // Right now cancellation in Rust is modeled as simply cleaning up
        // linear memory state, deallocating everything associated with this
        // task.
        drop(self);
    }

    unsafe fn raw_to_box(ptr: usize) -> Box<TaskAbi> {
        Box::from_raw(ptr as *mut TaskAbi)
    }

    unsafe fn with_raw<R>(ptr: usize, closure: impl FnOnce(&mut TaskAbi) -> R) -> R {
        closure(&mut *(ptr as *mut TaskAbi))
    }
}

impl ChildState {
    fn update_to(&mut self, new: ChildState) {
        match self {
            ChildState::Waiting => *self = new,
            _ => panic!("invalid {:?} => {:?} transition", self, new),
        }
    }
}

impl Wake for TaskState {
    fn wake(self: Arc<Self>) {
        self.cond.notify(1);
    }
}

/// Executes an import, asynchronously.
///
/// The `buf` argument is a pointer to place the canonical ABI results of the
/// function into.
///
/// The `invoke` closure is invoked first. It receives the `buf` pointer and
/// returns a task ID (as specified by the canonical ABI).
///
/// The `decode` closure is eventually invoked when the `buf` pointer is filled
/// in.
pub async unsafe fn import<T>(
    buf: usize,
    invoke: impl FnOnce(usize) -> u32,
    decode: impl FnOnce(usize) -> T,
) -> T {
    let child = invoke(buf);
    if child != 0 {
        Child::new(child, buf).await
    }
    return decode(buf);
}

struct Child(u32);

impl Child {
    /// Creates a new wrapper `Child` around the `child` index owned by the
    /// current task.
    ///
    /// The `retptr` argument is where to write the results of this child and is
    /// the reason why this function is `unsafe`.
    ///
    /// Registers the current task to start listening for events on `child`.
    unsafe fn new(child: u32, retptr: usize) -> Child {
        // First register our canonical ABI task as listening to `child` with a
        // destination for the results to be written. This means that we'll
        // start getting notifications for the `child` to our canonical ABI
        // task.
        task_listen(child, retptr);

        // ... and as bookeeping we update the list of children we're waiting on
        // in the current task as well.
        tls::with(|state| {
            // TODO: is this tls-always-here assumption valid?
            let state = state.expect("async work attempted outside of `Task::on_event`");
            let prev = state
                .children
                .lock()
                .unwrap()
                .insert(child, ChildState::Waiting);
            debug_assert!(prev.is_none());
        });

        Child(child)
    }
}

impl Future for Child {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<()> {
        // FIXME: what are the implications of not using the waker from
        // `Context` here? Doesn't that force usage with this one
        // particular executor mechanism? Is there a way to be more robust
        // with that?
        //
        // There's basically an implicit assumption that this `poll` function is
        // transitively reached from the original `Task::on_event` callback. Is
        // that always true? Naively I'm thinking so, but if that's not true
        // then this needs to actually handle the waker.

        tls::with(|state| {
            // TODO: is this tls-always-here assumption valid?
            let state = state.expect("async work attempted outside of `Task::on_event`");

            let children = state.children.lock().unwrap();
            match &children[&self.0] {
                // Still waiting for this child to finish, so we're still
                // pending.
                ChildState::Waiting => Poll::Pending,

                // This future is for waiting on a child future, which
                // should never enter the "wrote some data" state since
                // that's only used for streams.
                ChildState::Wrote(_) => panic!("child future in wrote state"),

                // The child has returned its data, so we can actually
                // decode that here (it was written into the pointer
                // specified to `task_listen`).
                //
                // TODO: we still, however, return pending here. The child
                // still needs to transition to the `Done` state to be
                // completely finished and without a great way to model that
                // in Rust we just wait for the `Done` state before
                // progressing.
                ChildState::Returned => Poll::Pending,

                // The child has completely finished, meaning our index is
                // actually no longer valid. In this situation we're done
                // and our future has resolved.
                ChildState::Done => Poll::Ready(()),
            }
        })
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        // First remove the internal state associated with this child in the
        // current task. This should always be present, which is asserted later.
        let state = tls::with(|state| {
            state.and_then(|state| state.children.lock().unwrap().remove(&self.0))
        });

        match state {
            // If our child is in the "done" state then there's nothing
            // else to do after removing it, we've already un-listened it
            // in the canonical ABI because the Done event was received and
            // the id is no longer valid anyway.
            Some(ChildState::Done) => {}

            // Otherwise if our child is in any other state then we are no
            // longer interested in its event or its completion. This means
            // we need to cancel the work, so request that happens.
            Some(_) => unsafe { task_cancel(self.0) },

            None => panic!("dropped outside of `Task::on_event` or child missing"),
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
