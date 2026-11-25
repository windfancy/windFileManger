//! 与本机 7-Zip 的双向互操作验证。
//!
//! 注意：产品代码零外部依赖（需求 C-2）。本测试文件是**唯一**允许调用外部
//! 压缩程序的地方，目的只是交叉验证自研实现是否符合公开格式规范。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use windsakinfe::archive::zip::{ZipArchive, ZipWriter};

fn seven_zip() -> Option<PathBuf> {
    let candidates = [
        r"C:\Program Files\7-Zip\7z.exe",
        r"C:\Program Files (x86)\7-Zip\7z.exe",
        "/usr/bin/7z",
        "/usr/local/bin/7z",
        "/opt/homebrew/bin/7z",
    ];
    candidates.iter().map(PathBuf::from).find(|p| p.exists())
}

fn tmpdir(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!(
        "wsak_it_{}_{}_{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&base).unwrap();
    base
}

fn sha256_file(p: &Path) -> String {
    use windsakinfe::algo::hash::{hash_file, Algo};
    windsakinfe::util::hex(&hash_file(Algo::Sha256, p).unwrap())
}

/// 自研 writer 产出 → 7-Zip 解压 → 内容一致
#[test]
fn our_zip_read_by_7zip() {
    let Some(sz) = seven_zip() else {
        eprintln!("跳过：本机无 7z，无法做互操作验证");
        return;
    };
    let dir = tmpdir("our2sz");
    let src = dir.join("src");
    fs::create_dir_all(src.join("sub")).unwrap();
    let text = b"The quick brown fox jumps over the lazy dog. ".repeat(200);
    let bin: Vec<u8> = (0..50_000u32).map(|i| (i * 37 % 256) as u8).collect();
    fs::write(src.join("a.txt"), &text).unwrap();
    fs::write(src.join("sub/b.bin"), &bin).unwrap();
    fs::write(src.join("中文名.txt"), "中文内容测试\n".as_bytes()).unwrap();

    let zip_path = dir.join("out.zip");
    {
        let f = fs::File::create(&zip_path).unwrap();
        let mut w = ZipWriter::new(f, 6, None);
        w.add_file("a.txt", &text, None).unwrap();
        w.add_file("sub/b.bin", &bin, None).unwrap();
        w.add_file("中文名.txt", "中文内容测试\n".as_bytes(), None).unwrap();
        w.add_dir("emptydir", None).unwrap();
        w.finish().unwrap();
    }

    let out = dir.join("unpacked");
    let st = Command::new(&sz)
        .arg("x")
        .arg(&zip_path)
        .arg(format!("-o{}", out.display()))
        .arg("-y")
        .output()
        .unwrap();
    assert!(
        st.status.success(),
        "7-Zip 解压失败：{}",
        String::from_utf8_lossy(&st.stdout)
    );
    assert_eq!(fs::read(out.join("a.txt")).unwrap(), text);
    assert_eq!(fs::read(out.join("sub/b.bin")).unwrap(), bin);
    assert_eq!(
        fs::read_to_string(out.join("中文名.txt")).unwrap(),
        "中文内容测试\n"
    );
    assert!(out.join("emptydir").is_dir());

    // 7z 的完整性测试（校验 CRC）
    let t = Command::new(&sz).arg("t").arg(&zip_path).output().unwrap();
    assert!(t.status.success(), "7z t 报告损坏：{}", String::from_utf8_lossy(&t.stdout));

    fs::remove_dir_all(&dir).ok();
}

/// 7-Zip 产出 → 自研 reader 解码 → 内容一致
#[test]
fn sz_zip_read_by_us() {
    let Some(sz) = seven_zip() else {
        eprintln!("跳过：本机无 7z");
        return;
    };
    let dir = tmpdir("sz2our");
    let src = dir.join("src");
    fs::create_dir_all(src.join("sub")).unwrap();
    let text = "hello 7zip deflate interoperability\n".repeat(500);
    let bin: Vec<u8> = (0..80_000u32).map(|i| (i * 13 % 251) as u8).collect();
    fs::write(src.join("a.txt"), &text).unwrap();
    fs::write(src.join("sub/b.bin"), &bin).unwrap();

    let zip_path = dir.join("sz.zip");
    let st = Command::new(&sz)
        .arg("a")
        .arg("-tzip")
        .arg("-mx=6")
        .arg(&zip_path)
        .arg(src.join("a.txt"))
        .arg(src.join("sub/b.bin"))
        .output()
        .unwrap();
    assert!(st.status.success(), "{}", String::from_utf8_lossy(&st.stdout));

    let data = fs::read(&zip_path).unwrap();
    let ar = ZipArchive::open(data).unwrap();
    assert_eq!(ar.entries().len(), 2);
    let mut got_text = None;
    let mut got_bin = None;
    for i in 0..ar.entries().len() {
        let name = ar.entries()[i].name.clone();
        let content = ar.read(i, None).unwrap();
        if name.ends_with("a.txt") {
            got_text = Some(content);
        } else if name.ends_with("b.bin") {
            got_bin = Some(content);
        }
    }
    assert_eq!(String::from_utf8(got_text.unwrap()).unwrap(), text);
    assert_eq!(got_bin.unwrap(), bin);

    fs::remove_dir_all(&dir).ok();
}

/// 7-Zip 加密 zip → 自研 ZipCrypto 解密
#[test]
fn sz_encrypted_zip_read_by_us() {
    let Some(sz) = seven_zip() else {
        eprintln!("跳过：本机无 7z");
        return;
    };
    let dir = tmpdir("zipsecret");
    let src = dir.join("s.txt");
    fs::write(&src, "secret payload 秘密内容").unwrap();
    let zip_path = dir.join("enc.zip");
    let st = Command::new(&sz)
        .arg("a")
        .arg("-tzip")
        .arg("-mem=ZipCrypto")
        .arg("-pP@ssw0rd")
        .arg(&zip_path)
        .arg(&src)
        .output()
        .unwrap();
    assert!(st.status.success(), "{}", String::from_utf8_lossy(&st.stdout));

    let ar = ZipArchive::open(fs::read(&zip_path).unwrap()).unwrap();
    let idx = (0..ar.entries().len())
        .find(|&i| ar.entries()[i].name.ends_with("s.txt"))
        .unwrap();
    assert_eq!(
        ar.read(idx, Some(b"P@ssw0rd")).unwrap(),
        "secret payload 秘密内容".as_bytes()
    );
    assert!(ar.read(idx, Some(b"bad")).is_err());
    fs::remove_dir_all(&dir).ok();
}

/// 我们的 zip 加密 → 7-Zip 能解密（验证 ZipCrypto 实现正确）
#[test]
fn our_encrypted_zip_read_by_7zip() {
    let Some(sz) = seven_zip() else {
        eprintln!("跳过：本机无 7z");
        return;
    };
    let dir = tmpdir("oursecret");
    let zip_path = dir.join("ours_enc.zip");
    {
        let f = fs::File::create(&zip_path).unwrap();
        let mut w = ZipWriter::new(f, 6, Some(b"pw123"));
        w.add_file("x.txt", b"encrypted by our own zipcrypto", None).unwrap();
        w.finish().unwrap();
    }
    let out = dir.join("out");
    let st = Command::new(&sz)
        .arg("x")
        .arg(&zip_path)
        .arg(format!("-o{}", out.display()))
        .arg("-ppw123")
        .arg("-y")
        .output()
        .unwrap();
    assert!(
        st.status.success(),
        "7-Zip 无法解开我们加密的 zip：{}",
        String::from_utf8_lossy(&st.stdout)
    );
    assert_eq!(
        fs::read_to_string(out.join("x.txt")).unwrap(),
        "encrypted by our own zipcrypto"
    );
    fs::remove_dir_all(&dir).ok();
}

/// 大文件（跨多个 DEFLATE 块）互操作
#[test]
fn large_zip_interop() {
    let Some(sz) = seven_zip() else {
        eprintln!("跳过：本机无 7z");
        return;
    };
    let dir = tmpdir("large");
    let mut data = Vec::with_capacity(3_000_000);
    let mut x = 0x2545_F491_4F6C_DD1Du64;
    while data.len() < 3_000_000 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        // 混合随机与重复，保证既有字面量也有匹配
        if x % 3 == 0 {
            data.extend_from_slice(b"repeated-block-");
        } else {
            data.push((x & 0xFF) as u8);
        }
    }
    let zip_path = dir.join("big.zip");
    {
        let f = fs::File::create(&zip_path).unwrap();
        let mut w = ZipWriter::new(f, 6, None);
        w.add_file("big.bin", &data, None).unwrap();
        w.finish().unwrap();
    }
    let out = dir.join("o");
    let st = Command::new(&sz)
        .arg("x")
        .arg(&zip_path)
        .arg(format!("-o{}", out.display()))
        .arg("-y")
        .output()
        .unwrap();
    assert!(st.status.success());
    assert_eq!(fs::read(out.join("big.bin")).unwrap(), data);

    // 反向：7z 压大文件，我们解
    let zip2 = dir.join("big2.zip");
    let st = Command::new(&sz)
        .arg("a")
        .arg("-tzip")
        .arg("-mx=9")
        .arg(&zip2)
        .arg(zip_path.parent().unwrap().join("big.bin"))
        .output();
    let _ = st; // 源文件在 zip 里，需要先解出工作副本
    fs::write(dir.join("work.bin"), &data).unwrap();
    let _ = fs::remove_file(&zip2);
    let st = Command::new(&sz)
        .arg("a")
        .arg("-tzip")
        .arg("-mx=9")
        .arg(&zip2)
        .arg(dir.join("work.bin"))
        .output()
        .unwrap();
    assert!(st.status.success());
    let ar = ZipArchive::open(fs::read(&zip2).unwrap()).unwrap();
    assert_eq!(ar.read(0, None).unwrap(), data);
    assert!(sha256_file(&zip_path).len() == 64);
    fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------- 7z 互操作

use windsakinfe::archive::sevenz::{write_7z, SevenZArchive, SevenZInput};

#[test]
fn our_7z_read_by_7zip() {
    let Some(sz) = seven_zip() else { return };
    let dir = tmpdir("our7z");
    let text = "7z writer test line\n".repeat(500);
    let bin: Vec<u8> = (0..40_000u32).map(|i| (i * 17 % 256) as u8).collect();
    let entries = vec![
        SevenZInput { name: "a.txt".into(), data: text.clone().into_bytes(), is_dir: false, mtime_filetime: None },
        SevenZInput { name: "sub/b.bin".into(), data: bin.clone(), is_dir: false, mtime_filetime: None },
        SevenZInput { name: "空目录".into(), data: Vec::new(), is_dir: true, mtime_filetime: None },
    ];
    let p = dir.join("our.7z");
    {
        let f = fs::File::create(&p).unwrap();
        let mut w = std::io::BufWriter::new(f);
        write_7z(&mut w, &entries, 6).unwrap();
    }
    let out = dir.join("o");
    let st = Command::new(&sz)
        .arg("x").arg(&p).arg(format!("-o{}", out.display())).arg("-y").output().unwrap();
    assert!(st.status.success(), "7z 解压我们写的 7z 失败：{}", String::from_utf8_lossy(&st.stdout));
    assert_eq!(fs::read_to_string(out.join("a.txt")).unwrap(), text);
    assert_eq!(fs::read(out.join("sub/b.bin")).unwrap(), bin);
    assert!(out.join("空目录").is_dir());
    let t = Command::new(&sz).arg("t").arg(&p).output().unwrap();
    assert!(t.status.success(), "7z t 报错：{}", String::from_utf8_lossy(&t.stdout));
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn sz_7z_read_by_us() {
    let Some(sz) = seven_zip() else { return };
    let dir = tmpdir("sz7z");
    let src = dir.join("src");
    fs::create_dir_all(src.join("sub")).unwrap();
    let text = "interop 7z read by our own decoder\n".repeat(400);
    let bin: Vec<u8> = (0..90_000u32).map(|i| (i * 29 % 251) as u8).collect();
    fs::write(src.join("a.txt"), &text).unwrap();
    fs::write(src.join("sub/b.bin"), &bin).unwrap();
    let p = dir.join("sz.7z");
    let st = Command::new(&sz)
        .arg("a").arg("-t7z").arg("-mx=5").arg(&p)
        .arg(src.join("a.txt")).arg(src.join("sub/b.bin"))
        .output().unwrap();
    assert!(st.status.success(), "{}", String::from_utf8_lossy(&st.stdout));

    let ar = SevenZArchive::open(fs::read(&p).unwrap()).unwrap();
    let all = ar.extract_all().unwrap();
    let mut got_text = None;
    let mut got_bin = None;
    for (i, e) in ar.entries.iter().enumerate() {
        if e.name.ends_with("a.txt") { got_text = Some(all[i].clone()); }
        if e.name.ends_with("b.bin") { got_bin = Some(all[i].clone()); }
    }
    assert_eq!(String::from_utf8(got_text.unwrap()).unwrap(), text);
    assert_eq!(got_bin.unwrap(), bin);
    fs::remove_dir_all(&dir).ok();
}
