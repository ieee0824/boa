//! Boa's API to create and customize `ECMAScript` jobs and job queues.
//!
//! [`Job`] is an ECMAScript [Job], or a closure that runs an `ECMAScript` computation when
//! there's no other computation running. The module defines several type of jobs:
//! - [`PromiseJob`] for Promise related jobs.
//! - [`TimeoutJob`] for jobs that run after a certain amount of time.
//! - [`NativeAsyncJob`] for jobs that support [`Future`].
//! - [`NativeJob`] for generic jobs that aren't related to Promises.
//!
//! [`JobCallback`] is an ECMAScript [`JobCallback`] record, containing an `ECMAScript` function
//! that is executed when a promise is either fulfilled or rejected.
//!
//! [`JobExecutor`] is a trait encompassing the required functionality for a job executor; this allows
//! implementing custom event loops, custom handling of Jobs or other fun things.
//! This trait is also accompanied by two implementors of the trait:
//! - [`IdleJobExecutor`], which is an executor that does nothing, and the default executor if no executor is
//!   provided. Useful for hosts that want to disable promises.
//! - [`SimpleJobExecutor`], which is a simple FIFO queue that runs all jobs to completion, bailing
//!   on the first error encountered. This simple executor will block on any async job queued.
//!
//! ## [`Trace`]?
//!
//! Most of the types defined in this module don't implement `Trace`. This is because most jobs can only
//! be run once, and putting a `JobExecutor` on a garbage collected object is not allowed.
//!
//! In addition to that, not implementing `Trace` makes it so that the garbage collector can consider
//! any captured variables inside jobs as roots, since you cannot store jobs within a [`Gc`].
//!
//! [Job]: https://tc39.es/ecma262/#sec-jobs
//! [JobCallback]: https://tc39.es/ecma262/#sec-jobcallback-records
//! [`Gc`]: boa_gc::Gc

use crate::context::time::{JsDuration, JsInstant};
use crate::sys::time;
use crate::{
    Context, JsResult, JsValue,
    object::{JsFunction, JsFunctionEdge, NativeObject},
    realm::Realm,
};
use boa_gc::{Finalize, Trace};
use futures_concurrency::future::FutureGroup;
use futures_lite::{StreamExt, future};
use std::any::Any;
use std::cell::Cell;
use std::collections::BTreeMap;
use std::mem;
use std::rc::Rc;
use std::{
    cell::{Ref, RefCell, RefMut},
    collections::VecDeque,
    fmt::Debug,
    future::Future,
    pin::Pin,
};

/// An ECMAScript [Job Abstract Closure].
///
/// This is basically a synchronous task that needs to be run to progress [`Promise`] objects,
/// or unblock threads waiting on [`Atomics.waitAsync`].
///
/// [Job]: https://tc39.es/ecma262/#sec-jobs
/// [`Promise`]: https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/Promise
/// [`Atomics.waitAsync`]: https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/Atomics/waitAsync
pub struct NativeJob {
    #[allow(clippy::type_complexity)]
    f: Box<dyn FnOnce(&mut Context) -> JsResult<JsValue>>,
    realm: Option<Realm>,
}

impl Debug for NativeJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeJob").finish_non_exhaustive()
    }
}

impl NativeJob {
    /// Creates a new `NativeJob` from a closure.
    pub fn new<F>(f: F) -> Self
    where
        F: FnOnce(&mut Context) -> JsResult<JsValue> + 'static,
    {
        Self {
            f: Box::new(f),
            realm: None,
        }
    }

    /// Creates a new `NativeJob` from a closure and an execution realm.
    pub fn with_realm<F>(f: F, realm: Realm) -> Self
    where
        F: FnOnce(&mut Context) -> JsResult<JsValue> + 'static,
    {
        Self {
            f: Box::new(f),
            realm: Some(realm),
        }
    }

    /// Gets a reference to the execution realm of the job.
    #[must_use]
    pub const fn realm(&self) -> Option<&Realm> {
        self.realm.as_ref()
    }

