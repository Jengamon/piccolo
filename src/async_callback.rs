use std::{
    cell::Cell,
    future::{poll_fn, Future},
    marker::PhantomData,
    mem,
    pin::Pin,
    ptr,
    task::{self, Poll, RawWaker, RawWakerVTable, Waker},
};

use gc_arena::{Collect, DynamicRootSet, Gc, Mutation, StaticCollect};

use crate::{
    stash::{Fetchable, Stashable},
    BoxSequence, Callback, CallbackReturn, Context, Error, Execution, Function, Sequence,
    SequencePoll, Stack, StashedCallback, StashedClosure, StashedError, StashedFunction,
    StashedString, StashedTable, StashedThread, StashedUserData, StashedValue, Thread,
};

/// Return type for futures that are driving an async sequence.
///
/// This performs equivalent actions to [`CallbackReturn`] and the returning variants of
/// [`SequencePoll`], so check those for more information on precisely what these actions mean.
pub enum SequenceReturn<'seq> {
    /// Sequence finished, all of the values in the stack will be returned to the caller.
    Return,
    /// Call the given function with the values in the stack as arguments.
    Call(LocalFunction<'seq>),
    /// Yield the values in the stack.
    Yield(Option<LocalThread<'seq>>),
    /// Resume the given thread with the values in the stack as arguments.
    Resume(LocalThread<'seq>),
}

pub type SeqFuture<'seq> =
    Box<dyn Future<Output = Result<SequenceReturn<'seq>, LocalError<'seq>>> + 'seq>;

#[derive(Collect)]
#[collect(no_drop)]
pub struct AsyncSequence<'gc> {
    fut: SeqFut<'gc>,
    locals: DynamicRootSet<'gc>,
    _invariant: Invariant<'gc>,
}

impl<'gc> AsyncSequence<'gc> {
    /// Create a `Sequence` impl from a Rust future that can suspend, call Lua functions, yield to
    /// Lua, and resume threads as async method calls on a held [`SequenceState`].
    ///
    /// Can be used to implement `Sequence` in a way MUCH easier than manual state machines.
    ///
    /// Currently uses `async` to express what in the future could be better expressed by the
    /// unstable [`std::ops::Coroutine`]. The [`std::task::Context`] available within the created
    /// future is *meaningless* and has a NOOP waker, we are only using `async` as a stable way to
    /// express a more simple Rust coroutine.
    ///
    /// It is possible to integrate async code with `piccolo`, and to even have a method to "wake"
    /// Lua coroutines with a real [`std::task::Waker`], but simply calling an external async method
    /// from the created future here is *not* the way to do it. It will not do what you want, and
    /// probably will result in panics.
    ///
    /// # Panics
    ///
    /// All Rust yields (`.await`) within the created future must occur from calling an async method
    /// on `SequenceState`, and all async methods on `SequenceState` must be `.await`ed when they
    /// are called. Not doing so may result in panics.
    pub fn new_seq<F>(mc: &Mutation<'gc>, create: F) -> BoxSequence<'gc>
    where
        F: for<'seq> FnOnce(SequenceState<'seq>) -> SeqFuture<'seq> + 'static,
    {
        Self::new_seq_with(mc, (), move |_, seq| create(seq))
    }

    /// A version of [`AsyncSequence::new_seq`] that accepts an associated GC root object passed to
    /// the create function.
    ///
    /// This is important because the create function must be 'static, and is not called until the
    /// resulting sequence is first polled.
    pub fn new_seq_with<R, F>(mc: &Mutation<'gc>, root: R, create: F) -> BoxSequence<'gc>
    where
        R: Collect + 'gc,
        F: for<'seq> FnOnce(R, SequenceState<'seq>) -> SeqFuture<'seq> + 'static,
    {
        BoxSequence::new(
            mc,
            Self {
                fut: SeqFut::new(root, create),
                locals: DynamicRootSet::new(mc),
                _invariant: PhantomData,
            },
        )
    }

    /// Create a new callback which invokes the given async sequence in a single step.
    ///
    /// The given create function must implement `Fn` rather than `FnOnce`, becuase the resulting
    /// callback can be called any number of times.
    pub fn new_callback<F>(mc: &Mutation<'gc>, create: F) -> Callback<'gc>
    where
        F: for<'seq> Fn(SequenceState<'seq>) -> SeqFuture<'seq> + 'static,
    {
        Self::new_callback_with(mc, (), move |_, seq| create(seq))
    }

    /// Create a new callback which invokes the given async sequence in a single step.
    ///
    /// In addition to the create function needing to implement `Fn` rather than `FnOnce`, the `R`
    /// root type will also be passed to the create function by *reference* rather than by value.
    pub fn new_callback_with<R, F>(mc: &Mutation<'gc>, root: R, create: F) -> Callback<'gc>
    where
        R: Collect + 'gc,
        F: for<'seq> Fn(&R, SequenceState<'seq>) -> SeqFuture<'seq> + 'static,
    {
        let state = Gc::new(mc, (root, StaticCollect(create)));
        Callback::from_fn_with(mc, state, |state, ctx, _, _| {
            Ok(CallbackReturn::Sequence(Self::new_seq_with(
                &ctx,
                *state,
                |state, seq| {
                    let (root, create) = state.as_ref();
                    (create.0)(&root, seq)
                },
            )))
        })
    }

    fn poll_fut(
        &mut self,
        ctx: Context<'gc>,
        exec: Execution<'gc, '_>,
        mut stack: Stack<'gc, '_>,
        error: Option<Error<'gc>>,
    ) -> Result<SequencePoll<'gc>, Error<'gc>> {
        let mut next_op = None;

        let mut shared = Shared {
            locals: self.locals,
            ctx,
            exec,
            stack: stack.reborrow(),
            error,
            next_op: &mut next_op,
        };
        Ok(match self.fut.poll(&mut shared) {
            Poll::Ready(res) => {
                assert!(
                    next_op.is_none(),
                    "`SequenceState` async method not `await`ed"
                );
                match res? {
                    SeqReturn::Return => SequencePoll::Return,
                    SeqReturn::TailCall { function } => SequencePoll::TailCall(function),
                    SeqReturn::TailYield { to_thread } => SequencePoll::TailYield(to_thread),
                    SeqReturn::TailResume { thread } => SequencePoll::TailResume(thread),
                }
            }
            Poll::Pending => {
                match next_op.expect("`await` of a future other than `SequenceState` methods") {
                    SeqOp::Pending => SequencePoll::Pending,
                    SeqOp::Call { function, bottom } => SequencePoll::Call { function, bottom },
                    SeqOp::Yield { to_thread, bottom } => SequencePoll::Yield { to_thread, bottom },
                    SeqOp::Resume { thread, bottom } => SequencePoll::Resume { thread, bottom },
                }
            }
        })
    }
}

impl<'gc> Sequence<'gc> for AsyncSequence<'gc> {
    fn poll(
        &mut self,
        ctx: Context<'gc>,
        exec: Execution<'gc, '_>,
        stack: Stack<'gc, '_>,
    ) -> Result<SequencePoll<'gc>, Error<'gc>> {
        self.poll_fut(ctx, exec, stack, None)
    }

    fn error(
        &mut self,
        ctx: Context<'gc>,
        exec: Execution<'gc, '_>,
        error: Error<'gc>,
        stack: Stack<'gc, '_>,
    ) -> Result<SequencePoll<'gc>, Error<'gc>> {
        self.poll_fut(ctx, exec, stack, Some(error))
    }
}

