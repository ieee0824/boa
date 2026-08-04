//! Boa's implementation of ECMAScript's Scripts.
//!
//! This module contains the [`Script`] type, which represents a [**Script Record**][script].
//!
//! More information:
//!  - [ECMAScript reference][spec]
//!
//! [spec]: https://tc39.es/ecma262/#sec-scripts
//! [script]: https://tc39.es/ecma262/#sec-script-records

use std::path::{Path, PathBuf};

use rustc_hash::FxHashMap;

use boa_gc::{Finalize, GcEdge, GcRefCell, Rooted, Trace};
use boa_parser::{Parser, Source, source::ReadChar};

use crate::{
    Context, HostDefined, JsResult, JsString, JsValue, SpannedSourceText,
    bytecompiler::{ByteCompiler, global_declaration_instantiation_context},
    js_string,
    realm::{Realm, RealmEdge},
    spanned_source_text::SourceText,
    vm::{ActiveRunnable, CallFrame, CallFrameFlags, CodeBlock},
};

/// ECMAScript's [**Script Record**][spec].
///
/// [spec]: https://tc39.es/ecma262/#sec-script-records
#[derive(Clone)]
pub struct Script {
    inner: Rooted<Inner>,
}

/// A script reference stored inside a traced garbage-collected value.
///
/// Use [`Script::to_edge`] before placing a script in a native-function capture,
/// and [`Self::root`] when an external owner is needed again.
#[derive(Clone, Trace, Finalize)]
pub struct ScriptEdge {
    inner: GcEdge<Inner>,
}

struct AsyncScriptFrameGuard<'a> {
    context: &'a mut Context,
    completed: bool,
    native_continuation_depth: usize,
}

impl AsyncScriptFrameGuard<'_> {
    fn complete(mut self) {
        self.context.vm.pop_frame();
        self.context
            .vm
            .native_call_continuations
            .truncate(self.native_continuation_depth);
        self.completed = true;
    }
}

impl Drop for AsyncScriptFrameGuard<'_> {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        if let Some(frame) = self.context.vm.pop_frame() {
            self.context.vm.stack.truncate_to_frame(&frame);
        }
        self.context
            .vm
            .native_call_continuations
            .truncate(self.native_continuation_depth);
    }
}

impl std::fmt::Debug for Script {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Script")
            .field("realm", &self.inner.realm.addr())
            .field("code", &self.inner.source)
            .field("loaded_modules", &self.inner.loaded_modules)
            .finish()
    }
}

impl std::fmt::Debug for ScriptEdge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScriptEdge")
            .field("realm", &self.inner.realm.addr())
            .field("code", &self.inner.source)
            .field("loaded_modules", &self.inner.loaded_modules)
            .finish()
    }
}

#[derive(Trace, Finalize)]
struct Inner {
    realm: RealmEdge,
    #[unsafe_ignore_trace]
    source: boa_ast::Script,
    source_text: SourceText,
    codeblock: GcRefCell<Option<GcEdge<CodeBlock>>>,
    loaded_modules: GcRefCell<FxHashMap<JsString, crate::module::ModuleEdge>>,
    host_defined: HostDefined,
    path: Option<PathBuf>,
}

impl Script {
    /// Creates an unregistered script edge suitable for traced heap storage.
    #[must_use]
    pub fn to_edge(&self) -> ScriptEdge {
        ScriptEdge {
            inner: self.inner.clone().into_edge(),
        }
    }

    /// Gets the realm of this script.
    #[must_use]
    pub fn realm(&self) -> Realm {
        self.inner.realm.to_rooted()
    }

    /// Returns the [`ECMAScript specification`][spec] defined [`\[\[HostDefined\]\]`][`HostDefined`] field of the [`crate::Module`].
    ///
    /// [spec]: https://tc39.es/ecma262/#script-record
    #[must_use]
    pub fn host_defined(&self) -> &HostDefined {
        &self.inner.host_defined
    }

    /// Gets the loaded modules of this script.
    pub(crate) fn loaded_modules(
        &self,
    ) -> &GcRefCell<FxHashMap<JsString, crate::module::ModuleEdge>> {
        &self.inner.loaded_modules
    }

    /// Abstract operation [`ParseScript ( sourceText, realm, hostDefined )`][spec].
    ///
    /// Parses the provided `src` as an ECMAScript script, returning an error if parsing fails.
    ///
    /// [spec]: https://tc39.es/ecma262/#sec-parse-script
    pub fn parse<R: ReadChar>(
        src: Source<'_, R>,
        realm: Option<Realm>,
        context: &mut Context,
    ) -> JsResult<Self> {
        let path = src.path().map(Path::to_path_buf);
        let mut parser = Parser::new(src);
        parser.set_identifier(context.next_parser_identifier());
        if context.is_strict() {
            parser.set_strict();
        }
        let scope = context.realm().scope().clone();
        let (mut code, source) = parser.parse_script_with_source(&scope, context.interner_mut())?;
        if !context.optimizer_options().is_empty() {
            context.optimize_statement_list(code.statements_mut());
        }

        let source_text = SourceText::new(source);

        let realm = realm.unwrap_or_else(|| context.realm().clone());
        Ok(Self {
            inner: Rooted::new(Inner {
                realm: realm.to_edge(),
                source: code,
                source_text,
                codeblock: GcRefCell::default(),
                loaded_modules: GcRefCell::default(),
                host_defined: HostDefined::default(),
                path,
            }),
        })
    }

