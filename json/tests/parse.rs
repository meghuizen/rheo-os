//! Host correctness tests for rheo-json (`cargo test -p rheo-json`). Covers
//! literals, numbers, strings/escapes/unicode, nesting, whitespace, the
//! zero-copy borrow, and the error surface.

use rheo_json::{ErrorKind, Number, Value, parse};

#[test]
fn literals() {
    assert!(parse("null").unwrap().is_null());
    assert_eq!(parse("true").unwrap().as_bool(), Some(true));
    assert_eq!(parse("false").unwrap().as_bool(), Some(false));
    assert_eq!(parse("  \n\ttrue\r\n ").unwrap().as_bool(), Some(true));
}

#[test]
fn numbers() {
    assert_eq!(parse("0").unwrap().as_u64(), Some(0));
    assert_eq!(parse("42").unwrap().as_u64(), Some(42));
    assert_eq!(parse("-7").unwrap().as_i64(), Some(-7));
    assert_eq!(parse("3.5").unwrap().as_f64(), Some(3.5));
    assert_eq!(parse("1e3").unwrap().as_f64(), Some(1000.0));
    assert_eq!(parse("-2.5E-1").unwrap().as_f64(), Some(-0.25));
    assert_eq!(
        parse("18446744073709551615").unwrap().as_number(),
        Some(&Number::Unsigned(u64::MAX))
    );
    // Too big for u64 -> float.
    assert!(matches!(
        parse("99999999999999999999999").unwrap().as_number(),
        Some(Number::Float(_))
    ));
}

#[test]
fn bad_numbers() {
    for s in ["01", "-", "1.", "1e", "1e+", ".5", "+1", "1..2"] {
        assert!(parse(s).is_err(), "{s:?} should be rejected");
    }
}

#[test]
fn strings_and_escapes() {
    assert_eq!(parse(r#""hi""#).unwrap().as_str(), Some("hi"));
    assert_eq!(
        parse(r#""a\n\t\\\"/b""#).unwrap().as_str(),
        Some("a\n\t\\\"/b")
    );
    assert_eq!(parse(r#""Aé""#).unwrap().as_str(), Some("Aé"));
    // Surrogate pair for U+1D11E (musical G clef).
    assert_eq!(parse(r#""𝄞""#).unwrap().as_str(), Some("\u{1D11E}"));
    // Raw multi-byte UTF-8 passes through.
    assert_eq!(parse("\"héllo→\"").unwrap().as_str(), Some("héllo→"));
}

#[test]
fn bad_strings() {
    assert_eq!(parse(r#""\x""#).unwrap_err().kind, ErrorKind::String);
    assert_eq!(parse("\"\u{1}\"").unwrap_err().kind, ErrorKind::String); // raw control
    assert_eq!(parse(r#""\uD834""#).unwrap_err().kind, ErrorKind::Unicode); // lone surrogate
    assert_eq!(parse(r#""abc"#).unwrap_err().kind, ErrorKind::Eof); // unterminated
}

#[test]
fn zero_copy_borrow() {
    // A string with no escapes must borrow, not allocate.
    let src = String::from(r#""borrowed""#);
    let v = parse(&src).unwrap();
    match v {
        Value::String(std::borrow::Cow::Borrowed(s)) => assert_eq!(s, "borrowed"),
        other => panic!("expected a borrowed string, got {other:?}"),
    }
    // An escaped string owns.
    let v = parse(r#""a\nb""#).unwrap();
    assert!(matches!(v, Value::String(std::borrow::Cow::Owned(_))));
}

#[test]
fn arrays_and_objects() {
    let v = parse(r#"[1, 2, 3]"#).unwrap();
    assert_eq!(v.at(0).unwrap().as_u64(), Some(1));
    assert_eq!(v.at(2).unwrap().as_u64(), Some(3));
    assert!(v.at(3).is_none());
    assert_eq!(parse("[]").unwrap().as_array().unwrap().len(), 0);
    assert_eq!(parse("{}").unwrap(), Value::Object(vec![]));

    let doc = r#"
      { "name": "rheo", "ok": true, "n": 3,
        "nested": { "xs": [10, 20], "s": "x\ny" }, "z": null }
    "#;
    let v = parse(doc).unwrap();
    assert_eq!(v.get("name").unwrap().as_str(), Some("rheo"));
    assert_eq!(v.get("ok").unwrap().as_bool(), Some(true));
    assert_eq!(v.get("n").unwrap().as_u64(), Some(3));
    assert!(v.get("z").unwrap().is_null());
    let nested = v.get("nested").unwrap();
    assert_eq!(nested.get("xs").unwrap().at(1).unwrap().as_u64(), Some(20));
    assert_eq!(nested.get("s").unwrap().as_str(), Some("x\ny"));
    assert!(v.get("missing").is_none());
}

#[test]
fn structural_errors() {
    assert_eq!(parse("").unwrap_err().kind, ErrorKind::Eof);
    assert_eq!(parse("[1,]").unwrap_err().kind, ErrorKind::Unexpected);
    assert_eq!(parse("[1 2]").unwrap_err().kind, ErrorKind::Unexpected);
    assert_eq!(
        parse(r#"{"a":1,}"#).unwrap_err().kind,
        ErrorKind::Unexpected
    );
    assert_eq!(parse(r#"{"a" 1}"#).unwrap_err().kind, ErrorKind::Unexpected);
    assert_eq!(parse("truer").unwrap_err().kind, ErrorKind::Trailing);
    assert_eq!(parse("[1,2] extra").unwrap_err().kind, ErrorKind::Trailing);
}

#[test]
fn depth_limit() {
    let deep = "[".repeat(1000) + &"]".repeat(1000);
    assert_eq!(parse(&deep).unwrap_err().kind, ErrorKind::Depth);
    // Just under the limit parses.
    let ok = "[".repeat(200) + &"]".repeat(200);
    assert!(parse(&ok).is_ok());
}

#[test]
fn realistic_document() {
    let doc = r#"{
      "id": 1234567890,
      "active": true,
      "ratio": 0.7501,
      "tags": ["a", "b", "c"],
      "meta": {"unicode": "café ☕", "empty": {}, "list": [true, false, null]}
    }"#;
    let v = parse(doc).unwrap();
    assert_eq!(v.get("id").unwrap().as_u64(), Some(1234567890));
    assert_eq!(v.get("ratio").unwrap().as_f64(), Some(0.7501));
    assert_eq!(v.get("tags").unwrap().as_array().unwrap().len(), 3);
    assert_eq!(
        v.get("meta").unwrap().get("unicode").unwrap().as_str(),
        Some("café ☕")
    );
}