    /// Calls the native job with the specified [`Context`].
    ///
    /// # Note
    ///
    /// If the native job has an execution realm defined, this sets the running execution
    /// context to the realm's before calling the inner closure, and resets it after execution.
    pub fn call(self, context: &mut Context) -> JsResult<JsValue> {
        // If realm is not null, each time job is invoked the implementation must perform
        // implementation-defined steps such that execution is prepared to evaluate ECMAScript
        // code at the time of job's invocation.
        if let Some(realm) = self.realm {
            let old_realm = context.enter_realm(realm);

            // Let scriptOrModule be GetActiveScriptOrModule() at the time HostEnqueuePromiseJob is
            // invoked. If realm is not null, each time job is invoked the implementation must
            // perform implementation-defined steps such that scriptOrModule is the active script or
            // module at the time of job's invocation.
            let result = (self.f)(context);

            context.enter_realm(old_realm);

            result
        } else {
            (self.f)(context)
        }
    }
}

/// Flag that can only be set once.
#[derive(Debug, Clone)]
pub(crate) struct OnceFlag(Rc<Cell<bool>>);

impl OnceFlag {
    /// Creates a new `OnceFlag`.
    pub(crate) fn new() -> Self {
        Self(Rc::new(Cell::new(false)))
    }

    /// Sets this `OnceFlag` to `true`.
    pub(crate) fn set(&self) {
        self.0.set(true);
    }

    /// Returns `true` if this `OnceFlag` has been set, or `false` otherwise.
    pub(crate) fn is_set(&self) -> bool {
        self.0.get()
    }
}

/// An ECMAScript [Job] that runs after a certain amount of time.
///
/// This represents the [HostEnqueueTimeoutJob] operation from the specification.
///
/// [HostEnqueueTimeoutJob]: https://tc39.es/ecma262/#sec-hostenqueuetimeoutjob
#[derive(Debug)]
pub struct TimeoutJob {
    /// The distance in milliseconds in the future when the job should run.
    /// This will be added to the current time when the job is enqueued.
    timeout: JsDuration,
    /// The job to run after the time has passed.
    job: NativeJob,
    /// Signals if the timeout job was cancelled.
    cancelled: OnceFlag,
    /// Signals that this job is recurring. A recurring job shouldn't be
    /// awaited for when considering whether a run of the event loop is
    /// done.
    recurring: bool,
}

impl TimeoutJob {
    /// Create a new `TimeoutJob` with a timeout and a job.
    #[must_use]
    pub fn new(job: NativeJob, timeout_in_millis: u64) -> Self {
        Self {
            timeout: JsDuration::from_millis(timeout_in_millis),
            job,
            cancelled: OnceFlag::new(),
            recurring: false,
        }
    }

    /// Create a new `TimeoutJob` that is marked as recurring.
    #[must_use]
    pub fn recurring(job: NativeJob, timeout_in_millis: u64) -> Self {
        Self {
            timeout: JsDuration::from_millis(timeout_in_millis),
            job,
            cancelled: OnceFlag::new(),
            recurring: true,
        }
    }

    /// Creates a new `TimeoutJob` from a closure and a timeout as [`std::time::Duration`].
    #[must_use]
    pub fn from_duration<F>(f: F, timeout: impl Into<JsDuration>) -> Self
    where
        F: FnOnce(&mut Context) -> JsResult<JsValue> + 'static,
    {
        Self::new(NativeJob::new(f), timeout.into().as_millis())
    }

    /// Creates a new `TimeoutJob` from a closure, a timeout, and an execution realm.
    #[must_use]
    pub fn with_realm<F>(f: F, realm: Realm, timeout: time::Duration) -> Self
    where
        F: FnOnce(&mut Context) -> JsResult<JsValue> + 'static,
    {
        Self::new(NativeJob::with_realm(f, realm), timeout.as_millis() as u64)
    }

    /// Calls the native job with the specified [`Context`].
    ///
    /// # Note
    ///
    /// If the native job has an execution realm defined, this sets the running execution
    /// context to the realm's before calling the inner closure, and resets it after execution.
    pub fn call(self, context: &mut Context) -> JsResult<JsValue> {
        self.job.call(context)
    }

    /// Returns the timeout value in milliseconds since epoch.
    #[inline]
    #[must_use]
    pub fn timeout(&self) -> JsDuration {
        self.timeout
    }