/// A local variable for an async sequence.
///
/// Like "stashed values" in the registry, `Local`s are *not* branded with `'gc`. Unlike registry
/// stashed values, they are instead branded by `'seq`, which ensures that they cannot escape the
/// body of the async block driving the [`AsyncSequence`].
///
/// Locals cannot escape their parent future, but they *can* be safely stored outside of
/// [`SequenceState::enter`] *and* across await points. If *only* `Local` variables are used to
/// store all garbage collected values within the future, then resulting `AsyncSequence` will always
/// be properly garbage collected, *even if* there are reference cycles between the held locals and
/// the sequence itself.
///
/// The same cannot be said for registry stashed values! An `AsyncSequence` has its own
/// [`gc_arena::DynamicRootSet`] which allows `Local`s ownership to be tied to that *particular*
/// `AsyncSequence`. If GC values are instead stashed in the global registry, for example with
/// `ctx.stash()`, then those values will live as long as *the global registry itself*, which is as
/// long as the `Lua` instance itself is alive. If such a stashed value indirectly points back to
/// the `AsyncSequence` holding it, this will result in an uncollectable cycle.
///
/// In summary: Do NOT store registry stashed values (`ctx.stash()`) within async sequences, instead
/// only use `Local` variables!
#[derive(Clone)]
pub struct Local<'seq, S> {
    stashed: S,
    _invariant: Invariant<'seq>,
}

impl<'seq, S> Local<'seq, S> {
    fn stash<'gc>(
        mc: &Mutation<'gc>,
        locals: DynamicRootSet<'gc>,
        v: impl Stashable<'gc, Stashed = S>,
    ) -> Self {
        Local {
            stashed: v.stash(mc, locals),
            _invariant: PhantomData,
        }
    }
}