    /// Compiles the codeblock of this script.
    ///
    /// This is a no-op if this has been called previously.
    pub fn codeblock(&self, context: &mut Context) -> JsResult<Rooted<CodeBlock>> {
        if let Some(codeblock) = &*self.inner.codeblock.borrow() {
            return Ok(codeblock.clone().root());
        }

        let mut annex_b_function_names = Vec::new();

        global_declaration_instantiation_context(
            &mut annex_b_function_names,
            &self.inner.source,
            self.inner.realm.scope(),
            context,
        )?;

        let spanned_source_text = SpannedSourceText::new_source_only(self.get_source());
        let mut compiler = ByteCompiler::new(
            js_string!("<main>"),
            self.inner.source.strict(),
            false,
            self.inner.realm.scope().clone(),
            self.inner.realm.scope().clone(),
            false,
            false,
            context.interner_mut(),
            false,
            spanned_source_text,
            self.path().map(Path::to_owned).into(),
        );

        #[cfg(feature = "annex-b")]
        {
            compiler.annex_b_function_names = annex_b_function_names;
        }

        // TODO: move to `Script::evaluate` to make this operation infallible.
        compiler.global_declaration_instantiation(&self.inner.source);
        compiler.compile_statement_list(self.inner.source.statements(), true, false);

        let cb = Rooted::new(compiler.finish());

        *self.inner.codeblock.borrow_mut() = Some(cb.clone().into_edge());

        Ok(cb)
    }

    /// Evaluates this script and returns its result.
    ///
    /// Note that this won't run any scheduled promise jobs; you need to call [`Context::run_jobs`]
    /// on the context or [`JobExecutor::run_jobs`] on the provided queue to run them.
    ///
    /// [`JobExecutor::run_jobs`]: crate::job::JobExecutor::run_jobs
    pub fn evaluate(&self, context: &mut Context) -> JsResult<JsValue> {
        self.prepare_run(context)?;
        let record = context.run();

        context.vm.pop_frame();
        record.consume()
    }

    /// Evaluates this script and returns its result, periodically yielding to the executor
    /// in order to avoid blocking the current thread.
    ///
    /// This uses an implementation defined amount of "clock cycles" that need to pass before
    /// execution is suspended. See [`Script::evaluate_async_with_budget`] if you want to also
    /// customize this parameter.
    #[allow(clippy::future_not_send)]
    pub async fn evaluate_async(&self, context: &mut Context) -> JsResult<JsValue> {
        self.evaluate_async_with_budget(context, 256).await
    }

    /// Evaluates this script and returns its result, yielding to the executor each time `budget`
    /// number of "clock cycles" pass.
    ///
    /// Note that "clock cycle" is in quotation marks because we can't determine exactly how many
    /// CPU clock cycles a VM instruction will take, but all instructions have a "cost" associated
    /// with them that depends on their individual complexity. We'd recommend benchmarking with
    /// different budget sizes in order to find the ideal yielding time for your application.
    #[allow(clippy::future_not_send)]
    pub async fn evaluate_async_with_budget(
        &self,
        context: &mut Context,
        budget: u32,
    ) -> JsResult<JsValue> {
        self.prepare_run(context)?;

        let frame = AsyncScriptFrameGuard {
            native_continuation_depth: context.vm.native_call_continuations.len(),
            context,
            completed: false,
        };
        let record = frame.context.run_async_with_budget(budget).await;

        frame.complete();
        record.consume()
    }

    fn prepare_run(&self, context: &mut Context) -> JsResult<()> {
        let codeblock = self.codeblock(context)?;

        let env_fp = context.vm.environments.len() as u32;
        context.vm.push_frame_with_stack(
            CallFrame::new_rooted(
                codeblock,
                Some(ActiveRunnable::Script(self.clone())),
                context.vm.environments.clone(),
                self.inner.realm.to_rooted(),
            )
            .with_env_fp(env_fp)
            .with_flags(CallFrameFlags::EXIT_EARLY),
            JsValue::undefined(),
            JsValue::null(),
        );

        // TODO: Here should be https://tc39.es/ecma262/#sec-globaldeclarationinstantiation

        self.realm().resize_global_env();

        Ok(())
    }

    pub(super) fn path(&self) -> Option<&Path> {
        self.inner.path.as_deref()
    }

    pub(super) fn get_source(&self) -> SourceText {
        self.inner.source_text.clone()
    }
}

impl ScriptEdge {
    /// Creates an explicitly registered external script owner.
    #[must_use]
    pub fn root(&self) -> Script {
        Script {
            inner: self.inner.clone().root(),
        }
    }

    pub(crate) fn to_rooted(&self) -> Script {
        self.root()
    }
}

#[cfg(test)]
mod tests {
    use super::Script;
    use crate::{Context, JsValue, NativeFunction, Source};

    #[test]
    fn native_capture_keeps_script_alive_across_collection() {
        let mut context = Context::default();
        let script = Script::parse(Source::from_bytes("1 + 1"), None, &mut context)
            .expect("script should parse");

        let callback = NativeFunction::from_copy_closure_with_captures(
            |_, _, script, _| {
                let script = script.root();
                assert!(script.path().is_none());
                Ok(JsValue::undefined())
            },
            script.to_edge(),
        )
        .to_js_function(context.realm());

        drop(script);
        boa_gc::force_collect();

        callback
            .call(&JsValue::undefined(), &[], &mut context)
            .expect("captured script should survive collection");
    }
}
