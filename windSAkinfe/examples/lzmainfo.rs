use windsakinfe::algo::hash::{hash_hex, Algo};
use windsakinfe::codec::lzma;
fn main() {
    let p = std::env::args().nth(1).unwrap();
    let data = std::fs::read(&p).unwrap();
    let r = if p.ends_with(".lz2") {
        lzma::lzma2_decompress(&data, None)
    } else {
        lzma::lzma_alone_decompress(&data)
    };
    match r {
        Ok(out) => println!("ok {} bytes sha256 {}", out.len(), hash_hex(Algo::Sha256, &out)),
        Err(e) => println!("ERR {e}"),
    }
}
