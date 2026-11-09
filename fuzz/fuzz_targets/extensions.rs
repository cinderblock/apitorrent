//! BEP 10 extension payloads: the extended handshake, ut_metadata and ut_pex.
#![no_main]
use apitorrent_wire::extended::{ExtensionHandshake, MetadataMessage, PexMessage};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = ExtensionHandshake::decode(data);
    let _ = MetadataMessage::decode(&bytes::Bytes::copy_from_slice(data));
    let _ = PexMessage::decode(data);
    let _ = apitorrent_wire::compact::parse_v4(data);
    let _ = apitorrent_wire::compact::parse_v6(data);
});
