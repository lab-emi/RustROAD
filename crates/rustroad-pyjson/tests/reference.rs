//! Byte identity with CPython's `json` module on outputs it wrote
//! (`tests/fixtures/generate.py`).
use rustroad_pyjson::{
    Duplicates, Layout, Value, dumps, float_repr, parse, parse_prefix, parse_with,
};
use std::path::PathBuf;

fn fixture(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    std::fs::read_to_string(path).unwrap()
}

#[test]
fn float_repr_equals_cpython_on_reference_doubles() {
    let text = fixture("floats.txt");
    let mut count = 0;
    for line in text.lines() {
        let (bits, expected) = line.split_once(' ').unwrap();
        let value = f64::from_bits(u64::from_str_radix(bits, 16).unwrap());
        assert_eq!(float_repr(value).as_deref(), Some(expected), "{bits}");
        // The decoder side: the text parses back to the same double.
        match parse(expected).unwrap() {
            Value::Float(parsed) => assert_eq!(parsed.to_bits(), value.to_bits(), "{bits}"),
            other => panic!("{expected} decoded as {other:?}"),
        }
        count += 1;
    }
    assert!(count > 1000);
}

#[test]
fn documents_render_as_cpython_renders_them() {
    for index in 0..5 {
        let value = parse(&fixture(&format!("doc{index}.src.json"))).unwrap();
        for (name, layout) in [
            ("line", Layout::Line),
            ("indent2", Layout::Indent(2)),
            ("indent1", Layout::Indent(1)),
            ("compact", Layout::Compact),
            ("lineutf8", Layout::LineUtf8),
        ] {
            assert_eq!(
                dumps(&value, layout).unwrap(),
                fixture(&format!("doc{index}.{name}.json")),
                "doc{index} {name}"
            );
        }
    }
}

#[test]
fn equality_and_duplicates_follow_python() {
    let a = parse(r#"{"x": 1, "y": [1.0, true], "z": {"k": null}}"#).unwrap();
    let b = parse(r#"{"z": {"k": null}, "y": [1, 1], "x": 1.0}"#).unwrap();
    assert_eq!(a, b);
    assert_ne!(a, parse(r#"{"x": 1, "y": [1.0, true]}"#).unwrap());
    assert_ne!(parse("[1, 2]").unwrap(), parse("[2, 1]").unwrap());
    assert_ne!(parse("1").unwrap(), parse("\"1\"").unwrap());
    assert_ne!(parse("1.5").unwrap(), parse("1").unwrap());
    assert_eq!(
        parse_with(r#"{"a": 1, "a": 2}"#, Duplicates::Reject),
        Err(rustroad_pyjson::ParseError::DuplicateKey)
    );
    // Forty keys cross the indexed path; the repeated key keeps its first slot.
    let mut text = String::from("{");
    for index in 0..40 {
        text.push_str(&format!("\"k{index}\": {index}, "));
    }
    text.push_str("\"k3\": -1}");
    let value = parse(&text).unwrap();
    let dict = value.as_dict().unwrap();
    assert_eq!(dict.len(), 40);
    assert_eq!(dict.keys().nth(3), Some("k3"));
    assert_eq!(dict.get("k3"), Some(&Value::Int(-1)));
    assert!(parse_with(&text, Duplicates::Reject).is_err());
}

#[test]
fn malformed_text_is_rejected() {
    for text in [
        "",
        "{",
        "[1,",
        "[1 2]",
        "{\"a\" 1}",
        "{\"a\":}",
        "01",
        "1.",
        "-",
        "+1",
        "1e",
        "nul",
        "\"abc",
        "\"\\x\"",
        "\"\u{1}\"",
        "[1]]",
        "{} {}",
        "NaN",
        "Infinity",
        "\"\\ud800\"",
        "{\"a\":1,}",
        "[,]",
        "'a'",
    ] {
        assert!(parse(text).is_err(), "{text:?}");
    }
    let deep = "[".repeat(300) + &"]".repeat(300);
    assert_eq!(parse(&deep), Err(rustroad_pyjson::ParseError::Depth));
    assert!(matches!(
        parse("123456789012345678901234567890123456789012345678901234567890"),
        Err(rustroad_pyjson::ParseError::IntegerRange)
    ));
}

#[test]
fn prefix_decoding_stops_after_one_value() {
    let text = r#"{"format_version":6,"revision":"rev41","rest":[1,2"#;
    let (key, end) = parse_prefix(text, 1, Duplicates::Reject).unwrap();
    assert_eq!(key, Value::Str("format_version".into()));
    assert_eq!(&text[end..=end], ":");
    let (value, end) = parse_prefix(text, end + 1, Duplicates::Reject).unwrap();
    assert_eq!(value, Value::Int(6));
    assert_eq!(&text[end..=end], ",");
    let open = text.find('[').unwrap();
    assert_eq!(
        parse_prefix(text, open, Duplicates::Reject),
        Err(rustroad_pyjson::ParseError::Truncated)
    );
}

#[test]
fn pure_path_matches_pathlib() {
    for (input, expected) in [
        ("/a/b", "/a/b"),
        ("/a//b/", "/a/b"),
        ("/a/./b/.", "/a/b"),
        ("a/../b", "a/../b"),
        ("//a/b", "//a/b"),
        ("///a/b", "/a/b"),
        ("", "."),
        (".", "."),
        ("./a", "a"),
        ("/", "/"),
    ] {
        assert_eq!(rustroad_pyjson::pure_path(input), expected, "{input}");
    }
}

#[test]
fn file_identity_hashes_and_bounds() {
    let dir = std::env::temp_dir().join(format!("rustroad-pyjson-id-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("abc.txt");
    std::fs::write(&path, b"abc").unwrap();
    let id = rustroad_pyjson::FileId::of(&path, 16).unwrap();
    assert_eq!(
        id.sha256,
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(id.bytes, 3);
    assert_eq!(
        dumps(&id.to_value(), Layout::Line).unwrap(),
        format!(
            "{{\"path\": \"{}\", \"sha256\": \"{}\", \"bytes\": 3}}",
            id.path, id.sha256
        )
    );
    assert_eq!(
        rustroad_pyjson::FileId::from_value(&id.to_value()),
        Some(id)
    );
    assert!(rustroad_pyjson::FileId::of(&path, 2).is_err());
    assert!(rustroad_pyjson::FileId::of(&dir, 16).is_err());
    std::fs::remove_dir_all(&dir).unwrap();
}
