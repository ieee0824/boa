use crate::{TestAction, run_test_actions};
use boa_macros::js_str;
use indoc::indoc;

#[test]
fn class_field_initializer_name_static() {
    run_test_actions([
        TestAction::run(indoc! {r#"
            class C {
                static a = function() {};
                static ["b"] = function() {};
                static #c = function() {};
                static c = this.#c
            }
        "#}),
        TestAction::assert_eq("C.a.name", js_str!("a")),
        TestAction::assert_eq("C.b.name", js_str!("b")),
        TestAction::assert_eq("C.c.name", js_str!("#c")),
    ]);
}

#[test]
fn class_field_initializer_name() {
    run_test_actions([
        TestAction::run(indoc! {r#"
            class C {
                a = function() {};
                ["b"] = function() {};
                #c = function() {};
                c = this.#c
            }
            let c = new C();
        "#}),
        TestAction::assert_eq("c.a.name", js_str!("a")),
        TestAction::assert_eq("c.b.name", js_str!("b")),
        TestAction::assert_eq("c.c.name", js_str!("#c")),
    ]);
}

#[test]
fn class_superclass_from_regex_error() {
    run_test_actions([TestAction::assert_native_error(
        "class A extends /=/ {}",
        crate::JsNativeErrorKind::Type,
        "superclass must be a constructor",
    )]);
}

// https://github.com/boa-dev/boa/issues/3055
#[test]
fn class_can_access_super_from_static_initializer() {
    run_test_actions([
        TestAction::run(indoc! {r#"
            class a {
                static field = "super field";
            }

            class b extends a {
                static #field = super.field;
                static get field() {
                    return this.#field;
                }
            }

            class c extends a {
                static field = super.field;
            }

        "#}),
        TestAction::assert_eq("a.field", js_str!("super field")),
        TestAction::assert_eq("b.field", js_str!("super field")),
        TestAction::assert_eq("c.field", js_str!("super field")),
    ]);
}

#[test]
fn class_field_initializers_capture_outer_lexical_bindings() {
    run_test_actions([
        TestAction::run(indoc! {r#"
            function makeClass() {
                const value = "captured";
                return class {
                    publicField = () => value;
                    static publicStatic = () => value;
                    #privateField = () => value;
                    static #privateStatic = () => value;

                    privateValue() { return this.#privateField(); }
                    static privateStaticValue() { return this.#privateStatic(); }
                };
            }
            const CapturingClass = makeClass();
            const capturingInstance = new CapturingClass();
        "#}),
        TestAction::assert_eq("capturingInstance.publicField()", js_str!("captured")),
        TestAction::assert_eq("CapturingClass.publicStatic()", js_str!("captured")),
        TestAction::assert_eq("capturingInstance.privateValue()", js_str!("captured")),
        TestAction::assert_eq("CapturingClass.privateStaticValue()", js_str!("captured")),
    ]);
}

#[test]
fn static_class_field_arrows_capture_module_binding() {
    run_test_actions([
        TestAction::run(indoc! {r#"
            let language;
            class Text {
                static setLanguage = value => { language = value; };
                static getLanguage = () => language;
            }
            Text.setLanguage("ja");
        "#}),
        TestAction::assert_eq("Text.getLanguage()", js_str!("ja")),
    ]);
}
