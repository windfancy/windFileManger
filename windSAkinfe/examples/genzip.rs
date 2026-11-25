use windsakinfe::archive::zip::ZipWriter;
fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| "target/enc.zip".into());
    let f = std::fs::File::create(&out).unwrap();
    let mut w = ZipWriter::new(f, 6, Some(b"pw123"));
    w.add_file("x.txt", b"encrypted by our own zipcrypto", None).unwrap();
    w.finish().unwrap();
    println!("wrote {out}");
}