    /// Returns `true` if the timeout was cancelled, and its execution can be skipped.
    #[inline]
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.is_set()
    }

    /// Returns the `OnceFlag` to cancel this timeout job.
    pub(crate) fn cancelled_flag(&self) -> OnceFlag {
        self.cancelled.clone()
    }

    /// Returns `true` if the job is recurring (meaning it happens regularly).
    #[must_use]
    pub fn is_recurring(&self) -> bool {
        self.recurring
    }
}

/// An ECMAScript Generic [Job].
///
/// This represents the [HostEnqueueGenericJob] operation from the specification, which
/// enqueues a job that is just like a [`PromiseJob`], but unconstrained in relation
/// to priority and ordering.
///
/// [HostEnqueueGenericJob]: https://tc39.es/ecma262/#sec-hostenqueuegenericjob
pub struct GenericJob(NativeJob);

impl Debug for GenericJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GenericJob").finish_non_exhaustive()
    }
}

impl GenericJob {
    /// Creates a new `GenericJob` from a closure and an execution realm.
    pub fn new<F>(f: F, realm: Realm) -> Self
    where
        F: FnOnce(&mut Context) -> JsResult<JsValue> + 'static,
    {
        Self(NativeJob::with_realm(f, realm))
    }

    /// Gets a reference to the execution realm of the job.
    #[must_use]
    pub const fn realm(&self) -> &Realm {
        self.0
            .realm
            .as_ref()
            .expect("all generic jobs must have an execution realm")
    }

    /// Calls the `GenericJob` with the specified [`Context`], setting the execution
    /// context to the job's realm before calling the inner closure, and resets it after execution.
    pub fn call(self, context: &mut Context) -> JsResult<JsValue> {
        self.0.call(context)
    }
}

/// The [`Future`] job returned by a [`NativeAsyncJob`] operation.
pub type BoxedFuture<'a> = Pin<Box<dyn Future<Output = JsResult<JsValue>> + 'a>>;

/// Mutable execution-context storage shared by asynchronous jobs.
pub struct AsyncContext<'a> {
    context: RefCell<Option<&'a mut Context>>,
    previous_async_jobs_enabled: bool,
}

impl Debug for AsyncContext<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AsyncContext").finish_non_exhaustive()
    }
}

impl<'a> AsyncContext<'a> {
    /// Creates async job context storage and enables asynchronous suspension until it is dropped.
    ///
    /// Dropping the storage restores the execution context's previous suspension mode.
    pub fn new(context: &'a mut Context) -> Self {
        Self::with_async_jobs_enabled(context, true)
    }

    /// Creates async job context storage with asynchronous suspension disabled until it is dropped.
    ///
    /// This is intended for synchronous executor entry points. Dropping the storage restores the
    /// execution context's previous suspension mode.
    pub fn new_sync(context: &'a mut Context) -> Self {
        Self::with_async_jobs_enabled(context, false)
    }

    fn with_async_jobs_enabled(context: &'a mut Context, async_jobs_enabled: bool) -> Self {
        let previous_async_jobs_enabled = context.async_jobs_enabled;
        context.async_jobs_enabled = async_jobs_enabled;

        Self {
            context: RefCell::new(Some(context)),
            previous_async_jobs_enabled,
        }
    }

    /// Immutably borrows the execution context.
    pub fn borrow(&self) -> Ref<'_, Context> {
        Ref::map(self.context.borrow(), |context| {
            context.as_deref().expect("execution context is in use")
        })
    }

    /// Mutably borrows the execution context until the returned guard is dropped.
    pub fn borrow_mut(&self) -> RefMut<'_, Context> {
        RefMut::map(self.context.borrow_mut(), |context| {
            context.as_deref_mut().expect("execution context is in use")
        })
    }

    pub(crate) fn take(&self) -> ContextLease<'_, 'a> {
        let context = self
            .context
            .borrow_mut()
            .take()
            .expect("execution context is already in use");
        ContextLease {
            owner: self,
            context: Some(context),
        }
    }
}

