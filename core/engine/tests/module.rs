#![allow(unused_crate_dependencies, missing_docs)]

use std::cell::RefCell;
use std::rc::Rc;

use boa_engine::builtins::promise::PromiseState;
use boa_engine::module::{ModuleLoader, Referrer};
use boa_engine::{Context, JsNativeError, JsResult, JsString, Module, Source, js_string};

#[test]
fn test_json_module_from_str() {
    struct TestModuleLoader(JsString);
    impl ModuleLoader for TestModuleLoader {
        async fn load_imported_module(
            self: Rc<Self>,
            _referrer: Referrer,
            specifier: JsString,
            context: &RefCell<&mut Context>,
        ) -> JsResult<Module> {
            assert_eq!(specifier.to_std_string_escaped(), "basic");
            let src = self.0.clone();

            Ok(Module::parse_json(src, &mut context.borrow_mut()).unwrap())
        }
    }

    let json_string = js_string!(r#"{"key":"value","other":123}"#);
    let mut context = Context::builder()
        .module_loader(Rc::new(TestModuleLoader(json_string.clone())))
        .build()
        .unwrap();

    let source = Source::from_bytes(
        b"
        import basic_json from 'basic';
        export let json = basic_json;
    ",
    );

    let module = Module::parse(source, None, &mut context).unwrap();
    let promise = module.load_link_evaluate(&mut context);
    context.run_jobs().unwrap();

    match promise.state() {
        PromiseState::Pending => {}
        PromiseState::Fulfilled(v) => {
            assert!(v.is_undefined());
        }
        PromiseState::Rejected(e) => {
            panic!("Unexpected error: {:?}", e.to_string(&mut context).unwrap());
        }
    }

    let json = module
        .namespace(&mut context)
        .get(js_string!("json"), &mut context)
        .unwrap();

    assert_eq!(
        JsString::from(json.to_json(&mut context).unwrap().unwrap().to_string()),
        json_string
    );
}

#[test]
fn async_load_link_evaluate_drives_module_graph_with_tla() {
    struct GraphLoader;
    impl ModuleLoader for GraphLoader {
        async fn load_imported_module(
            self: Rc<Self>,
            _referrer: Referrer,
            specifier: JsString,
            context: &RefCell<&mut Context>,
        ) -> JsResult<Module> {
            assert_eq!(specifier.to_std_string_escaped(), "dependency");
            Module::parse(
                Source::from_bytes("await Promise.resolve(); export const value = 41;"),
                None,
                &mut context.borrow_mut(),
            )
        }
    }

    let mut context = Context::builder()
        .module_loader(Rc::new(GraphLoader))
        .build()
        .unwrap();
    let module = Module::parse(
        Source::from_bytes(
            "import { value } from 'dependency'; await Promise.resolve(); export const result = value + 1;",
        ),
        None,
        &mut context,
    )
    .unwrap();

    futures_lite::future::block_on(module.load_link_evaluate_async(&mut context)).unwrap();
    assert_eq!(
        module
            .namespace(&mut context)
            .get(js_string!("result"), &mut context)
            .unwrap(),
        42.into()
    );
}

#[test]
fn async_load_link_evaluate_propagates_tla_rejection() {
    let mut context = Context::default();
    let module = Module::parse(
        Source::from_bytes("await Promise.reject('module failed')"),
        None,
        &mut context,
    )
    .unwrap();

    let error =
        futures_lite::future::block_on(module.load_link_evaluate_async(&mut context)).unwrap_err();
    assert_eq!(
        error.to_opaque(&mut context),
        js_string!("module failed").into()
    );
}

#[test]
fn async_load_link_evaluate_propagates_load_error() {
    struct FailingLoader;
    impl ModuleLoader for FailingLoader {
        async fn load_imported_module(
            self: Rc<Self>,
            _referrer: Referrer,
            _specifier: JsString,
            _context: &RefCell<&mut Context>,
        ) -> JsResult<Module> {
            Err(JsNativeError::error().with_message("load failed").into())
        }
    }

    let mut context = Context::builder()
        .module_loader(Rc::new(FailingLoader))
        .build()
        .unwrap();
    let module = Module::parse(Source::from_bytes("import 'missing'"), None, &mut context).unwrap();

    let error =
        futures_lite::future::block_on(module.load_link_evaluate_async(&mut context)).unwrap_err();
    assert!(error.to_string().contains("load failed"));
}
