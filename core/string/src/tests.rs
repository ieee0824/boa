#![allow(clippy::redundant_clone)]

use std::hash::{BuildHasher, BuildHasherDefault, Hash};

use crate::{
    CommonJsStringBuilder, JsStr, JsString, Latin1JsStringBuilder, RawJsString, StaticJsStrings,
    Utf16JsStringBuilder,
};

use rustc_hash::FxHasher;

fn hash_value<T: Hash>(value: &T) -> u64 {
    BuildHasherDefault::<FxHasher>::default().hash_one(value)
}

const fn ascii_to_utf16<const LEN: usize>(ascii: &[u8; LEN]) -> [u16; LEN] {
    let mut array = [0; LEN];
    let mut i = 0;
    while i < LEN {
        array[i] = ascii[i] as u16;
        i += 1;
    }
    array
}

#[test]
fn empty() {
    let s = StaticJsStrings::EMPTY_STRING;
    assert_eq!(&s, &[]);
}

#[test]
fn refcount() {
    let x = JsString::from("Hello world");
    assert_eq!(x.refcount(), Some(1));

    {
        let y = x.clone();
        assert_eq!(x.refcount(), Some(2));
        assert_eq!(y.refcount(), Some(2));

        {
            let z = y.clone();
            assert_eq!(x.refcount(), Some(3));
            assert_eq!(y.refcount(), Some(3));
            assert_eq!(z.refcount(), Some(3));
        }

        assert_eq!(x.refcount(), Some(2));
        assert_eq!(y.refcount(), Some(2));
    }

    assert_eq!(x.refcount(), Some(1));
}

#[test]
fn static_refcount() {
    let x = StaticJsStrings::EMPTY_STRING;
    assert_eq!(x.refcount(), None);

    {
        let y = x.clone();
        assert_eq!(x.refcount(), None);
        assert_eq!(y.refcount(), None);
    };

    assert_eq!(x.refcount(), None);
}

#[test]
fn ptr_eq() {
    let x = JsString::from("Hello");
    let y = x.clone();

    assert!(!x.is_static());

    assert_eq!(x.ptr.addr(), y.ptr.addr());

    let z = JsString::from("Hello");
    assert_ne!(x.ptr.addr(), z.ptr.addr());
    assert_ne!(y.ptr.addr(), z.ptr.addr());
}

#[test]
fn static_ptr_eq() {
    let x = StaticJsStrings::EMPTY_STRING;
    let y = x.clone();

    assert!(x.is_static());

    assert_eq!(x.ptr.addr(), y.ptr.addr());

    let z = StaticJsStrings::EMPTY_STRING;
    assert_eq!(x.ptr.addr(), z.ptr.addr());
    assert_eq!(y.ptr.addr(), z.ptr.addr());
}

#[test]
fn as_str() {
    const HELLO: &[u16] = &ascii_to_utf16(b"Hello");
    let x = JsString::from(HELLO);

    assert_eq!(&x, HELLO);
}

#[test]
fn hash() {
    const HELLOWORLD: JsStr<'_> = JsStr::latin1("Hello World!".as_bytes());
    let x = JsString::from(HELLOWORLD);

    assert_eq!(x.as_str(), HELLOWORLD);

    assert!(HELLOWORLD.is_latin1());
    assert!(x.as_str().is_latin1());

    let s_hash = hash_value(&HELLOWORLD);
    let x_hash = hash_value(&x);

    assert_eq!(s_hash, x_hash);
}

#[test]
fn concat() {
    const Y: &[u16] = &ascii_to_utf16(b", ");
    const W: &[u16] = &ascii_to_utf16(b"!");

    let x = JsString::from("hello");
    let z = JsString::from("world");

    let xy = JsString::concat(x.as_str(), JsString::from(Y).as_str());
    assert_eq!(&xy, &ascii_to_utf16(b"hello, "));
    assert_eq!(xy.refcount(), Some(1));

    let xyz = JsString::concat(xy.as_str(), z.as_str());
    assert_eq!(&xyz, &ascii_to_utf16(b"hello, world"));
    assert_eq!(xyz.refcount(), Some(1));

    let xyzw = JsString::concat(xyz.as_str(), JsString::from(W).as_str());
    assert_eq!(&xyzw, &ascii_to_utf16(b"hello, world!"));
    assert_eq!(xyzw.refcount(), Some(1));
}

#[test]
fn trim_start_non_ascii_to_ascii() {
    let s = "\u{2029}abc";
    let x = JsString::from(s);

    let y = JsString::from(x.trim_start());

    assert_eq!(&y, s.trim_start());
}