pub(crate) struct ContextLease<'a, 'context> {
    owner: &'a AsyncContext<'context>,
    context: Option<&'context mut Context>,
}

impl std::ops::Deref for ContextLease<'_, '_> {
    type Target = Context;

    fn deref(&self) -> &Self::Target {
        self.context.as_deref().expect("context lease is empty")
    }
}

impl std::ops::DerefMut for ContextLease<'_, '_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.context.as_deref_mut().expect("context lease is empty")
    }
}

impl Drop for ContextLease<'_, '_> {
    fn drop(&mut self) {
        self.owner.context.replace(self.context.take());
    }
}

impl Drop for AsyncContext<'_> {
    fn drop(&mut self) {
        if let Some(context) = self.context.get_mut().as_deref_mut() {
            context.async_jobs_enabled = self.previous_async_jobs_enabled;
        }
    }
}

/// The boxed future returned by [`JobExecutor::run_jobs_async`].
pub type JobExecutorFuture<'a> = Pin<Box<dyn Future<Output = JsResult<()>> + 'a>>;

/// An ECMAScript [Job] that can be run asynchronously.
///
/// This is an additional type of job that is not defined by the specification, enabling running `Future` tasks
/// created by ECMAScript code in an easier way.
#[allow(clippy::type_complexity)]
pub struct NativeAsyncJob {
    f: Box<dyn for<'a> FnOnce(&'a AsyncContext<'_>) -> BoxedFuture<'a>>,
    realm: Option<Realm>,
    exclusive: bool,
}

impl Debug for NativeAsyncJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeAsyncJob")
            .field("f", &"Closure")
            .finish()
    }
}

