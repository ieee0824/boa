use crate::{JsNativeErrorKind, JsValue, TestAction, run_test_actions};
use boa_macros::js_str;
use indoc::indoc;

#[test]
fn property_accessor_member_expression_dot_notation_on_string_literal() {
    run_test_actions([TestAction::assert_eq(
        "typeof 'asd'.matchAll",
        js_str!("function"),
    )]);
}

#[test]
fn property_accessor_member_expression_bracket_notation_on_string_literal() {
    run_test_actions([TestAction::assert_eq(
        "typeof 'asd'['matchAll']",
        js_str!("function"),
    )]);
}

#[test]
fn short_circuit_evaluation() {
    run_test_actions([
        // OR operation
        TestAction::assert("true || true"),
        TestAction::assert("true || false"),
        TestAction::assert("false || true"),
        TestAction::assert("!(false || false)"),
        // short circuiting OR.
        TestAction::assert_eq(
            indoc! {r#"
                function add_one_a(counter) {
                    counter.value += 1;
                    return true;
                }
                let counter_a = { value: 0 };
                add_one_a(counter_a) || add_one_a(counter_a);
                counter_a.value
            "#},
            1,
        ),
        TestAction::assert_eq(
            indoc! {r#"
                function add_one_b(counter) {
                    counter.value += 1;
                    return false;
                }
                let counter_b = { value: 0 };
                add_one_b(counter_b) || add_one_b(counter_b);
                counter_b.value
            "#},
            2,
        ),
        // AND operation
        TestAction::assert("true && true"),
        TestAction::assert("!(true && false)"),
        TestAction::assert("!(false && true)"),
        TestAction::assert("!(false && false)"),
        // short circuiting AND
        TestAction::assert_eq(
            indoc! {r#"
                function add_one_c(counter) {
                    counter.value += 1;
                    return true;
                }
                let counter_c = { value: 0 };
                add_one_c(counter_c) && add_one_c(counter_c);
                counter_c.value
            "#},
            2,
        ),
        TestAction::assert_eq(
            indoc! {r#"
                function add_one_d(counter) {
                    counter.value += 1;
                    return false;
                }
                let counter_d = { value: 0 };
                add_one_d(counter_d) && add_one_d(counter_d);
                counter_d.value
            "#},
            1,
        ),
    ]);
}

#[test]
fn tilde_operator() {
    run_test_actions([
        // float
        TestAction::assert_eq("~(-1.2)", 0),
        // numeric
        TestAction::assert_eq("~1789", -1790),
        // nan
        TestAction::assert_eq("~NaN", -1),
        // object
        TestAction::assert_eq("~{}", -1),
        // boolean true
        TestAction::assert_eq("~true", -2),
        // boolean false
        TestAction::assert_eq("~false", -1),
    ]);
}

#[test]
fn assign_operator_precedence() {
    run_test_actions([TestAction::assert_eq(
        indoc! {r#"
            let a = 1;
            a = a + 1;
            a
        "#},
        2,
    )]);
}

#[test]
fn unary_pre() {
    run_test_actions([
        TestAction::assert_eq("{ let a = 5; ++a; a }", 6),
        TestAction::assert_eq("{ let b = 5; --b; b }", 4),
        TestAction::assert_eq("{ const c = { a: 5 }; ++c.a; c['a'] }", 6),
        TestAction::assert_eq("{ const d = { a: 5 }; --d['a']; d.a }", 4),
        TestAction::assert_eq("{ let e = 5; ++e }", 6),
        TestAction::assert_eq("{ let f = 5; --f }", 4),
        TestAction::assert_eq("{ let g = 2147483647; ++g }", 2_147_483_648_i64),
        TestAction::assert_eq("{ let h = -2147483648; --h }", -2_147_483_649_i64),
        TestAction::assert_eq(
            indoc! {r#"
                let i = {[Symbol.toPrimitive]() { return 123; }};
                ++i
            "#},
            124,
        ),
        TestAction::assert_eq(
            indoc! {r#"
                let j = {[Symbol.toPrimitive]() { return 123; }};
                ++j
            "#},
            124,
        ),
    ]);
}

#[test]
fn invalid_unary_access() {
    run_test_actions([
        TestAction::assert_native_error(
            "++[]",
            JsNativeErrorKind::Syntax,
            "Invalid left-hand side in assignment at line 1, col 1",
        ),
        TestAction::assert_native_error(
            "[]++",
            JsNativeErrorKind::Syntax,
            "Invalid left-hand side in assignment at line 1, col 3",
        ),
        TestAction::assert_native_error(
            "--[]",
            JsNativeErrorKind::Syntax,
            "Invalid left-hand side in assignment at line 1, col 1",
        ),
        TestAction::assert_native_error(
            "[]--",
            JsNativeErrorKind::Syntax,
            "Invalid left-hand side in assignment at line 1, col 3",
        ),
    ]);
}

#[test]
fn unary_operations_on_this() {
    // https://tc39.es/ecma262/#sec-assignment-operators-static-semantics-early-errors
    run_test_actions([
        TestAction::assert_native_error(
            "++this",
            JsNativeErrorKind::Syntax,
            "Invalid left-hand side in assignment at line 1, col 1",
        ),
        TestAction::assert_native_error(
            "--this",
            JsNativeErrorKind::Syntax,
            "Invalid left-hand side in assignment at line 1, col 1",
        ),
        TestAction::assert_native_error(
            "this++",
            JsNativeErrorKind::Syntax,
            "Invalid left-hand side in assignment at line 1, col 5",
        ),
        TestAction::assert_native_error(
            "this--",
            JsNativeErrorKind::Syntax,
            "Invalid left-hand side in assignment at line 1, col 5",
        ),
    ]);
}

#[test]
fn typeofs() {
    run_test_actions([
        TestAction::assert_eq("typeof String()", js_str!("string")),
        TestAction::assert_eq("typeof 5", js_str!("number")),
        TestAction::assert_eq("typeof 0.5", js_str!("number")),
        TestAction::assert_eq("typeof undefined", js_str!("undefined")),
        TestAction::assert_eq("typeof true", js_str!("boolean")),
        TestAction::assert_eq("typeof null", js_str!("object")),
        TestAction::assert_eq("typeof {}", js_str!("object")),
        TestAction::assert_eq("typeof Symbol()", js_str!("symbol")),
        TestAction::assert_eq("typeof function(){}", js_str!("function")),
    ]);
}

#[test]
fn unary_post() {
    run_test_actions([
        TestAction::assert_eq("{ let a = 5; a++; a }", 6),
        TestAction::assert_eq("{ let b = 5; b--; b }", 4),
        TestAction::assert_eq("{ const c = { a: 5 }; c.a++; c['a'] }", 6),
        TestAction::assert_eq("{ const d = { a: 5 }; d['a']--; d.a }", 4),
        TestAction::assert_eq("{ let e = 5; e++ }", 5),
        TestAction::assert_eq("{ let f = 5; f-- }", 5),
        TestAction::assert_eq("{ let g = 2147483647; g++; g }", 2_147_483_648_i64),
        TestAction::assert_eq("{ let h = -2147483648; h--; h }", -2_147_483_649_i64),
        TestAction::assert_eq(
            indoc! {r#"
                let i = {[Symbol.toPrimitive]() { return 123; }};
                i++
            "#},
            123,
        ),
        TestAction::assert_eq(
            indoc! {r#"
                let j = {[Symbol.toPrimitive]() { return 123; }};
                j--
            "#},
            123,
        ),
    ]);
}

#[test]
fn unary_void() {
    run_test_actions([
        TestAction::assert_eq("{ const a = 0; void a }", JsValue::undefined()),
        TestAction::assert_eq(
            indoc! {r#"
                let a = 0;
                const test = () => a = 42;
                const b = void test() + '';
                a + b
            "#},
            js_str!("42undefined"),
        ),
    ]);
}

#[test]
fn unary_delete() {
    run_test_actions([
        TestAction::assert("{ var a = 5; !(delete a) && a === 5 }"),
        TestAction::assert("{ const a = { b: 5 }; delete a.b && a.b === undefined }"),
        TestAction::assert("{ const a = { b: 5 }; delete a.c && a.b === 5 }"),
        TestAction::assert("{ const a = { b: 5 }; delete a['b'] && a.b === undefined }"),
        TestAction::assert("{ const a = { b: 5 }; !(delete a) }"),
        TestAction::assert("delete []"),
        TestAction::assert("delete function(){}"),
        TestAction::assert("delete delete delete 1"),
    ]);
}

#[test]
fn comma_operator() {
    run_test_actions([
        TestAction::assert_eq(
            indoc! {r#"
                var a, b;
                b = 10;
                a = (b++, b);
                a
            "#},
            11,
        ),
        TestAction::assert_eq(
            indoc! {r#"
                var a, b;
                b = 10;
                a = (b += 5, b /= 3, b - 3);
                a
            "#},
            2,
        ),
    ]);
}

#[test]
fn assignment_to_non_assignable() {
    // Relates to the behaviour described at
    // https://tc39.es/ecma262/#sec-assignment-operators-static-semantics-early-errors
    // Tests all assignment operators as per [spec] and [mdn]
    //
    // [mdn]: https://developer.mozilla.org/en-US/docs/Web/JavaScript/Guide/Expressions_and_Operators#Assignment
    // [spec]: https://tc39.es/ecma262/#prod-AssignmentOperator

    run_test_actions(
        [
            "3 -= 5", "3 *= 5", "3 /= 5", "3 %= 5", "3 &= 5", "3 ^= 5", "3 |= 5", "3 += 5", "3 = 5",
        ]
        .into_iter()
        .map(|src| {
            TestAction::assert_native_error(
                src,
                JsNativeErrorKind::Syntax,
                "Invalid left-hand side in assignment at line 1, col 3",
            )
        }),
    );
}

#[test]
fn assignment_to_non_assignable_ctd() {
    run_test_actions(
        [
            "(()=>{})() -= 5",
            "(()=>{})() *= 5",
            "(()=>{})() /= 5",
            "(()=>{})() %= 5",
            "(()=>{})() &= 5",
            "(()=>{})() ^= 5",
            "(()=>{})() |= 5",
            "(()=>{})() += 5",
            "(()=>{})() = 5",
        ]
        .into_iter()
        .map(|src| {
            TestAction::assert_native_error(
                src,
                JsNativeErrorKind::Syntax,
                "Invalid left-hand side in assignment at line 1, col 12",
            )
        }),
    );
}

#[test]
fn multicharacter_assignment_to_non_assignable() {
    // Relates to the behaviour described at
    // https://tc39.es/ecma262/#sec-assignment-operators-static-semantics-early-errors
    run_test_actions(["3 **= 5", "3 <<= 5", "3 >>= 5"].into_iter().map(|src| {
        TestAction::assert_native_error(
            src,
            JsNativeErrorKind::Syntax,
            "Invalid left-hand side in assignment at line 1, col 3",
        )
    }));
}

#[test]
fn multicharacter_assignment_to_non_assignable_ctd() {
    run_test_actions(
        ["(()=>{})() **= 5", "(()=>{})() <<= 5", "(()=>{})() >>= 5"]
            .into_iter()
            .map(|src| {
                TestAction::assert_native_error(
                    src,
                    JsNativeErrorKind::Syntax,
                    "Invalid left-hand side in assignment at line 1, col 12",
                )
            }),
    );
}

#[test]
fn multicharacter_bitwise_assignment_to_non_assignable() {
    run_test_actions(
        ["3 >>>= 5", "3 &&= 5", "3 ||= 5", "3 ??= 5"]
            .into_iter()
            .map(|src| {
                TestAction::assert_native_error(
                    src,
                    JsNativeErrorKind::Syntax,
                    "Invalid left-hand side in assignment at line 1, col 3",
                )
            }),
    );
}

#[test]
fn multicharacter_bitwise_assignment_to_non_assignable_ctd() {
    run_test_actions(
        [
            "(()=>{})() >>>= 5",
            "(()=>{})() &&= 5",
            "(()=>{})() ||= 5",
            "(()=>{})() ??= 5",
        ]
        .into_iter()
        .map(|src| {
            TestAction::assert_native_error(
                src,
                JsNativeErrorKind::Syntax,
                "Invalid left-hand side in assignment at line 1, col 12",
            )
        }),
    );
}

#[test]
fn assign_to_array_decl() {
    run_test_actions([
        TestAction::assert_native_error(
            "[1] = [2]",
            JsNativeErrorKind::Syntax,
            "Invalid left-hand side in assignment at line 1, col 5",
        ),
        TestAction::assert_native_error(
            "[3, 5] = [7, 8]",
            JsNativeErrorKind::Syntax,
            "Invalid left-hand side in assignment at line 1, col 8",
        ),
        TestAction::assert_native_error(
            "[6, 8] = [2]",
            JsNativeErrorKind::Syntax,
            "Invalid left-hand side in assignment at line 1, col 8",
        ),
        TestAction::assert_native_error(
            "[6] = [2, 9]",
            JsNativeErrorKind::Syntax,
            "Invalid left-hand side in assignment at line 1, col 5",
        ),
    ]);
}

#[test]
fn assign_to_object_decl() {
    run_test_actions([TestAction::assert_native_error(
        "{a: 3} = {a: 5};",
        JsNativeErrorKind::Syntax,
        "unexpected token '=', primary expression at line 1, col 8",
    )]);
}

#[test]
fn assignmentoperator_lhs_not_defined() {
    run_test_actions([TestAction::assert_native_error(
        "a += 5",
        JsNativeErrorKind::Reference,
        "a is not defined",
    )]);
}

#[test]
fn assignmentoperator_rhs_throws_error() {
    run_test_actions([TestAction::assert_native_error(
        "let a; a += b",
        JsNativeErrorKind::Reference,
        "b is not defined",
    )]);
}

#[test]
fn instanceofoperator_rhs_not_object() {
    run_test_actions([TestAction::assert_native_error(
        "let s = new String(); s instanceof 1",
        JsNativeErrorKind::Type,
        "right-hand side of 'instanceof' should be an object, got `number`",
    )]);
}

#[test]
fn instanceofoperator_rhs_not_callable() {
    run_test_actions([TestAction::assert_native_error(
        "let s = new String(); s instanceof {}",
        JsNativeErrorKind::Type,
        "right-hand side of 'instanceof' is not callable",
    )]);
}

#[test]
fn logical_nullish_assignment() {
    run_test_actions([
        TestAction::assert_eq("{ let a = undefined; a ??= 10; a }", 10),
        TestAction::assert_eq("{ let a = 20; a ??= 10; a }", 20),
    ]);
}

#[test]
fn logical_assignment() {
    run_test_actions([
        TestAction::assert("{ let a = false; a &&= 10; !a }"),
        TestAction::assert_eq("{ let a = 20; a &&= 10; a }", 10),
        TestAction::assert_eq("{ let a = null; a ||= 10; a }", 10),
        TestAction::assert_eq("{ let a = 20; a ||= 10; a }", 20),
    ]);
}

#[test]
fn conditional_op() {
    run_test_actions([TestAction::assert_eq("1 === 2 ? 'a' : 'b'", js_str!("b"))]);
}

#[test]
fn delete_variable_in_strict() {
    // Checks as per https://tc39.es/ecma262/#sec-delete-operator-static-semantics-early-errors
    // that delete on a variable name is an error in strict mode code.
    run_test_actions([TestAction::assert_native_error(
        indoc! {r#"
            'use strict';
            let x = 10;
            delete x;
        "#},
        JsNativeErrorKind::Syntax,
        "cannot delete variables in strict mode at line 3, col 1",
    )]);
}

#[test]
fn delete_non_configurable() {
    run_test_actions([TestAction::assert_native_error(
        "'use strict'; delete Boolean.prototype",
        JsNativeErrorKind::Type,
        "Cannot delete property",
    )]);
}

#[test]
fn delete_non_configurable_in_function() {
    run_test_actions([TestAction::assert_native_error(
        indoc! {r#"
            function t() {
                'use strict';
                delete Boolean.prototype;
            }
            t()
        "#},
        JsNativeErrorKind::Type,
        "Cannot delete property",
    )]);
}

#[test]
fn delete_after_strict_function() {
    run_test_actions([TestAction::assert_eq(
        indoc! {r#"
            function t() {
                'use strict';
            }
            t()
            delete Boolean.prototype;
        "#},
        false,
    )]);
}

#[test]
fn delete_in_function_global_strict() {
    run_test_actions([TestAction::assert_native_error(
        indoc! {r#"
            'use strict'
            function a(){
                delete Boolean.prototype;
            }
            a();
        "#},
        JsNativeErrorKind::Type,
        "Cannot delete property",
    )]);
}

#[test]
fn delete_in_function_in_strict_function() {
    run_test_actions([TestAction::assert_eq(
        indoc! {r#"
            function a(){
                return delete Boolean.prototype;
            }
            function b(){
                'use strict';
                return a();
            }
            b();
        "#},
        false,
    )]);
}

#[test]
fn delete_in_strict_function_returned() {
    run_test_actions([TestAction::assert_native_error(
        indoc! {r#"
            function a() {
                'use strict';
                return function () {
                    delete Boolean.prototype;
                }
            }
            a()();
        "#},
        JsNativeErrorKind::Type,
        "Cannot delete property",
    )]);
}

#[test]
fn ops_at_the_end() {
    let msg = "abrupt end";

    let mut actions = vec![TestAction::assert_eq("var a, b=3; a = b ++", 3)];

    let abrupt_op_sources = [
        // there was a bug with different behavior with and without space at the end;
        // so few lines are almost the same except for ending space
        "var a, b=3; a = b **",
        "var a, b=3; a = b ** ",
        "var a, b=3; a = b *",
        "var a, b=3; a = b * ",
        "var a, b=3; a /= b *",
        "var a, b=3; a /= b * ",
        "var a, b=3; a = b /",
        "var a, b=3; a = b / ",
        "var a, b=3; a = b +",
        "var a, b=3; a = b -",
        "var a, b=3; a = b ||",
        "var a, b=3; a = b || ",
        "var a, b=3; a = b ==",
        "var a, b=3; a = b ===",
    ];

    for source in abrupt_op_sources {
        actions.push(TestAction::assert_native_error(
            source,
            JsNativeErrorKind::Syntax,
            msg,
        ));
    }

    actions.push(TestAction::assert_eq("var a, b=3; a = b --", 3));

    run_test_actions(actions);
}

#[test]
fn regex_slash_eq() {
    run_test_actions([
        TestAction::assert_eq("+/=/", JsValue::nan()),
        TestAction::assert_eq("var a = 5; /=/; a", 5),
        TestAction::assert_eq("x = () => /=/;\n\"a=b\".match(x())[0]", js_str!("=")),
    ]);
}

mod in_operator {
    use super::*;

    #[test]
    fn propery_in_object() {
        run_test_actions([TestAction::assert("'a' in {a: 'x'}")]);
    }

    #[test]
    fn property_in_property_chain() {
        run_test_actions([TestAction::assert("'toString' in {}")]);
    }

    #[test]
    fn property_not_in_object() {
        run_test_actions([TestAction::assert("!('b' in {a: 'a'})")]);
    }

    #[test]
    fn number_in_array() {
        // Note: this is valid because the LHS is converted to a prop key with ToPropertyKey
        // and arrays are just fancy objects like {'0': 'a'}
        run_test_actions([TestAction::assert("0 in ['a']")]);
    }

    #[test]
    fn symbol_in_object() {
        run_test_actions([TestAction::assert(indoc! {r#"
                var sym = Symbol('hi');
                sym in { [sym]: 'hello' }
            "#})]);
    }

    #[test]
    fn should_type_error_when_rhs_not_object() {
        run_test_actions([TestAction::assert_native_error(
            "'fail' in undefined",
            JsNativeErrorKind::Type,
            "right-hand side of 'in' should be an object, got `undefined`",
        )]);
    }
}


/// Pins the observable behaviour of `+=` on strings.
///
/// Written ahead of making `+=` append into the string's own allocation rather than
/// building a new one (issue #319). Every case here is one that an in-place append
/// could plausibly break — a second holder seeing its contents change, a register
/// left cleared by a throwing operand, an encoding switch — and each is written to
/// be indistinguishable from concatenation, so it holds either way.
mod string_append {
    use super::{TestAction, run_test_actions};
    use boa_macros::js_str;
    use indoc::indoc;

    #[test]
    fn repeated_appending_builds_the_same_string() {
        run_test_actions([
            TestAction::assert_eq(
                indoc! {r#"
                    (function () {
                    var s = "";
                    for (var i = 0; i < 40; i++) { s += i % 10; }
                      return s;
                    })()
                "#},
                js_str!("0123456789012345678901234567890123456789"),
            ),
            TestAction::assert_eq(
                indoc! {r#"
                    (function () {
                    var t = "";
                    for (var i = 0; i < 40; i++) { t += i % 10; }
                      return t.length;
                    })()
                "#},
                40,
            ),
        ]);
    }

    /// The invariant the whole path rests on. If the append mutated a string
    /// another binding could see, `held` would grow along with `s`.
    #[test]
    fn appending_does_not_disturb_another_binding() {
        run_test_actions([
            TestAction::assert_eq(
                indoc! {r#"
                    (function () {
                    var s = "ab";
                    s += "cd";
                    var held = s;
                    s += "ef";
                      return held;
                    })()
                "#},
                js_str!("abcd"),
            ),
            TestAction::assert_eq(
                indoc! {r#"
                    (function () {
                    var s2 = "ab";
                    s2 += "cd";
                    var held2 = s2;
                    s2 += "ef";
                      return s2;
                    })()
                "#},
                js_str!("abcdef"),
            ),
            // Held in an object property rather than a binding.
            TestAction::assert_eq(
                indoc! {r#"
                    (function () {
                    var s3 = "ab";
                    s3 += "cd";
                    var box3 = { value: s3 };
                    s3 += "ef";
                      return box3.value;
                    })()
                "#},
                js_str!("abcd"),
            ),
            // Held in an array element.
            TestAction::assert_eq(
                indoc! {r#"
                    (function () {
                    var s4 = "ab";
                    s4 += "cd";
                    var seen = [];
                    for (var i = 0; i < 5; i++) { seen.push(s4); s4 += "x"; }
                      return seen.join("|");
                    })()
                "#},
                js_str!("abcd|abcdx|abcdxx|abcdxxx|abcdxxxx"),
            ),
            // Captured by a closure.
            TestAction::assert_eq(
                indoc! {r#"
                    (function () {
                    var s5 = "ab";
                    s5 += "cd";
                    var read = ((captured) => () => captured)(s5);
                    s5 += "ef";
                      return read();
                    })()
                "#},
                js_str!("abcd"),
            ),
        ]);
    }

    /// `s += s` reads the same register it writes.
    #[test]
    fn appending_a_string_to_itself() {
        run_test_actions([
            TestAction::assert_eq(
                indoc! {r#"
                    (function () {
                    var s = "ab";
                    s += s;
                    s += s;
                      return s;
                    })()
                "#},
                js_str!("abababab"),
            ),
            TestAction::assert_eq(
                indoc! {r#"
                    (function () {
                    var a = "ab";
                    a += "cd";
                    var b = a;
                    a += b;
                      return a;
                    })()
                "#},
                js_str!("abcdabcd"),
            ),
        ]);
    }

    /// Latin1 and UTF-16 storage differ, and a Latin1 buffer cannot absorb UTF-16
    /// without rewriting what is already in it.
    #[test]
    fn appending_across_encodings() {
        run_test_actions([
            // Latin1 grown by UTF-16.
            TestAction::assert_eq(
                indoc! {r#"
                    (function () {
                    var s = "ab";
                    s += "cd";
                    s += "\u0100";
                      return s;
                    })()
                "#},
                js_str!("abcd\u{0100}"),
            ),
            // UTF-16 grown by Latin1.
            TestAction::assert_eq(
                indoc! {r#"
                    (function () {
                    var s = "\u0100";
                    s += "\u0101";
                    s += "ab";
                      return s;
                    })()
                "#},
                js_str!("\u{0100}\u{0101}ab"),
            ),
            // Alternating, repeatedly, so the storage flips more than once.
            TestAction::assert_eq(
                indoc! {r#"
                    (function () {
                    var s = "";
                    for (var i = 0; i < 20; i++) { s += (i % 2 ? "a" : "\u0100"); }
                      return s.length;
                    })()
                "#},
                20,
            ),
            TestAction::assert_eq(
                indoc! {r#"
                    (function () {
                    var t = "";
                    for (var i = 0; i < 20; i++) { t += (i % 2 ? "a" : "\u0100"); }
                      return t.charCodeAt(0) + "," + t.charCodeAt(1);
                    })()
                "#},
                js_str!("256,97"),
            ),
            // Surrogate pairs must survive being appended and read back.
            TestAction::assert_eq(
                indoc! {r#"
                    (function () {
                    var s = "a";
                    s += "\u{1F600}";
                    s += "b";
                      return s.length + ":" + s.codePointAt(1).toString(16);
                    })()
                "#},
                js_str!("4:1f600"),
            ),
        ]);
    }

    /// An appended string has to behave like any other string everywhere it is
    /// read, not just when printed.
    #[test]
    fn appended_strings_are_ordinary_strings() {
        run_test_actions([
            TestAction::assert_eq(
                indoc! {r#"
                    (function () {
                    var s = "ab";
                    s += "cd";
                      return s === "abcd";
                    })()
                "#},
                true,
            ),
            TestAction::assert_eq(
                indoc! {r#"
                    (function () {
                    var s = "ab";
                    s += "cd";
                      return JSON.stringify({ [s]: s });
                    })()
                "#},
                js_str!(r#"{"abcd":"abcd"}"#),
            ),
            // As a property key, which goes through interning and hashing.
            TestAction::assert_eq(
                indoc! {r#"
                    (function () {
                    var s = "ab";
                    s += "cd";
                    var o = {};
                    o[s] = 1;
                      return o.abcd;
                    })()
                "#},
                1,
            ),
            TestAction::assert_eq(
                indoc! {r#"
                    (function () {
                    var s = "ab";
                    s += "cd";
                      return [s.length, s[1], s.charCodeAt(3), s.indexOf("cd"), s.slice(1, 3)].join(",");
                    })()
                "#},
                js_str!("4,b,100,2,bc"),
            ),
            // Map keys hash the contents.
            TestAction::assert_eq(
                indoc! {r#"
                    (function () {
                    var s = "ab";
                    s += "cd";
                    var m = new Map([["abcd", "hit"]]);
                      return m.get(s);
                    })()
                "#},
                js_str!("hit"),
            ),
        ]);
    }

    /// The append path is only entered for string + string, because anything else
    /// can throw partway through. If a throwing right-hand side left the register
    /// cleared, `s` would read as `undefined` after the catch instead of keeping
    /// its value.
    #[test]
    fn a_throwing_operand_leaves_the_target_intact() {
        run_test_actions([
            TestAction::assert_eq(
                indoc! {r#"
                    (function () {
                    var s = "ab";
                    s += "cd";
                    try { s += { toString() { throw new Error("no"); } }; } catch (e) {}
                      return s;
                    })()
                "#},
                js_str!("abcd"),
            ),
            TestAction::assert_eq(
                indoc! {r#"
                    (function () {
                    var s = "ab";
                    s += "cd";
                    try { s += Symbol("nope"); } catch (e) {}
                      return s;
                    })()
                "#},
                js_str!("abcd"),
            ),
            // A valueOf that returns a string still has to work, just not through
            // the fast path.
            TestAction::assert_eq(
                indoc! {r#"
                    (function () {
                    var s = "ab";
                    s += { toString() { return "cd"; } };
                      return s;
                    })()
                "#},
                js_str!("abcd"),
            ),
        ]);
    }

    /// `+` where the destination is not the left operand must not be treated as an
    /// append: the left operand is still live afterwards.
    #[test]
    fn plain_concatenation_leaves_both_operands_alone() {
        run_test_actions([
            TestAction::assert_eq(
                indoc! {r#"
                    (function () {
                    var a = "ab";
                    a += "cd";
                    var b = a + "ef";
                      return a + "/" + b;
                    })()
                "#},
                js_str!("abcd/abcdef"),
            ),
            TestAction::assert_eq(
                indoc! {r#"
                    (function () {
                    var a = "ab";
                    a += "cd";
                    var parts = [a + "1", a + "2", a + "3"];
                      return parts.join(",") + "/" + a;
                    })()
                "#},
                js_str!("abcd1,abcd2,abcd3/abcd"),
            ),
        ]);
    }

    /// Numbers and other non-strings keep their existing `+` behaviour.
    #[test]
    fn non_string_addition_is_unchanged() {
        run_test_actions([
            TestAction::assert_eq("var n = 1; n += 2; n", 3),
            TestAction::assert_eq("var m = 1; m += 0.5; m", 1.5),
            TestAction::assert_eq("var sn = 1; sn += 'a'; sn", js_str!("1a")),
            TestAction::assert_eq("var t = 'a'; t += 1; t", js_str!("a1")),
            TestAction::assert_eq("var u = 'a'; u += null; u", js_str!("anull")),
            TestAction::assert_eq("var v = 'a'; v += undefined; v", js_str!("aundefined")),
            TestAction::assert_eq("var w = 'a'; w += [1, 2]; w", js_str!("a1,2")),
            TestAction::assert("var x = 1n; x += 2n; x === 3n"),
        ]);
    }
}