#[test]
fn conversion_to_known_static_js_string() {
    const JS_STR_U8: &JsStr<'_> = &JsStr::latin1("length".as_bytes());
    const JS_STR_U16: &JsStr<'_> = &JsStr::utf16(&ascii_to_utf16(b"length"));

    assert!(JS_STR_U8.is_latin1());
    assert!(!JS_STR_U16.is_latin1());

    assert_eq!(JS_STR_U8, JS_STR_U8);
    assert_eq!(JS_STR_U16, JS_STR_U16);

    assert_eq!(JS_STR_U8, JS_STR_U16);
    assert_eq!(JS_STR_U16, JS_STR_U8);

    assert_eq!(hash_value(JS_STR_U8), hash_value(JS_STR_U16));

    let string = StaticJsStrings::get_string(JS_STR_U8);

    assert!(string.is_some());
    assert!(string.unwrap().as_str().is_latin1());

    let string = StaticJsStrings::get_string(JS_STR_U16);

    assert!(string.is_some());
    assert!(string.unwrap().as_str().is_latin1());
}

#[test]
fn to_std_string_escaped() {
    assert_eq!(
        JsString::from("Hello, \u{1D49E} world!").to_std_string_escaped(),
        "Hello, \u{1D49E} world!"
    );

    assert_eq!(
        JsString::from("Hello, world!").to_std_string_escaped(),
        "Hello, world!"
    );

    // 15 should not be escaped.
    let unpaired_surrogates: [u16; 3] = [0xDC58, 0xD83C, 0x0015];
    assert_eq!(
        JsString::from(&unpaired_surrogates).to_std_string_escaped(),
        "\\uDC58\\uD83C\u{15}"
    );
}

#[test]
fn from_static_js_string() {
    static STATIC_HELLO_WORLD: JsStr<'static> = JsStr::latin1("hello world".as_bytes());
    static STATIC_EMOJIS: JsStr<'static> =
        JsStr::utf16(&[0xD83C, 0xDFB9, 0xD83C, 0xDFB6, 0xD83C, 0xDFB5]); // 🎹🎶🎵

    let latin1 = JsString::from_static_js_str(&STATIC_HELLO_WORLD);
    let utf16 = JsString::from_static_js_str(&STATIC_EMOJIS);

    // content compare
    assert_eq!(latin1, "hello world");
    assert_eq!(utf16, "🎹🎶🎵");

    // refcount check
    let clone = latin1.clone();

    assert_eq!(clone, latin1);

    let clone = utf16.clone();

    assert_eq!(clone, utf16);

    assert!(latin1.refcount().is_none());
    assert!(utf16.refcount().is_none());

    // `is_latin1` check
    assert!(latin1.as_str().is_latin1());
    assert!(!utf16.as_str().is_latin1());
}

#[test]
fn compare_static_and_dynamic_js_string() {
    static STATIC_HELLO_WORLD: JsStr<'static> = JsStr::latin1("hello world".as_bytes());
    static STATIC_EMOJIS: JsStr<'static> =
        JsStr::utf16(&[0xD83C, 0xDFB9, 0xD83C, 0xDFB6, 0xD83C, 0xDFB5]); // 🎹🎶🎵

    let static_latin1 = JsString::from_static_js_str(&STATIC_HELLO_WORLD);
    let static_utf16 = JsString::from_static_js_str(&STATIC_EMOJIS);

    let dynamic_latin1 = JsString::from(JsStr::latin1("hello world".as_bytes()));
    let dynamic_utf16 = JsString::from(&[0xD83C, 0xDFB9, 0xD83C, 0xDFB6, 0xD83C, 0xDFB5]);

    // content compare
    assert_eq!(static_latin1, dynamic_latin1);
    assert_eq!(static_utf16, dynamic_utf16);

    // length check
    assert_eq!(static_latin1.len(), dynamic_latin1.len());
    assert_eq!(static_utf16.len(), dynamic_utf16.len());

    // `is_static` check
    assert!(static_latin1.is_static());
    assert!(static_utf16.is_static());
    assert!(!dynamic_latin1.is_static());
    assert!(!dynamic_utf16.is_static());
}