impl<'seq, 'gc, S> Local<'seq, S>
where
    S: Fetchable<'gc>,
{
    fn fetch(&self, locals: DynamicRootSet<'gc>) -> S::Fetched {
        self.stashed.fetch(locals)
    }
}

pub type LocalString<'seq> = Local<'seq, StashedString>;
pub type LocalTable<'seq> = Local<'seq, StashedTable>;
pub type LocalClosure<'seq> = Local<'seq, StashedClosure>;
pub type LocalCallback<'seq> = Local<'seq, StashedCallback>;
pub type LocalThread<'seq> = Local<'seq, StashedThread>;
pub type LocalUserData<'seq> = Local<'seq, StashedUserData>;
pub type LocalFunction<'seq> = Local<'seq, StashedFunction>;
pub type LocalValue<'seq> = Local<'seq, StashedValue>;
pub type LocalError<'seq> = Local<'seq, StashedError>;

/// The held state for a `Sequence` being driven by a Rust async block.
///
/// `SequenceState` and [`Local`] are both branded by a generative `'seq` lifetime to ensure that
/// neither can escape their enclosing async block.
///
/// Many methods on `SequenceState` are async; `.await`ing them causes the outer [`AsyncSequence`]
/// to return a non-tail [`SequencePoll`] value, triggering the appropriate action. If this action
/// results in an error, the async method will return the [`Error`] provided to [`Sequence::error`].
///
/// All async methods on `SequenceState` should be `.await`ed immediately, not doing so may result
/// in panics.
pub struct SequenceState<'seq> {
    _invariant: Invariant<'seq>,
}

impl<'seq> SequenceState<'seq> {
    /// Enter the garbage collector context within an async sequence.
    ///
    /// Unfortunately, today's Rust does not provide any way for generator (async block) state
    /// machines to possibly implement [`gc_arena::Collect`]. Therefore, we must ensure that garbage
    /// collected values **cannot** be directly stored by the enclosing async block. We guard all
    /// access to the garbage collector context to prevent this from happening, similar to the
    /// interface we use from the outside (like `Lua::enter`).
    pub fn enter<F, R>(&mut self, f: F) -> R
    where
        F: for<'gc> FnOnce(
            Context<'gc>,
            Locals<'seq, 'gc>,
            Execution<'gc, '_>,
            Stack<'gc, '_>,
        ) -> R,
        R: 'seq,
    {
        visit_shared(move |shared| {
            f(
                shared.ctx,
                Locals {
                    locals: shared.locals,
                    _invariant: PhantomData,
                },
                shared.exec.reborrow(),
                shared.stack.reborrow(),
            )
        })
    }

    /// A version of [`SequenceState::enter`] which supports failure, and automatically turns any
    /// returned error into an async sequence [`Local`].
    pub fn try_enter<F, R>(&mut self, f: F) -> Result<R, LocalError<'seq>>
    where
        F: for<'gc> FnOnce(
            Context<'gc>,
            Locals<'seq, 'gc>,
            Execution<'gc, '_>,
            Stack<'gc, '_>,
        ) -> Result<R, Error<'gc>>,
        R: 'seq,
    {
        visit_shared(move |shared| {
            f(
                shared.ctx,
                Locals {
                    locals: shared.locals,
                    _invariant: PhantomData,
                },
                shared.exec.reborrow(),
                shared.stack.reborrow(),
            )
            .map_err(|e| Local::stash(&shared.ctx, shared.locals, e))
        })
    }

    /// Return [`SequencePoll::Pending`] to the code driving the `Sequence`.
    ///
    /// In normal use, this will return control to the calling `Executor` and potentially the
    /// calling Rust code.
    ///
    /// This usually also allows garbage collection to take place, (depending on how the `Executor`
    /// is being driven).
    pub async fn pending(&mut self) {
        visit_shared(move |shared| {
            shared.set_next_op(SeqOp::Pending);
        });
        wait_once().await;
        visit_shared(move |shared| {
            assert!(
                shared.error.is_none(),
                "SequencePoll::Pending cannot be followed by an error"
            );
        });
    }

    /// Call the given Lua function with arguments / returns starting at `bottom` in the Stack.
    pub async fn call(
        &mut self,
        func: &LocalFunction<'seq>,
        bottom: usize,
    ) -> Result<(), LocalError<'seq>> {
        visit_shared(move |shared| {
            shared.set_next_op(SeqOp::Call {
                function: func.fetch(shared.locals),
                bottom,
            });
        });
        wait_once().await;
        visit_shared(move |shared| {
            if let Some(err) = shared.error.take() {
                Err(Local::stash(&shared.ctx, shared.locals, err))
            } else {
                Ok(())
            }
        })
    }

    /// Yield to the calling code (or to `to_thread`) values starting at `bottom` in the stack. When
    /// this `Sequence` is resumed, resume arguments will be placed at `bottom` in the stack.
    pub async fn yield_to(
        &mut self,
        to_thread: Option<&LocalThread<'seq>>,
        bottom: usize,
    ) -> Result<(), LocalError<'seq>> {
        visit_shared(move |shared| {
            shared.set_next_op(SeqOp::Yield {
                to_thread: to_thread.map(|t| t.fetch(shared.locals)),
                bottom,
            });
        });
        wait_once().await;
        visit_shared(move |shared| {
            if let Some(err) = shared.error.take() {
                Err(Local::stash(&shared.ctx, shared.locals, err))
            } else {
                Ok(())
            }
        })
    }

    /// Resume `thread` with arguments starting at `bottom` in the stack. When the thread completes,
    /// return values will be placed at `bottom` in the stack.
    pub async fn resume(
        &mut self,
        thread: &LocalThread<'seq>,
        bottom: usize,
    ) -> Result<(), LocalError<'seq>> {
        visit_shared(move |shared| {
            shared.set_next_op(SeqOp::Resume {
                thread: thread.fetch(shared.locals),
                bottom,
            });
        });
        wait_once().await;
        visit_shared(move |shared| {
            if let Some(err) = shared.error.take() {
                Err(Local::stash(&shared.ctx, shared.locals, err))
            } else {
                Ok(())
            }
        })
    }
}

