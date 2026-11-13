//! Byte vectors produced by the reference implementation Deluge 2.1 loads: `rencode` 1.0.9
//! (the Cython `_rencode`), `rencode.dumps(value).hex()` in Python 3.12.

use super::*;

fn hex(s: &str) -> Vec<u8> {
    hex::decode(s).expect("hex")
}

fn s(x: &str) -> Value {
    Value::Str(x.to_owned())
}

fn round_trip(v: &Value, expected_hex: &str) {
    let bytes = encode(v).expect("encodes");
    assert_eq!(hex::encode(&bytes), expected_hex, "encoding {v:?}");
    let back = decode(&bytes, Limits::default()).expect("decodes");
    assert_eq!(&back, v, "decoding {expected_hex}");
}

#[test]
fn integers_match_python() {
    let cases: &[(i64, &str)] = &[
        (0, "00"),
        (43, "2b"),
        (44, "3e2c"),
        (-1, "46"),
        (-32, "65"),
        (-33, "3edf"),
        (127, "3e7f"),
        (-128, "3e80"),
        (128, "3f0080"),
        (-129, "3fff7f"),
        (32767, "3f7fff"),
        (-32768, "3f8000"),
        (32768, "4000008000"),
        (2_147_483_647, "407fffffff"),
        (-2_147_483_648, "4080000000"),
        (2_147_483_648, "410000000080000000"),
        (i64::MAX, "417fffffffffffffff"),
        (i64::MIN, "418000000000000000"),
    ];
    for (i, h) in cases {
        round_trip(&Value::Int(*i), h);
    }
}

#[test]
fn big_integers_match_python() {
    round_trip(
        &Value::BigInt("9223372036854775808".into()),
        "3d393232333337323033363835343737353830387f",
    );
    round_trip(
        &Value::BigInt("-9223372036854775809".into()),
        "3d2d393232333337323033363835343737353830397f",
    );
    // A decimal integer that fits decodes as Int (Python's decoder accepts any length).
    assert_eq!(
        decode(&hex("3d3132337f"), Limits::default()),
        Ok(Value::Int(123))
    );
    assert_eq!(
        decode(&hex("3d2d317f"), Limits::default()),
        Ok(Value::Int(-1))
    );
    assert_eq!(decode(&hex("3d307f"), Limits::default()), Ok(Value::Int(0)));
    for bad in [
        "3d7f", "3d2d7f", "3d30317f", "3d2d307f", "3d31", "3d31617f", "3d2b317f",
    ] {
        assert!(decode(&hex(bad), Limits::default()).is_err(), "{bad}");
    }
    // 63 digits is the most; 64 is refused, as Python refuses it.
    let ok = format!("3d{}7f", "31".repeat(63));
    assert!(matches!(
        decode(&hex(&ok), Limits::default()),
        Ok(Value::BigInt(_))
    ));
    let too_long = format!("3d{}7f", "31".repeat(64));
    assert!(decode(&hex(&too_long), Limits::default()).is_err());
    assert!(encode(&Value::BigInt("1".repeat(64))).is_err());
    assert!(encode(&Value::BigInt("12x".into())).is_err());
}

#[test]
fn constants_match_python() {
    round_trip(&Value::Bool(true), "43");
    round_trip(&Value::Bool(false), "44");
    round_trip(&Value::None, "45");
}

#[test]
fn strings_match_python() {
    round_trip(&s(""), "80");
    round_trip(&s("foobar"), "86666f6f626172");
    round_trip(&s(&"a".repeat(63)), &format!("bf{}", "61".repeat(63)));
    round_trip(&s(&"a".repeat(64)), &format!("36343a{}", "61".repeat(64)));
    round_trip(
        &s(&"f".repeat(255)),
        &format!("3235353a{}", "66".repeat(255)),
    );
    round_trip(&s("héllo"), "8668c3a96c6c6f");
    round_trip(&Value::Bytes(vec![0xff, 0x00]), "82ff00");
    // Non-shortest forms the reference decoder accepts.
    assert_eq!(
        decode(&hex("353a6162636465"), Limits::default()),
        Ok(s("abcde"))
    );
    // Lengths that don't fit, or aren't digits.
    for bad in [
        "353a61",
        "35",
        "353561",
        "393939393939393939393939393939393a",
        "3a",
    ] {
        assert!(decode(&hex(bad), Limits::default()).is_err(), "{bad}");
    }
}

#[test]
fn floats() {
    // Python writes float32 by default; Deluge's replies carry these.
    assert_eq!(
        decode(&hex("423f000000"), Limits::default()),
        Ok(Value::Float(0.5))
    );
    let f = decode(&hex("42449a51ec"), Limits::default()).unwrap();
    assert_eq!(f, Value::Float(f64::from(1234.56_f32)));
    // We write float64, as `rencode.dumps(1234.56, 64)` does.
    round_trip(&Value::Float(1234.56), "2c40934a3d70a3d70a");
    assert!(decode(&hex("2c4093"), Limits::default()).is_err());
}