#[test]
#[allow(clippy::cast_possible_truncation)]
#[allow(clippy::undocumented_unsafe_blocks)]
fn js_string_builder() {
    let s = "2024年5月21日";
    let utf16 = s.encode_utf16().collect::<Vec<_>>();
    let s_utf16 = utf16.as_slice();
    let ascii = "Lorem ipsum dolor sit amet";
    let s_ascii = ascii.as_bytes();
    let latin1_as_utf8_literal = "Déjà vu";
    let s_latin1_literal: &[u8] = &[
        b'D', 0xE9, /* é */
        b'j', 0xE0, /* à */
        b' ', b'v', b'u',
    ];

    // latin1 builder -- test

    // push ascii
    let mut builder = Latin1JsStringBuilder::new();
    for &code in s_ascii {
        builder.push(code);
    }
    let s_builder = builder.build().unwrap_or_default();
    assert_eq!(s_builder, ascii);

    // push latin1
    let mut builder = Latin1JsStringBuilder::new();
    for &code in s_latin1_literal {
        builder.push(code);
    }
    let s_builder = unsafe { builder.build_as_latin1() };
    assert_eq!(
        s_builder.to_std_string().unwrap_or_default(),
        latin1_as_utf8_literal
    );

    // from_iter ascii
    let s_builder = s_ascii
        .iter()
        .copied()
        .collect::<Latin1JsStringBuilder>()
        .build()
        .unwrap_or_default();
    assert_eq!(s_builder.to_std_string().unwrap_or_default(), ascii);

    // from_iter latin1
    let s_builder = unsafe {
        s_latin1_literal
            .iter()
            .copied()
            .collect::<Latin1JsStringBuilder>()
            .build_as_latin1()
    };
    assert_eq!(
        s_builder.to_std_string().unwrap_or_default(),
        latin1_as_utf8_literal
    );

    // extend_from_slice ascii
    let mut builder = Latin1JsStringBuilder::new();
    builder.extend_from_slice(s_ascii);
    let s_builder = builder.build().unwrap_or_default();
    assert_eq!(s_builder.to_std_string().unwrap_or_default(), ascii);

    // extend_from_slice latin1
    let mut builder = Latin1JsStringBuilder::new();
    builder.extend_from_slice(s_latin1_literal);
    let s_builder = unsafe { builder.build_as_latin1() };
    assert_eq!(
        s_builder.to_std_string().unwrap_or_default(),
        latin1_as_utf8_literal
    );

    // build from utf16 encoded string
    let s_builder = s
        .as_bytes()
        .iter()
        .copied()
        .collect::<Latin1JsStringBuilder>()
        .build();
    assert_eq!(None, s_builder);

    let s_builder = s_utf16
        .iter()
        .copied()
        .map(|v| v as u8)
        .collect::<Latin1JsStringBuilder>()
        .build();
    assert_eq!(None, s_builder);

    // utf16 builder -- test

    // push
    let mut builder = Utf16JsStringBuilder::new();
    for &code in s_utf16 {
        builder.push(code);
    }
    let s_builder = builder.build();
    assert_eq!(s_builder.to_std_string().unwrap_or_default(), s);

    // from_iter
    let s_builder = s_utf16
        .iter()
        .copied()
        .collect::<Utf16JsStringBuilder>()
        .build();
    assert_eq!(s_builder.to_std_string().unwrap_or_default(), s);

    // extend_from_slice
    let mut builder = Utf16JsStringBuilder::new();
    builder.extend_from_slice(s_utf16);
    let s_builder = builder.build();
    assert_eq!(s_builder.to_std_string().unwrap_or_default(), s);
}

#[test]
fn clone_builder() {
    // latin1 builder -- test
    let origin = Latin1JsStringBuilder::from(&b"0123456789"[..]);
    let empty_origin = Latin1JsStringBuilder::new();

    // clone == origin
    let cloned = origin.clone();
    assert_eq!(origin, cloned);

    // clone_from == origin
    let mut cloned_from = Latin1JsStringBuilder::new();
    cloned_from.clone_from(&origin);
    assert_eq!(origin, cloned_from);

    // clone == origin(empty)
    let cloned = empty_origin.clone();
    assert_eq!(empty_origin, cloned);

    // clone_from == origin(empty)

    cloned_from.clone_from(&empty_origin);
    assert!(cloned_from.capacity() > 0); // Should not be reallocated so the capacity is preserved.
    assert_eq!(empty_origin, cloned_from);

    // clone_from(empty) == origin(empty)
    let mut cloned_from = Latin1JsStringBuilder::new();
    cloned_from.clone_from(&empty_origin);
    assert!(cloned_from.capacity() == 0);
    assert_eq!(empty_origin, cloned_from);

    // utf16 builder -- test
    let s = "2024年5月21日";

    let origin = Utf16JsStringBuilder::from(s.encode_utf16().collect::<Vec<_>>().as_slice());
    let empty_origin = Utf16JsStringBuilder::new();
    // clone == origin
    let cloned = origin.clone();
    assert_eq!(origin, cloned);

    // clone_from == origin(empty)
    let mut cloned_from = Utf16JsStringBuilder::new();
    cloned_from.clone_from(&origin);

    assert_eq!(origin, cloned_from);
    // clone == origin(empty)
    let cloned = empty_origin.clone();
    assert_eq!(empty_origin, cloned);

    // clone_from == origin(empty)

    cloned_from.clone_from(&empty_origin);
    assert!(cloned_from.capacity() > 0); // should not be reallocated so the capacity is preserved.
    assert_eq!(empty_origin, cloned_from);

    // clone_from(empty) == origin(empty)
    let mut cloned_from = Utf16JsStringBuilder::new();
    cloned_from.clone_from(&empty_origin);
    assert!(cloned_from.capacity() == 0);
    assert_eq!(empty_origin, cloned_from);
}