impl NativeAsyncJob {
    /// Creates a new `NativeAsyncJob` from an async closure.
    pub fn new<F>(f: F) -> Self
    where
        F: AsyncFnOnce(&AsyncContext<'_>) -> JsResult<JsValue> + 'static,
    {
        Self {
            f: Box::new(move |ctx| Box::pin(async move { f(ctx).await })),
            realm: None,
            exclusive: false,
        }
    }

    /// Creates a new `NativeAsyncJob` from an async closure and an execution realm.
    pub fn with_realm<F>(f: F, realm: Realm) -> Self
    where
        F: AsyncFnOnce(&AsyncContext<'_>) -> JsResult<JsValue> + 'static,
    {
        Self {
            f: Box::new(move |ctx| Box::pin(async move { f(ctx).await })),
            realm: Some(realm),
            exclusive: false,
        }
    }

    /// Creates an async job which exclusively owns the context while pending.
    pub(crate) fn new_exclusive<F>(f: F) -> Self
    where
        F: AsyncFnOnce(&AsyncContext<'_>) -> JsResult<JsValue> + 'static,
    {
        Self {
            f: Box::new(move |ctx| Box::pin(async move { f(ctx).await })),
            realm: None,
            exclusive: true,
        }
    }

    /// Returns whether this async job requires exclusive access to the context while pending.
    #[must_use]
    pub const fn is_exclusive(&self) -> bool {
        self.exclusive
    }

    /// Gets a reference to the execution realm of the job.
    #[must_use]
    pub const fn realm(&self) -> Option<&Realm> {
        self.realm.as_ref()
    }

    /// Calls the native async job with the specified [`Context`].
    ///
    /// # Note
    ///
    /// If the native async job has an execution realm defined, this sets the running execution
    /// context to the realm's before calling the inner closure, and resets it after execution.
    pub fn call<'a, 'b>(
        self,
        context: &'a AsyncContext<'b>,
        // We can make our users assume `Unpin` because `self.f` is already boxed, so we shouldn't
        // need pin at all.
    ) -> impl Future<Output = JsResult<JsValue>> + Unpin + use<'a, 'b> {
        // If realm is not null, each time job is invoked the implementation must perform
        // implementation-defined steps such that execution is prepared to evaluate ECMAScript
        // code at the time of job's invocation.
        let realm = self.realm;

        let mut future = if let Some(realm) = &realm {
            let old_realm = context.borrow_mut().enter_realm(realm.clone());

            // Let scriptOrModule be GetActiveScriptOrModule() at the time HostEnqueuePromiseJob is
            // invoked. If realm is not null, each time job is invoked the implementation must
            // perform implementation-defined steps such that scriptOrModule is the active script or
            // module at the time of job's invocation.
            let result = (self.f)(context);

            context.borrow_mut().enter_realm(old_realm);
            result
        } else {
            (self.f)(context)
        };

        std::future::poll_fn(move |cx| {
            // We need to do the same dance again since the inner code could assume we're still
            // on the same realm.
            if let Some(realm) = &realm {
                let old_realm = context.borrow_mut().enter_realm(realm.clone());

                let poll_result = future.as_mut().poll(cx);

                context.borrow_mut().enter_realm(old_realm);
                poll_result
            } else {
                future.as_mut().poll(cx)
            }
        })
    }
}

/// An ECMAScript [Job Abstract Closure] executing code related to
/// [`Promise`](https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/Promise) objects.
///
/// This represents the [`HostEnqueuePromiseJob`] operation from the specification.
///
/// ### [Requirements]
///
/// - If realm is not null, each time job is invoked the implementation must perform implementation-defined
///   steps such that execution is prepared to evaluate ECMAScript code at the time of job's invocation.
/// - Let `scriptOrModule` be [`GetActiveScriptOrModule()`] at the time `HostEnqueuePromiseJob` is invoked.
///   If realm is not null, each time job is invoked the implementation must perform implementation-defined steps
///   such that `scriptOrModule` is the active script or module at the time of job's invocation.
/// - Jobs must run in the same order as the `HostEnqueuePromiseJob` invocations that scheduled them.
///
/// Of all the requirements, Boa guarantees the first two by its internal implementation of `NativeJob`, meaning
/// implementations of [`JobExecutor`] must only guarantee that jobs are run in the same order as they're enqueued.
///
/// [`HostEnqueuePromiseJob`]: https://tc39.es/ecma262/#sec-hostenqueuepromisejob
/// [Job Abstract Closure]: https://tc39.es/ecma262/#sec-jobs
/// [Requirements]: https://tc39.es/ecma262/multipage/executable-code-and-execution-contexts.html#sec-hostenqueuepromisejob
/// [`GetActiveScriptOrModule()`]: https://tc39.es/ecma262/multipage/executable-code-and-execution-contexts.html#sec-getactivescriptormodule
enum PromiseJobInner {
    Sync(NativeJob),
    Async { job: NativeAsyncJob, realm: Realm },
}

/// An ECMAScript [Job Abstract Closure] executing code related to
/// [`Promise`](https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/Promise) objects.
pub struct PromiseJob(PromiseJobInner);

impl Debug for PromiseJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PromiseJob").finish_non_exhaustive()
    }
}

impl PromiseJob {
    /// Creates a new `PromiseJob` from a closure.
    pub fn new<F>(f: F) -> Self
    where
        F: FnOnce(&mut Context) -> JsResult<JsValue> + 'static,
    {
        Self(PromiseJobInner::Sync(NativeJob::new(f)))
    }

    /// Creates a new `PromiseJob` from a closure and an execution realm.
    pub fn with_realm<F>(f: F, realm: Realm) -> Self
    where
        F: FnOnce(&mut Context) -> JsResult<JsValue> + 'static,
    {
        Self(PromiseJobInner::Sync(NativeJob::with_realm(f, realm)))
    }

