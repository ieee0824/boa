use boa_gc::{Finalize, Rooted, Trace};

use crate::{Context, JsResult, JsValue};

trait TraceableNativeCallContinuation: Trace {
    fn call(&self, result: JsResult<JsValue>, context: &mut Context) -> JsResult<JsValue>;
}

#[derive(Trace, Finalize)]
struct Continuation<F, T>
where
    F: Fn(JsResult<JsValue>, &T, &mut Context) -> JsResult<JsValue>,
    T: Trace,
{
    #[unsafe_ignore_trace]
    function: F,
    captures: T,
}

impl<F, T> TraceableNativeCallContinuation for Continuation<F, T>
where
    F: Fn(JsResult<JsValue>, &T, &mut Context) -> JsResult<JsValue>,
    T: Trace,
{
    fn call(&self, result: JsResult<JsValue>, context: &mut Context) -> JsResult<JsValue> {
        (self.function)(result, &self.captures, context)
    }
}

/// A garbage-collected continuation resumed after a native function's JavaScript callback.
#[derive(Clone)]
pub struct NativeCallContinuation {
    inner: Rooted<dyn TraceableNativeCallContinuation>,
}

impl std::fmt::Debug for NativeCallContinuation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativeCallContinuation")
            .finish_non_exhaustive()
    }
}

impl NativeCallContinuation {
    /// Creates a continuation from a `Copy` closure and traceable captures.
    pub fn from_copy_closure_with_captures<F, T>(function: F, captures: T) -> Self
    where
        F: Fn(JsResult<JsValue>, &T, &mut Context) -> JsResult<JsValue> + Copy + 'static,
        T: Trace + 'static,
    {
        // SAFETY: `Copy` prevents the closure from implicitly owning GC-managed captures.
        unsafe { Self::from_closure_with_captures(function, captures) }
    }

    /// Creates a continuation from a closure and traceable captures.
    ///
    /// # Safety
    ///
    /// The closure must not implicitly capture values which require garbage collection tracing.
    pub unsafe fn from_closure_with_captures<F, T>(function: F, captures: T) -> Self
    where
        F: Fn(JsResult<JsValue>, &T, &mut Context) -> JsResult<JsValue> + 'static,
        T: Trace + 'static,
    {
        let pointer = Rooted::into_raw(Rooted::new(Continuation { function, captures }));
        // SAFETY: only coercing the allocation to its traceable trait object.
        unsafe {
            Self {
                inner: Rooted::from_raw(pointer),
            }
        }
    }

    pub(crate) fn call(
        &self,
        result: JsResult<JsValue>,
        context: &mut Context,
    ) -> JsResult<JsValue> {
        self.inner.call(result, context)
    }
}
