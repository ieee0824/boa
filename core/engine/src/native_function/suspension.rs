//! Suspension of synchronous native calls during asynchronous script evaluation.

use std::{fmt, future::Future, pin::Pin, task};

use boa_gc::{Finalize, GcRefCell, Rooted, Trace};

use crate::{JsObject, JsResult, JsValue};

/// A completion handle for a suspended synchronous native call.
///
/// The handle can be cloned and moved into a host event loop. Completing it wakes the
/// asynchronous script evaluation that created it. The completion is accepted exactly once.
#[derive(Clone, Trace, Finalize)]
pub struct NativeCallSuspension {
    #[unsafe_ignore_trace]
    inner: Rooted<GcRefCell<Inner>>,
}

struct NativeCallWait {
    inner: Rooted<GcRefCell<Inner>>,
}

impl fmt::Debug for NativeCallSuspension {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NativeCallSuspension")
            .finish_non_exhaustive()
    }
}

#[derive(Trace, Finalize)]
struct Inner {
    origin: Option<JsObject>,
    placeholder: Option<JsObject>,
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
            inner: Rooted::new(GcRefCell::new(Inner {
                origin: Some(origin),
                placeholder: Some(placeholder),
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

    pub(crate) async fn wait(&self) -> JsResult<JsValue> {
        NativeCallWait {
            inner: self.inner.clone(),
        }
        .await
    }

    pub(crate) fn placeholder(&self) -> JsObject {
        self.inner
            .borrow()
            .placeholder
            .clone()
            .expect("an active suspension must retain its placeholder")
    }

    pub(crate) fn originated_from(&self, function: &JsObject) -> bool {
        self.inner
            .borrow()
            .origin
            .as_ref()
            .is_some_and(|origin| JsObject::equals(origin, function))
    }

    pub(crate) fn release_roots(&self) {
        let mut inner = self.inner.borrow_mut();
        inner.origin = None;
        inner.placeholder = None;
    }

    pub(crate) fn cancel(&self) {
        let mut inner = self.inner.borrow_mut();
        inner.resumed = true;
        inner.result = None;
        inner.task = None;
        inner.origin = None;
        inner.placeholder = None;
    }
}

impl Drop for NativeCallWait {
    fn drop(&mut self) {
        let mut inner = self.inner.borrow_mut();
        inner.resumed = true;
        inner.result = None;
        inner.task = None;
        inner.origin = None;
        inner.placeholder = None;
    }
}

impl Future for NativeCallWait {
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