    /// Creates an asynchronous `PromiseJob` from a closure and an execution realm.
    pub fn with_realm_async<F>(f: F, realm: Realm) -> Self
    where
        F: AsyncFnOnce(&AsyncContext<'_>) -> JsResult<JsValue> + 'static,
    {
        let job_realm = realm.clone();
        let job = NativeAsyncJob::new(async move |context| {
            let old_realm = context.borrow_mut().enter_realm(job_realm);
            let result = f(context).await;
            context.borrow_mut().enter_realm(old_realm);
            result
        });
        Self(PromiseJobInner::Async { job, realm })
    }

    /// Gets a reference to the execution realm of the `PromiseJob`.
    #[must_use]
    pub const fn realm(&self) -> Option<&Realm> {
        match &self.0 {
            PromiseJobInner::Sync(job) => job.realm(),
            PromiseJobInner::Async { realm, .. } => Some(realm),
        }
    }

    /// Calls the `PromiseJob` with the specified [`Context`].
    ///
    /// # Note
    ///
    /// If the job has an execution realm defined, this sets the running execution
    /// context to the realm's before calling the inner closure, and resets it after execution.
    pub fn call(self, context: &mut Context) -> JsResult<JsValue> {
        match self.0 {
            PromiseJobInner::Sync(job) => job.call(context),
            PromiseJobInner::Async { job, .. } => {
                future::block_on(job.call(&AsyncContext::new_sync(context)))
            }
        }
    }

    /// Calls the promise job asynchronously without blocking the host executor.
    pub fn call_async<'a>(self, context: &'a AsyncContext<'_>) -> BoxedFuture<'a> {
        match self.0 {
            PromiseJobInner::Sync(job) => {
                Box::pin(async move { job.call(&mut context.borrow_mut()) })
            }
            PromiseJobInner::Async { job, .. } => Box::pin(job.call(context)),
        }
    }
}

/// [`JobCallback`][spec] records.
///
/// [spec]: https://tc39.es/ecma262/#sec-jobcallback-records
#[derive(Trace, Finalize)]
pub struct JobCallback {
    callback: JsFunctionEdge,
    host_defined: Box<dyn NativeObject>,
}

impl Debug for JobCallback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobCallback")
            .field("callback", &self.callback)
            .field("host_defined", &"dyn NativeObject")
            .finish()
    }
}

impl JobCallback {
    /// Creates a new `JobCallback`.
    #[inline]
    pub fn new<T: NativeObject>(callback: JsFunction, host_defined: T) -> Self {
        Self {
            callback: callback.into_edge(),
            host_defined: Box::new(host_defined),
        }
    }

    /// Gets the inner callback of the job.
    #[inline]
    #[must_use]
    pub fn callback(&self) -> JsFunction {
        self.callback.root()
    }

    /// Gets a reference to the host defined additional field as an [`NativeObject`] trait object.
    #[inline]
    #[must_use]
    pub fn host_defined(&self) -> &dyn NativeObject {
        &*self.host_defined
    }

    /// Gets a mutable reference to the host defined additional field as an [`NativeObject`] trait object.
    #[inline]
    pub fn host_defined_mut(&mut self) -> &mut dyn NativeObject {
        &mut *self.host_defined
    }
}

/// A job that needs to be handled by a [`JobExecutor`].
///
/// # Requirements
///
/// The specification defines many types of jobs, but all of them must adhere to a set of requirements:
///
/// - At some future point in time, when there is no running execution context and the execution
///   context stack is empty, the implementation must:
///     - Perform any host-defined preparation steps.
///     - Invoke the Job Abstract Closure.
///     - Perform any host-defined cleanup steps, after which the execution context stack must be empty.
/// - Only one Job may be actively undergoing evaluation at any point in time.
/// - Once evaluation of a Job starts, it must run to completion before evaluation of any other Job starts.
/// - The Abstract Closure must return a normal completion, implementing its own handling of errors.
///
/// Boa is a little bit flexible on the last requirement, since it allows jobs to return either
/// values or errors, but the rest of the requirements must be followed for all conformant implementations.
///
/// Additionally, each job type can have additional requirements that must also be followed in addition
/// to the previous ones.
#[non_exhaustive]
#[derive(Debug)]
pub enum Job {
    /// A `Promise`-related job.
    ///
    /// See [`PromiseJob`] for more information.
    PromiseJob(PromiseJob),
    /// A [`Future`]-related job.
    ///
    /// See [`NativeAsyncJob`] for more information.
    AsyncJob(NativeAsyncJob),
    /// A generic job that is to be executed after a number of milliseconds.
    ///
    /// See [`TimeoutJob`] for more information.
    TimeoutJob(TimeoutJob),
    /// A generic job.
    ///
    /// See [`GenericJob`] for more information.
    GenericJob(GenericJob),
}

impl From<NativeAsyncJob> for Job {
    fn from(native_async_job: NativeAsyncJob) -> Self {
        Job::AsyncJob(native_async_job)
    }
}

