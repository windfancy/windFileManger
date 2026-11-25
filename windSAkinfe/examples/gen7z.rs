use windsakinfe::archive::sevenz::{write_7z, SevenZInput};
fn main() {
    let entries = vec![
        SevenZInput { name: "a.txt".into(), data: b"hello 7z writer hello 7z writer".to_vec(), is_dir: false, mtime_filetime: None },
        SevenZInput { name: "d".into(), data: Vec::new(), is_dir: true, mtime_filetime: None },
        SevenZInput { name: "b.bin".into(), data: (0..3000u32).map(|i| (i % 251) as u8).collect(), is_dir: false, mtime_filetime: None },
    ];
    let f = std::fs::File::create("target/our.7z").unwrap();
    let mut w = std::io::BufWriter::new(f);
    write_7z(&mut w, &entries, 6).unwrap();
    println!("ok");
}