#[test]
fn common_js_string_builder() {
    let utf16 = "2024年5月21日".encode_utf16().collect::<Vec<_>>();
    let s_utf16 = utf16.as_slice();
    let s = "Lorem ipsum dolor sit amet";
    let js_str_utf16 = JsStr::utf16(s_utf16);
    let js_str_ascii = JsStr::latin1(s.as_bytes());
    let latin1_bytes = [
        b'D', 0xE9, /* é */
        b'j', 0xE0, /* à */
        b' ', b'v', b'u',
    ];
    let ch = '🎹';
    let mut builder = CommonJsStringBuilder::with_capacity(10);
    builder += ch;
    builder += s;
    builder += js_str_utf16;
    builder += js_str_ascii;
    builder += ch;
    assert_eq!(builder.len(), 5);
    let js_string = builder.build_from_utf16();
    assert_eq!(
        js_string,
        "🎹Lorem ipsum dolor sit amet2024年5月21日Lorem ipsum dolor sit amet🎹"
    );
    let mut builder = CommonJsStringBuilder::new();
    for b in latin1_bytes {
        builder += b;
    }
    builder += s_utf16;
    builder += ch;
    let js_string = builder.build();
    assert_eq!(
        js_string.to_std_string().unwrap_or_default(),
        "Déjà vu2024年5月21日🎹"
    );
}


/// The header is on every heap-allocated string, so a field added to it is paid
/// for by the whole program. Pinned so that growing it is a deliberate act with a
/// measurement attached rather than a side effect of an unrelated change.
#[test]
fn raw_js_string_header_size_is_pinned() {
    assert_eq!(size_of::<RawJsString>(), 3 * size_of::<usize>());
    assert_eq!(align_of::<RawJsString>(), align_of::<usize>());
    assert_eq!(crate::DATA_OFFSET, 3 * size_of::<usize>());
}

/// Exact allocations keep reporting a capacity equal to their length, which is
/// what every existing caller produces.
#[test]
fn exact_allocations_have_capacity_equal_to_length() {
    for string in [
        JsString::from("abc"),
        JsString::from("a longer latin1 string, still exact"),
        JsString::from(JsStr::utf16(&[0x41, 0x0100, 0x42])),
    ] {
        assert_eq!(string.capacity(), Some(string.len()));
    }
}

/// Static strings have no allocation to describe, so they report no capacity —
/// the same convention `refcount` already uses.
#[test]
fn static_strings_report_no_capacity() {
    let static_string = StaticJsStrings::EMPTY_STRING;
    assert!(static_string.is_static());
    assert_eq!(static_string.capacity(), None);
    assert_eq!(static_string.refcount(), None);
}

/// The point of the whole change: a string whose allocation is larger than its
/// contents still behaves as a string of its length.
#[test]
fn over_allocated_strings_behave_as_their_length() {
    let latin1 = JsString::with_capacity_from(JsStr::latin1(b"abc"), 64);
    assert_eq!(latin1.len(), 3);
    assert_eq!(latin1.capacity(), Some(64));
    assert_eq!(latin1, JsString::from("abc"));
    assert_eq!(latin1.to_std_string_escaped(), "abc");
    assert_eq!(latin1.get_expect(1), 0x62);
    assert_eq!(latin1.as_str().len(), 3);
    assert!(latin1.as_str().is_latin1());

    let utf16 = JsString::with_capacity_from(JsStr::utf16(&[0x41, 0x0100]), 32);
    assert_eq!(utf16.len(), 2);
    assert_eq!(utf16.capacity(), Some(32));
    assert_eq!(utf16, JsString::from(JsStr::utf16(&[0x41, 0x0100])));
    assert_eq!(utf16.get_expect(1), 0x0100);
    assert!(!utf16.as_str().is_latin1());
}

