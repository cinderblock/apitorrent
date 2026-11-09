//! A peer's byte stream: the handshake, then messages, fed in arbitrary chunks.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = apitorrent_wire::Handshake::decode(data);
    let Some((&split, rest)) = data.split_first() else {
        return;
    };
    let mut d = apitorrent_wire::Decoder::new(1 << 20);
    let chunk = usize::from(split).max(1);
    for part in rest.chunks(chunk) {
        d.extend(part);
        loop {
            match d.next() {
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
    }
});