#[test]
fn lists_match_python() {
    round_trip(&Value::List(vec![]), "c0");
    round_trip(
        &Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)]),
        "c3010203",
    );
    round_trip(
        &Value::List(vec![
            Value::Int(1),
            Value::List(vec![s("a"), Value::None]),
            Value::List(vec![Value::Bool(true)]),
        ]),
        "c301c2816145c143",
    );
    let ints = |n: i64| Value::List((0..n).map(Value::Int).collect());
    let enc63 = hex::encode(encode(&ints(63)).unwrap());
    assert!(enc63.starts_with("ff000102"), "{enc63}");
    assert!(enc63.ends_with("3e3e"));
    round_trip(&ints(63), &enc63);
    let enc64 = hex::encode(encode(&ints(64)).unwrap());
    assert!(enc64.starts_with("3b000102"), "{enc64}");
    assert!(enc64.ends_with("3e3f7f"));
    round_trip(&ints(64), &enc64);
    // Variable-length form for a short list decodes too.
    assert_eq!(
        decode(&hex("3b0102037f"), Limits::default()),
        Ok(Value::List(vec![
            Value::Int(1),
            Value::Int(2),
            Value::Int(3)
        ]))
    );
    assert!(decode(&hex("3b0102"), Limits::default()).is_err());
    assert!(decode(&hex("c30102"), Limits::default()).is_err());
}

#[test]
fn dicts_match_python() {
    round_trip(&Value::Dict(vec![]), "66");
    round_trip(&Value::dict([("a", Value::Int(1))]), "67816101");
    let numbered = |n: i64| Value::dict((0..n).map(|i| (i.to_string(), Value::Int(i))));
    round_trip(
        &numbered(24),
        "7e8130008131018132028133038134048135058136068137078138088139098231300a8231310b8231320c\
         8231330d8231340e8231350f8231361082313711823138128231391382323014823231158232321682323317",
    );
    round_trip(
        &numbered(25),
        "3c8130008131018132028133038134048135058136068137078138088139098231300a8231310b8231320c\
         8231330d8231340e8231350f8231361082313711823138128231391382323014823231158232321682323317\
         823234187f",
    );
    assert_eq!(
        decode(&hex("3c8161017f"), Limits::default()),
        Ok(Value::dict([("a", Value::Int(1))]))
    );
    // A repeated key, a list as key, a missing value.
    assert!(decode(&hex("6881610181610102"), Limits::default()).is_err());
    assert!(decode(&hex("67c001"), Limits::default()).is_err());
    assert!(decode(&hex("678161"), Limits::default()).is_err());
    assert!(decode(&hex("3c8161"), Limits::default()).is_err());
}

#[test]
fn a_deluge_request_matches_python() {
    // ((0, "daemon.login", ("localclient", "pw"), {"client_version": "2.1.1"}),)
    let req = Value::List(vec![Value::List(vec![
        Value::Int(0),
        s("daemon.login"),
        Value::List(vec![s("localclient"), s("pw")]),
        Value::dict([("client_version", s("2.1.1"))]),
    ])]);
    round_trip(
        &req,
        "c1c4008c6461656d6f6e2e6c6f67696ec28b6c6f63616c636c69656e74827077678e636c69656e745f76657273696f6e85322e312e31",
    );
}

#[test]
fn rejects_bad_input() {
    for bad in [
        "",     // nothing
        "7f",   // a terminator alone
        "2d",   // '-': unused
        "2e",   // '.': unused
        "2f",   // '/': unused
        "30",   // '0': the reference decoder has no zero-length long string
        "3a",   // ':'
        "0000", // trailing bytes
        "3e",   // INT1 without its byte
        "3f00", // INT2 short
        "4000", // INT4 short
        "41",   // INT8 short
        "4200", // FLOAT32 short
    ] {
        assert!(decode(&hex(bad), Limits::default()).is_err(), "{bad}");
    }
}

#[test]
fn depth_is_bounded() {
    let limits = Limits { max_depth: 3 };
    assert!(decode(&hex("c1c1c100"), limits).is_ok());
    assert!(decode(&hex("c1c1c1c100"), limits).is_err());
    // Deep nesting of every container form is refused without blowing the stack.
    let deep = vec![0xC1; 1_000_000];
    assert!(decode(&deep, Limits::default()).is_err());
    let deep = vec![CHR_LIST; 1_000_000];
    assert!(decode(&deep, Limits::default()).is_err());
    let deep = vec![DICT_FIXED_START + 1; 1_000_000];
    assert!(decode(&deep, Limits::default()).is_err());
}

#[test]
fn huge_claims_allocate_nothing() {
    // A string claiming 4 GB, in 12 bytes of input.
    assert!(decode(b"4294967295:a", Limits::default()).is_err());
    // A string length with more digits than fit a usize.
    assert!(decode(b"99999999999999999999999999:a", Limits::default()).is_err());
}

#[test]
fn helpers() {
    let v = Value::dict([("a", Value::Int(1)), ("b", s("x"))]);
    assert_eq!(v.get("a").and_then(Value::as_i64), Some(1));
    assert_eq!(v.get("b").and_then(Value::as_str), Some("x"));
    assert_eq!(v.get("c"), None);
    assert_eq!(Value::Int(2).as_f64(), Some(2.0));
    assert_eq!(Value::str_list(["a"]), Value::List(vec![s("a")]));
}
