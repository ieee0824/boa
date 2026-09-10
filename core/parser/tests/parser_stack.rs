//! Stack regressions run on explicitly sized threads, independent of `RUST_MIN_STACK`.
#![allow(unused_crate_dependencies)]
use boa_ast::scope::Scope;
use boa_interner::Interner;
use boa_parser::{Parser, Source};

#[test]
fn shallow_inputs_on_2_mib_stack() {
    std::thread::Builder::new()
        .name("parser-shallow".to_owned())
        .stack_size(2 * 1024 * 1024)
        .spawn(|| {
            let mut interner = Interner::default();
            for source in ["", "42;", "function f(a) { return a + 1; } f(41);"] {
                Parser::new(Source::from_bytes(source))
                    .parse_script(&Scope::new_global(), &mut interner)
                    .expect("shallow script must parse, including with ASAN fake stack");
                let utf16: Vec<u16> = source.encode_utf16().collect();
                Parser::new(Source::from_utf16(&utf16))
                    .parse_script(&Scope::new_global(), &mut interner)
                    .expect("shallow UTF-16 script must parse");
                Parser::new(Source::from_reader(std::io::Cursor::new(source), None))
                    .parse_script(&Scope::new_global(), &mut interner)
                    .expect("shallow reader-backed script must parse");
                Parser::new(Source::from_bytes(source))
                    .parse_module(&Scope::new_global(), &mut interner)
                    .expect("shallow module must parse");
                Parser::new(Source::from_bytes(source))
                    .parse_eval(false, &mut interner)
                    .expect("shallow eval must parse");
                Parser::new(Source::from_bytes(source))
                    .parse_function_body(&mut interner, false, false)
                    .expect("shallow dynamic function body must parse");
            }
            Parser::new(Source::from_bytes("a, b = 42"))
                .parse_formal_parameters(&mut interner, false, false)
                .expect("shallow dynamic function parameters must parse");
        })
        .expect("spawn shallow parser thread")
        .join()
        .expect("shallow parsing must not panic");
}

fn nested_functions() -> String {
    let mut source = "function f() { return ".to_owned();
    source.push_str(&"function () { return ".repeat(14));
    source.push_str(
        "function (a) { var v = a; assert.sameValue(v, 42); return function () { return v; }; }",
    );
    source.push_str(&" };".repeat(15));
    source.push_str("assert.sameValue(f()()()()()()()()()()()()()()()(42)(), 42);");
    source
}

fn parse_on_stack(stack_bytes: usize) {
    std::thread::Builder::new()
        .name(format!("parser-{stack_bytes}"))
        .stack_size(stack_bytes)
        .spawn(|| {
            let source = nested_functions();
            let mut interner = Interner::default();
            let ast = Parser::new(Source::from_bytes(&source))
                .parse_script(&Scope::new_global(), &mut interner)
                .expect("valid nested functions must parse");
            drop(ast);
            let utf16: Vec<u16> = source.encode_utf16().collect();
            Parser::new(Source::from_utf16(&utf16))
                .parse_script(&Scope::new_global(), &mut interner)
                .expect("UTF-16 nested functions must parse");
            Parser::new(Source::from_reader(std::io::Cursor::new(&source), None))
                .parse_script(&Scope::new_global(), &mut interner)
                .expect("reader-backed nested functions must parse");
            Parser::new(Source::from_bytes(&source))
                .parse_module(&Scope::new_global(), &mut interner)
                .expect("nested functions must parse in modules");
            Parser::new(Source::from_bytes(&source))
                .parse_eval(false, &mut interner)
                .expect("nested functions must parse in eval");
            Parser::new(Source::from_bytes(&source))
                .parse_function_body(&mut interner, false, false)
                .expect("nested functions must parse in dynamic function bodies");
            let parameters = format!(
                "arg = ({} 42 {} )",
                "function () { return ".repeat(15),
                "};".repeat(15).trim_end_matches(';')
            );
            Parser::new(Source::from_bytes(&parameters))
                .parse_formal_parameters(&mut interner, false, false)
                .expect("nested functions must parse in parameter initializers");
            // The grammar budget must not impose a small syntax-depth limit on
            // optimized builds whose native frames already fit the host stack.
            #[cfg(not(debug_assertions))]
            for (name, source) in [
                (
                    "arrays",
                    format!("{}0{};", "[".repeat(100), "]".repeat(100)),
                ),
                (
                    "parentheses",
                    format!("{}0{};", "(".repeat(100), ")".repeat(100)),
                ),
            ] {
                Parser::new(Source::from_bytes(&source))
                    .parse_script(&Scope::new_global(), &mut interner)
                    .unwrap_or_else(|error| {
                        panic!(
                            "{name}: optimized parsing must retain ordinary deep nesting: {error}"
                        )
                    });
            }
            // Flat lists and iteratively parsed chains do not consume the recursion budget.
            for source in [
                "var value = 0;\n".repeat(2048),
                format!("{}0;", "0 + ".repeat(2048)),
                format!("f{};", "()".repeat(2048)),
            ] {
                Parser::new(Source::from_bytes(&source))
                    .parse_script(&Scope::new_global(), &mut interner)
                    .expect("large non-recursive input must still parse");
            }
        })
        .expect("spawn parser thread")
        .join()
        .expect("parser thread must not panic");
}