impl From<PromiseJob> for Job {
    fn from(promise_job: PromiseJob) -> Self {
        Job::PromiseJob(promise_job)
    }
}

impl From<TimeoutJob> for Job {
    fn from(job: TimeoutJob) -> Self {
        Job::TimeoutJob(job)
    }
}

impl From<GenericJob> for Job {
    fn from(job: GenericJob) -> Self {
        Job::GenericJob(job)
    }
}

/// An executor of `ECMAscript` [Jobs].
///
/// This is the main API that allows creating custom event loops.
///
/// [Jobs]: https://tc39.es/ecma262/#sec-jobs
pub trait JobExecutor: Any {
    /// Enqueues a `Job` on the executor.
    ///
    /// This method combines all the host-defined job enqueueing operations into a single method.
    /// See the [spec] for more information on the requirements that each operation must follow.
    ///
    /// [spec]: https://tc39.es/ecma262/#sec-jobs
    fn enqueue_job(self: Rc<Self>, job: Job, context: &mut Context);

    /// Runs all jobs in the executor.
    fn run_jobs(self: Rc<Self>, context: &mut Context) -> JsResult<()>;

    /// Asynchronously runs all jobs in the executor.
    ///
    /// By default forwards to [`JobExecutor::run_jobs`]. Implementors using async should override this
    /// with a proper algorithm to run jobs asynchronously.
    fn run_jobs_async<'a>(self: Rc<Self>, context: &'a AsyncContext<'_>) -> JobExecutorFuture<'a> {
        Box::pin(async move { self.run_jobs(&mut context.borrow_mut()) })
    }
}

/// A job executor that does nothing.
///
/// This executor is mostly useful if you want to disable the promise capabilities of the engine. This
/// can be done by passing it to the [`ContextBuilder`]:
///
/// ```
/// use boa_engine::{
///     context::ContextBuilder,
///     job::{IdleJobExecutor, JobExecutor},
/// };
/// use std::rc::Rc;
///
/// let executor = Rc::new(IdleJobExecutor);
/// let context = ContextBuilder::new().job_executor(executor).build();
/// ```
///
/// [`ContextBuilder`]: crate::context::ContextBuilder
#[derive(Debug, Clone, Copy)]
pub struct IdleJobExecutor;

impl JobExecutor for IdleJobExecutor {
    fn enqueue_job(self: Rc<Self>, _: Job, _: &mut Context) {}

    fn run_jobs(self: Rc<Self>, _: &mut Context) -> JsResult<()> {
        Ok(())
    }
}

/// A simple FIFO executor that bails on the first error.
///
/// This is the default job executor for the [`Context`], but it is mostly pretty limited
/// for a custom event loop.
///
/// To disable running promise jobs on the engine, see [`IdleJobExecutor`].
#[allow(clippy::struct_field_names)]
#[derive(Default)]
pub struct SimpleJobExecutor {
    promise_jobs: RefCell<VecDeque<PromiseJob>>,
    async_jobs: RefCell<VecDeque<NativeAsyncJob>>,
    timeout_jobs: RefCell<BTreeMap<JsInstant, TimeoutJob>>,
    generic_jobs: RefCell<VecDeque<GenericJob>>,
}

impl SimpleJobExecutor {
    fn clear(&self) {
        self.promise_jobs.borrow_mut().clear();
        self.async_jobs.borrow_mut().clear();
        self.timeout_jobs.borrow_mut().clear();
        self.generic_jobs.borrow_mut().clear();
    }
}

impl Debug for SimpleJobExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SimpleJobExecutor").finish_non_exhaustive()
    }
}

impl SimpleJobExecutor {
    /// Creates a new `SimpleJobExecutor`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl JobExecutor for SimpleJobExecutor {
    fn enqueue_job(self: Rc<Self>, job: Job, context: &mut Context) {
        match job {
            Job::PromiseJob(p) => self.promise_jobs.borrow_mut().push_back(p),
            Job::AsyncJob(a) => self.async_jobs.borrow_mut().push_back(a),
            Job::TimeoutJob(t) => {
                let now = context.clock().now();
                self.timeout_jobs.borrow_mut().insert(now + t.timeout(), t);
            }
            Job::GenericJob(g) => self.generic_jobs.borrow_mut().push_back(g),
        }
    }

