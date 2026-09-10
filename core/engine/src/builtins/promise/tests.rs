use crate::{JsValue, NativeFunction, TestAction, js_string, run_test_actions};
use indoc::indoc;

#[test]
fn promise() {
    run_test_actions([
        TestAction::run(indoc! {r#"
                    let count = 0;
                    const promise = new Promise((resolve, reject) => {
                        count += 1;
                        resolve(undefined);
                    }).then((_) => (count += 1));
                    count += 1;
                "#}),
        TestAction::assert_eq("count", 2),
        TestAction::inspect_context(|ctx| ctx.run_jobs().unwrap()),
        TestAction::assert_eq("count", 3),
    ]);
}

#[test]
fn resolving_promise_survives_collection_in_then_getter() {
    for (then_body, expected) in [
        ("return done => done(42)", "fulfilled:42"),
        ("throw 42", "rejected:42"),
        ("return undefined", "fulfilled:[object Object]"),
    ] {
        let source = format!(
            "let resolve, reject, result, getterCalls = 0;\
             new Promise((yes, no) => {{ resolve = yes; reject = no; }}).then(\
                 value => result = 'fulfilled:' + value,\
                 reason => result = 'rejected:' + reason);\
             resolve({{ get then() {{\
                 getterCalls++;\
                 forceCollect();\
                 resolve(99);\
                 reject(99);\
                 {then_body};\
             }} }});"
        );
        run_test_actions([
            TestAction::inspect_context(|ctx| {
                ctx.register_global_builtin_callable(
                    js_string!("forceCollect"),
                    0,
                    NativeFunction::from_fn_ptr(|_, _, _| {
                        boa_gc::force_collect();
                        Ok(JsValue::undefined())
                    }),
                )
                .unwrap();
            }),
            TestAction::run(source),
            TestAction::inspect_context(|ctx| ctx.run_jobs().unwrap()),
            TestAction::assert_eq("getterCalls", 1),
            TestAction::assert_eq("result", js_string!(expected)),
        ]);
    }
}
