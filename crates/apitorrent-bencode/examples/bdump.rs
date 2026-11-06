//! Print a bencoded file in a readable form: `cargo run --example bdump -- file.torrent`.

fn main() {
    let path = std::env::args().nth(1).expect("usage: bdump <file>");
    let data = std::fs::read(&path).expect("read file");
    match apitorrent_bencode::decode_with(&data, &apitorrent_bencode::DecodeOptions::lenient()) {
        Ok(v) => println!("{v:#?}"),
        Err(e) => eprintln!("{path}: {e}"),
    }
}
