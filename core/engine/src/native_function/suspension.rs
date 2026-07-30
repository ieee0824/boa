//! Suspension of synchronous native calls during asynchronous script evaluation.

use std::{fmt, future::Future, pin::Pin, task};

use boa_gc::{Finalize, Gc, GcRefCell, Trace};

use crate::{JsObject, JsResult, JsValue};

/// A completion handle for a suspended synchronous native call.
///
/// The handle can be cloned and moved into a host event loop. Completing it wakes the
/// asynchronous script evaluation that created it. The completion is accepted exactly once.
#[derive(Clone, Trace, Finalize)]
pub struct NativeCallSuspension {
    inner: Gc<GcRefCell<Inner>>,
}

impl fmt::Debug for NativeCallSuspension {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NativeCallSuspension")
            .finish_non_exhaustive()
    }
}

#[derive(Trace, Finalize)]
struct Inner {
    origin: JsObject,
    placeholder: JsObject,
    result: Option<JsResult<JsValue>>,
    resumed: bool,
    #[unsafe_ignore_trace]
    task: Option<task::Waker>,
}

/// Error returned when a native call suspension is completed more than once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeCallAlreadyResumed;

impl fmt::Display for NativeCallAlreadyResumed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("native call suspension was already resumed")
    }
}

impl std::error::Error for NativeCallAlreadyResumed {}

impl NativeCallSuspension {
    pub(crate) fn new(origin: JsObject, placeholder: JsObject) -> Self {
        Self {
            inner: Gc::new(GcRefCell::new(Inner {
                origin,
                placeholder,
                result: None,
                resumed: false,
                task: None,
            })),
        }
    }

    /// Completes the suspended native call with its return value or thrown error.
    pub fn resume(&self, result: JsResult<JsValue>) -> Result<(), NativeCallAlreadyResumed> {
        let task = {
            let mut inner = self.inner.borrow_mut();
            if inner.resumed {
                return Err(NativeCallAlreadyResumed);
            }
            inner.resumed = true;
            inner.result = Some(result);
            inner.task.take()
        };

        if let Some(task) = task {
            task.wake();
        }
        Ok(())
    }

    pub(crate) fn try_take_result(&self) -> Option<JsResult<JsValue>> {
        self.inner.borrow_mut().result.take()
    }

    pub(crate) fn placeholder(&self) -> JsObject {
        self.inner.borrow().placeholder.clone()
    }

    pub(crate) fn originated_from(&self, function: &JsObject) -> bool {
        JsObject::equals(&self.inner.borrow().origin, function)
    }
}

impl Future for NativeCallSuspension {
    type Output = JsResult<JsValue>;

    fn poll(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> task::Poll<Self::Output> {
        let mut inner = self.inner.borrow_mut();
        if let Some(result) = inner.result.take() {
            return task::Poll::Ready(result);
        }
        inner.task = Some(cx.waker().clone());
        task::Poll::Pending
    }
}