#[test]
fn nested_functions_on_2_mib_stack() {
    parse_on_stack(2 * 1024 * 1024);
}

#[test]
fn nested_functions_on_8_mib_stack() {
    parse_on_stack(8 * 1024 * 1024);
}

fn deep_inputs(depth: usize) -> [(&'static str, String); 15] {
    [
        (
            "functions",
            format!(
                "{}42;{}",
                "function f() { return ".repeat(depth),
                "};".repeat(depth)
            ),
        ),
        (
            "parentheses",
            format!("{}0{};", "(".repeat(depth), ")".repeat(depth)),
        ),
        (
            "arrays",
            format!("{}0{};", "[".repeat(depth), "]".repeat(depth)),
        ),
        (
            "blocks",
            format!("{};{}", "{".repeat(depth), "}".repeat(depth)),
        ),
        ("unary", format!("{}0;", "!".repeat(depth))),
        ("assignment", format!("{}0;", "a=".repeat(depth))),
        ("new", format!("{}A;", "new ".repeat(depth))),
        ("arrow", format!("{}0;", "() => ".repeat(depth))),
        (
            "template",
            format!("{}0{};", "`${".repeat(depth), "}`".repeat(depth)),
        ),
        ("condition", format!("{}0;", "a ? 0 : ".repeat(depth))),
        ("exponentiation", format!("{}1;", "1 ** ".repeat(depth))),
        ("logical-or", format!("{}true;", "false || ".repeat(depth))),
        (
            "arguments",
            format!("{}0{};", "f(".repeat(depth), ")".repeat(depth)),
        ),
        (
            "computed member",
            format!("{}0{};", "a[".repeat(depth), "]".repeat(depth)),
        ),
        (
            "objects",
            format!("({}0{});", "{x:".repeat(depth), "}".repeat(depth)),
        ),
    ]
}

// Preserve the last input label in CI artifacts if the process aborts.
#[allow(clippy::print_stderr)]
fn reject_deep_input(stack_bytes: usize) {
    std::thread::Builder::new()
        .name(format!("parser-deep-{stack_bytes}"))
        .stack_size(stack_bytes)
        .spawn(|| {
            let depth = 8192;
            for (name, source) in deep_inputs(depth) {
                eprintln!("deep input: {name}");
                let mut interner = Interner::default();
                let Err(error) = Parser::new(Source::from_bytes(&source))
                    .parse_script(&Scope::new_global(), &mut interner)
                else {
                    panic!("{name}: excessive recursion must return an error");
                };
                assert!(
                    error
                        .to_string()
                        .contains("parser recursion limit exceeded"),
                    "{name}: {error}"
                );
                for error in [
                    Parser::new(Source::from_bytes(&source))
                        .parse_module(&Scope::new_global(), &mut interner)
                        .err(),
                    Parser::new(Source::from_bytes(&source))
                        .parse_eval(false, &mut interner)
                        .err(),
                    Parser::new(Source::from_bytes(&source))
                        .parse_function_body(&mut interner, false, false)
                        .err(),
                ] {
                    assert!(
                        error.is_some_and(|e| e
                            .to_string()
                            .contains("parser recursion limit exceeded")),
                        "{name}"
                    );
                }
                Parser::new(Source::from_bytes("let recovered = 42;"))
                    .parse_script(&Scope::new_global(), &mut interner)
                    .expect("parsing remains usable after a resource limit error");
            }
            let parameters = format!("arg = {}0{}", "[".repeat(depth), "]".repeat(depth));
            let Err(error) = Parser::new(Source::from_bytes(&parameters)).parse_formal_parameters(
                &mut Interner::default(),
                false,
                false,
            ) else {
                panic!("parameter initializers must share the recursion limit");
            };
            assert!(
                error
                    .to_string()
                    .contains("parser recursion limit exceeded")
            );
        })
        .expect("spawn parser thread")
        .join()
        .expect("parser must reject excessive recursion without a panic");
}

#[test]
fn deep_input_on_2_mib_stack() {
    reject_deep_input(2 * 1024 * 1024);
}

#[test]
fn deep_input_on_8_mib_stack() {
    reject_deep_input(8 * 1024 * 1024);
}
