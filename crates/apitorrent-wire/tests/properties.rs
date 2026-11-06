//! The decoders must never panic on peer input, however it is split across reads.

use apitorrent_wire::extended::{ExtensionHandshake, MetadataMessage, PexMessage};
use apitorrent_wire::{Decoder, Handshake};
use bytes::Bytes;
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(3_000))]

    #[test]
    fn message_decoder_never_panics(data in proptest::collection::vec(any::<u8>(), 0..512), split in 0usize..512) {
        let mut d = Decoder::new(256);
        let split = split.min(data.len());
        for chunk in [&data[..split], &data[split..]] {
            d.extend(chunk);
            while let Ok(Some(_)) = d.next() {}
        }
    }

    #[test]
    fn payload_decoders_never_panic(data in proptest::collection::vec(any::<u8>(), 0..256)) {
        let _ = Handshake::decode(&data);
        let _ = ExtensionHandshake::decode(&data);
        let _ = MetadataMessage::decode(&Bytes::from(data.clone()));
        let _ = PexMessage::decode(&data);
    }
}
