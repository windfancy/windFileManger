use windsakinfe::codec::lzma_enc;
fn main() {
    let inp = std::env::args().nth(1).unwrap();
    let outp = std::env::args().nth(2).unwrap();
    let data = std::fs::read(&inp).unwrap();
    let t = std::time::Instant::now();
    let packed = lzma_enc::lzma2_compress(&data, 6, None);
    let dt = t.elapsed();
    std::fs::write(&outp, &packed).unwrap();
    let ok = lzma_enc::lzma2_roundtrip_ok(&data, &packed);
    println!("in={} out={} ratio={:.3} self_roundtrip={} {:.0}ms", data.len(), packed.len(),
             packed.len() as f64 / data.len() as f64, ok, dt.as_secs_f64()*1000.0);
}
