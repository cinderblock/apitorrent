//! Property tests: the decoder never panics on arbitrary input, and canonical documents
//! survive a decode/encode round trip byte for byte.

use std::collections::BTreeMap;

use apitorrent_bencode::{DecodeOptions, OwnedValue, decode, decode_with};
use proptest::prelude::*;

fn owned_value() -> impl Strategy<Value = OwnedValue> {
    let leaf = prop_oneof![
        any::<i64>().prop_map(OwnedValue::Int),
        proptest::collection::vec(any::<u8>(), 0..24).prop_map(OwnedValue::Bytes),
    ];
    leaf.prop_recursive(6, 64, 8, |inner| {
        prop_oneof![
            proptest::collection::vec(inner.clone(), 0..8).prop_map(OwnedValue::List),
            proptest::collection::btree_map(
                proptest::collection::vec(any::<u8>(), 0..12),
                inner,
                0..8
            )
            .prop_map(|m: BTreeMap<Vec<u8>, OwnedValue>| OwnedValue::Dict(m)),
        ]
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2_000))]

    #[test]
    fn canonical_round_trip(v in owned_value()) {
        let bytes = v.to_bytes();
        let decoded = decode(&bytes).expect("canonical encoding must decode");
        prop_assert_eq!(decoded.to_owned_value(), v);
        prop_assert_eq!(decoded.encode(), bytes);
    }

    #[test]
    fn arbitrary_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..256)) {
        let _ = decode(&bytes);
        let _ = decode_with(&bytes, &DecodeOptions::lenient());
    }

    #[test]
    fn mutated_documents_never_panic(v in owned_value(), flips in proptest::collection::vec((any::<usize>(), any::<u8>()), 1..4)) {
        let mut bytes = v.to_bytes();
        for (i, b) in flips {
            if !bytes.is_empty() {
                let n = bytes.len();
                bytes[i % n] = b;
            }
        }
        if let Ok(decoded) = decode_with(&bytes, &DecodeOptions::lenient()) {
            // Whatever decodes must re-encode and decode again to the same value.
            let again = decoded.encode();
            prop_assert_eq!(decode(&again).unwrap().to_owned_value(), decoded.to_owned_value());
        }
    }
}