/// Hashing and ordering read the contents, so the slack must not leak into them.
#[test]
fn over_allocated_strings_hash_and_compare_by_contents() {
    let slack = JsString::with_capacity_from(JsStr::latin1(b"abc"), 64);
    let exact = JsString::from("abc");
    assert_eq!(hash_value(&slack), hash_value(&exact));
    assert_eq!(slack.cmp(&exact), std::cmp::Ordering::Equal);

    let longer = JsString::with_capacity_from(JsStr::latin1(b"abd"), 64);
    assert_ne!(slack, longer);
    assert_eq!(slack.cmp(&longer), std::cmp::Ordering::Less);
}

/// `Drop` must hand back the layout the allocation was made with, not the one its
/// contents imply. Miri is unavailable on this toolchain, so rather than trying to
/// observe the deallocation this locks the layout computation that both sides use:
/// a regression that reverts `Drop` to deriving the layout from the length would
/// make these differ.
#[test]
fn deallocation_layout_follows_capacity_not_length() {
    for (contents, capacity) in [
        (JsStr::latin1(b"abc"), 64usize),
        (JsStr::latin1(b"abc"), 3),
        (JsStr::utf16(&[0x41, 0x0100]), 32),
        (JsStr::utf16(&[0x41, 0x0100]), 2),
    ] {
        let string = JsString::with_capacity_from(contents, capacity);
        let observed = string.allocation_layout().expect("not static");
        let expected = RawJsString::layout(capacity, contents.is_latin1()).expect("valid layout");
        assert_eq!(observed, expected);

        // The length-derived layout is only the same one when there is no slack,
        // so this is what tells the two computations apart.
        let from_length = RawJsString::layout(contents.len(), contents.is_latin1()).unwrap();
        if capacity == contents.len() {
            assert_eq!(observed, from_length);
        } else {
            assert_ne!(observed, from_length);
        }
    }
}

/// Cloning shares the allocation, so the clone reports the same slack and the
/// contents survive the original being dropped.
#[test]
fn cloning_an_over_allocated_string_shares_its_allocation() {
    let original = JsString::with_capacity_from(JsStr::latin1(b"abc"), 64);
    let clone = original.clone();
    assert_eq!(original.refcount(), Some(2));
    assert_eq!(clone.capacity(), Some(64));

    drop(original);
    assert_eq!(clone.refcount(), Some(1));
    assert_eq!(clone.to_std_string_escaped(), "abc");
    assert_eq!(clone.capacity(), Some(64));
}

/// Concatenating from an over-allocated operand must not copy the slack in.
#[test]
fn concatenating_an_over_allocated_string_uses_only_its_contents() {
    let slack = JsString::with_capacity_from(JsStr::latin1(b"abc"), 64);
    let joined = JsString::concat(slack.as_str(), JsStr::latin1(b"de"));
    assert_eq!(joined.to_std_string_escaped(), "abcde");
    assert_eq!(joined.len(), 5);
    assert_eq!(joined.capacity(), Some(5));
}

/// The builder shrinks to fit before writing the header, which this change leaves
/// alone: its output stays exact.
#[test]
fn builder_output_is_still_exact() {
    let mut builder = Latin1JsStringBuilder::new();
    builder.extend_from_slice(b"abc");
    let built = builder.build().expect("latin1 contents build");
    assert_eq!(built.len(), 3);
    assert_eq!(built.capacity(), Some(3));
    assert_eq!(built, JsString::from("abc"));

    // The builder grows geometrically, so this one really did have slack to shed.
    let mut grown = Utf16JsStringBuilder::new();
    for i in 0..40u16 {
        grown.push(0x0100 + i);
    }
    let grown = grown.build();
    assert_eq!(grown.len(), 40);
    assert_eq!(grown.capacity(), Some(40));
}

/// A zero-length allocation with slack is still a valid empty string, and cannot
/// be confused with the static empty string.
#[test]
fn empty_contents_with_slack_stay_empty() {
    let empty = JsString::with_capacity_from(JsStr::latin1(b""), 16);
    assert_eq!(empty.len(), 0);
    assert_eq!(empty.capacity(), Some(16));
    assert!(!empty.is_static());
    assert_eq!(empty, JsString::default());
    assert_eq!(empty.to_std_string_escaped(), "");
}