/// A collection of stashed values that are local to a specific [`AsyncSequence`].
///
/// [`Local`] values are branded by `'seq` and cannot escape their parent `AsyncSequence` and are
/// (for the purposes of garbage collection) considered *owned* by the parent `AsyncSequence`.
/// Because of this, they correctly mimic what we could do if async blocks themselves could be
/// traced, and so can't lead to uncollectable cycles with their parent.
pub struct Locals<'seq, 'gc> {
    locals: DynamicRootSet<'gc>,
    _invariant: Invariant<'seq>,
}

impl<'seq, 'gc> Locals<'seq, 'gc> {
    /// "Stash" a garbage collected value and return a handle branded with `'seq` that can be stored
    /// in the parent sequence async block.
    pub fn stash<S: Stashable<'gc>>(&self, mc: &Mutation<'gc>, s: S) -> Local<'seq, S::Stashed> {
        Local::stash(mc, self.locals, s)
    }

    /// "Fetch" the real garbage collected value for a handle that has been returned from
    /// [`Locals::stash`].
    pub fn fetch<F: Fetchable<'gc>>(&self, local: &Local<'seq, F>) -> F::Fetched {
        local.fetch(self.locals)
    }
}

enum SeqOp<'gc> {
    Pending,
    Call {
        function: Function<'gc>,
        bottom: usize,
    },
    Yield {
        to_thread: Option<Thread<'gc>>,
        bottom: usize,
    },
    Resume {
        thread: Thread<'gc>,
        bottom: usize,
    },
}

enum SeqReturn<'gc> {
    Return,
    TailCall { function: Function<'gc> },
    TailYield { to_thread: Option<Thread<'gc>> },
    TailResume { thread: Thread<'gc> },
}

#[derive(Collect)]
#[collect(no_drop)]
enum SeqFut<'gc> {
    Create {
        root: Box<dyn Collect + 'gc>,
        #[collect(require_static)]
        create: Box<dyn for<'seq> FnOnce(*mut (), SequenceState<'seq>) -> SeqFuture<'seq>>,
    },
    Run(
        #[collect(require_static)]
        Pin<Box<dyn Future<Output = Result<SequenceReturn<'static>, LocalError<'static>>>>>,
    ),
    Empty,
}

impl<'gc> SeqFut<'gc> {
    fn new<R, F>(root: R, create: F) -> Self
    where
        R: Collect + 'gc,
        F: for<'seq> FnOnce(R, SequenceState<'seq>) -> SeqFuture<'seq> + 'static,
    {
        Self::Create {
            root: Box::new(root),
            create: Box::new(move |rptr, seq| {
                // SAFETY: The pointer is created by `SeqFut::poll` from the `root` field, which is
                // always a type-erased `R`.
                let root = unsafe { Box::from_raw(rptr as *mut R) };
                create(*root, seq)
            }),
        }
    }
}