    fn run_jobs(self: Rc<Self>, context: &mut Context) -> JsResult<()> {
        future::block_on(self.run_jobs_async(&AsyncContext::new_sync(context)))
    }

    fn run_jobs_async<'a>(self: Rc<Self>, context: &'a AsyncContext<'_>) -> JobExecutorFuture<'a> {
        Box::pin(async move {
            let mut group = FutureGroup::new();
            loop {
                let async_jobs = mem::take(&mut *self.async_jobs.borrow_mut());
                for job in async_jobs {
                    if job.is_exclusive() {
                        while let Some(result) = group.next().await {
                            if let Err(error) = result {
                                self.clear();
                                return Err(error);
                            }
                        }
                        if let Err(err) = job.call(context).await {
                            self.clear();
                            return Err(err);
                        }
                    } else {
                        group.insert(job.call(context));
                    }
                }

                // There are no timeout jobs to run IIF there are no jobs to execute right now.
                let no_timeout_jobs_to_run = {
                    let now = context.borrow().clock().now();
                    !self.timeout_jobs.borrow().iter().any(|(t, _)| &now >= t)
                };

                if self.promise_jobs.borrow().is_empty()
                    && self.async_jobs.borrow().is_empty()
                    && self.generic_jobs.borrow().is_empty()
                    && no_timeout_jobs_to_run
                    && group.is_empty()
                {
                    break;
                }

                if let Some(Err(err)) = future::poll_once(group.next()).await.flatten() {
                    self.clear();
                    return Err(err);
                }

                {
                    let now = context.borrow().clock().now();
                    let mut timeouts_borrow = self.timeout_jobs.borrow_mut();
                    let mut jobs_to_keep = timeouts_borrow.split_off(&now);
                    jobs_to_keep.retain(|_, job| !job.is_cancelled());
                    let jobs_to_run = mem::replace(&mut *timeouts_borrow, jobs_to_keep);
                    drop(timeouts_borrow);

                    for job in jobs_to_run.into_values() {
                        if let Err(err) = job.call(&mut context.borrow_mut()) {
                            self.clear();
                            return Err(err);
                        }
                    }
                }

                let jobs = mem::take(&mut *self.promise_jobs.borrow_mut());
                for job in jobs {
                    if let Err(err) = job.call_async(context).await {
                        self.clear();
                        return Err(err);
                    }
                }

                let jobs = mem::take(&mut *self.generic_jobs.borrow_mut());
                for job in jobs {
                    if let Err(err) = job.call(&mut context.borrow_mut()) {
                        self.clear();
                        return Err(err);
                    }
                }
                context.borrow_mut().clear_kept_objects();
                future::yield_now().await;
            }

            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusive_async_jobs_wait_for_earlier_jobs_and_block_later_jobs() {
        let executor = Rc::new(SimpleJobExecutor::new());
        let order = Rc::new(RefCell::new(Vec::new()));
        let mut context = Context::default();

        let earlier_order = order.clone();
        executor.clone().enqueue_job(
            NativeAsyncJob::new(async move |_| {
                future::yield_now().await;
                earlier_order.borrow_mut().push(1);
                Ok(JsValue::undefined())
            })
            .into(),
            &mut context,
        );
        let exclusive_order = order.clone();
        executor.clone().enqueue_job(
            NativeAsyncJob::new_exclusive(async move |context| {
                let _context = context.take();
                exclusive_order.borrow_mut().push(2);
                Ok(JsValue::undefined())
            })
            .into(),
            &mut context,
        );
        let later_order = order.clone();
        executor.clone().enqueue_job(
            NativeAsyncJob::new(async move |_| {
                later_order.borrow_mut().push(3);
                Ok(JsValue::undefined())
            })
            .into(),
            &mut context,
        );

        future::block_on(executor.run_jobs_async(&AsyncContext::new(&mut context))).unwrap();

        assert_eq!(*order.borrow(), [1, 2, 3]);
    }
}
