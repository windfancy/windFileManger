use windsakinfe::codec::lzma;
use windsakinfe::codec::lzma_enc;
fn main() {
    for n in [0usize, 1, 5, 10, 50, 200, 1000] {
        let data: Vec<u8> = (0..n).map(|i| b'a' + (i % 3) as u8).collect();
        let packed = lzma_enc::lzma_alone_compress(&data, 6);
        match lzma::lzma_alone_decompress(&packed) {
            Ok(back) => {
                if back != data {
                    println!("n={n} MISMATCH got {} bytes: {:?}", back.len(), &back[..back.len().min(20)]);
                } else {
                    println!("n={n} ok (packed {})", packed.len());
                }
            }
            Err(e) => println!("n={n} ERR {e}"),
        }
    }
}