impl<'gc> SeqFut<'gc> {
    fn poll(&mut self, shared: &mut Shared<'gc, '_>) -> Poll<Result<SeqReturn<'gc>, Error<'gc>>> {
        let locals = shared.locals;
        with_shared(shared, || {
            match mem::replace(self, SeqFut::Empty) {
                SeqFut::Create { root, create } => {
                    *self = Self::Run(Box::into_pin(create(
                        Box::into_raw(root) as *mut (),
                        SequenceState {
                            _invariant: PhantomData,
                        },
                    )));
                }
                other => *self = other,
            }

            let SeqFut::Run(f) = self else { unreachable!() };
            f.as_mut()
                .poll(&mut task::Context::from_waker(&noop_waker()))
                .map(|r| match r {
                    Ok(seq_ret) => Ok(match seq_ret {
                        SequenceReturn::Return => SeqReturn::Return,
                        SequenceReturn::Call(function) => SeqReturn::TailCall {
                            function: function.fetch(locals),
                        },
                        SequenceReturn::Yield(to_thread) => SeqReturn::TailYield {
                            to_thread: to_thread.map(|t| t.fetch(locals)),
                        },
                        SequenceReturn::Resume(thread) => SeqReturn::TailResume {
                            thread: thread.fetch(locals),
                        },
                    }),
                    Err(err) => Err(err.fetch(locals)),
                })
        })
    }
}

// Invariant type that is also !Send and !Sync
type Invariant<'a> = PhantomData<*const Cell<&'a ()>>;

struct Shared<'gc, 'a> {
    locals: DynamicRootSet<'gc>,
    ctx: Context<'gc>,
    exec: Execution<'gc, 'a>,
    stack: Stack<'gc, 'a>,
    error: Option<Error<'gc>>,
    next_op: &'a mut Option<SeqOp<'gc>>,
}

impl<'gc, 'a> Shared<'gc, 'a> {
    fn set_next_op(&mut self, op: SeqOp<'gc>) {
        assert!(
            self.next_op.is_none(),
            "`SequenceState` async method not `await`ed"
        );
        *self.next_op = Some(op);
    }
}

thread_local! {
    static SHARED: Cell<*mut Shared<'static, 'static>> = const { Cell::new(ptr::null_mut()) };
}

fn with_shared<'gc, 'a, R>(shared: &mut Shared<'gc, 'a>, f: impl FnOnce() -> R) -> R {
    // SAFETY: We are erasing the lifetimes of the `Shared` thread local.
    //
    // We know this is sound because the only way we *access* the `Shared` local is through
    // `visit_shared`, which takes a callback which must work for *any* lifetimes 'gc and 'a. In
    // addition, We know the real lifetimes of `Shared` are valid for the body of this function,
    // and this function is the only thing that sets the thread local and it is unset before the
    // function exits using drop guards.
    unsafe {
        SHARED.set(mem::transmute::<
            *mut Shared<'_, '_>,
            *mut Shared<'static, 'static>,
        >(shared));
    }

    struct Guard;

    impl Drop for Guard {
        fn drop(&mut self) {
            SHARED.set(ptr::null_mut());
        }
    }

    let _guard = Guard;

    f()
}

fn visit_shared<R>(f: impl for<'gc, 'a> FnOnce(&'a mut Shared<'gc, 'a>) -> R) -> R {
    // SAFETY: This function must work for any lifetimes 'gc and 'a, so this is sound as long as the
    // call occurs within the callback given to `with_shared` (and this is guarded by setting the
    // SHARED ptr to null outside of `with_shared`). See the safety note in `with_shared`.
    unsafe {
        let shared =
            mem::transmute::<*mut Shared<'static, 'static>, *mut Shared<'_, '_>>(SHARED.get());
        assert!(!shared.is_null(), "AsyncSequence SHARED value unset");
        f(&mut *shared)
    }
}

fn noop_waker() -> Waker {
    const NOOP_RAW_WAKER: RawWaker = {
        const VTABLE: RawWakerVTable =
            RawWakerVTable::new(|_| NOOP_RAW_WAKER, |_| {}, |_| {}, |_| {});
        RawWaker::new(ptr::null(), &VTABLE)
    };

    // SAFETY: NOOP_RAW_WAKER VTable is trivial.
    unsafe { Waker::from_raw(NOOP_RAW_WAKER) }
}

async fn wait_once() {
    let mut done = false;
    poll_fn(|_| {
        if done {
            Poll::Ready(())
        } else {
            done = true;
            Poll::Pending
        }
    })
    .await;
}
