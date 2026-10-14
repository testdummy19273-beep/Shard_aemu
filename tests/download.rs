//! Checksums and the "already downloaded" check. No network.

use shard::sdk::download::{digest, file_ok, hex, matches, part_path, unhex};
use shard::sdk::manifest::{Checksum, ChecksumKind};

fn sha1(hex: &str) -> Checksum {
    Checksum {
        kind: ChecksumKind::Sha1,
        hex: hex.into(),
    }
}

#[test]
fn sha1_good_and_bad() {
    // sha1("abc")
    let good = sha1("a9993e364706816aba3e25717850c26c9cd0d89d");
    assert!(matches(&good, &b"abc"[..]).unwrap());
    assert!(!matches(&good, &b"abd"[..]).unwrap());
    // Upper-case hex in a manifest still matches.
    let upper = sha1("A9993E364706816ABA3E25717850C26C9CD0D89D");
    assert!(matches(&upper, &b"abc"[..]).unwrap());
}

#[test]
fn sha256_digest() {
    assert_eq!(
        digest(ChecksumKind::Sha256, &b"abc"[..]).unwrap(),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn file_ok_checks_size_then_hash() {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("download-file-ok");
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("x.zip");
    std::fs::write(&f, b"abc").unwrap();
    let good = sha1("a9993e364706816aba3e25717850c26c9cd0d89d");
    assert!(file_ok(&f, 3, &good).unwrap());
    assert!(!file_ok(&f, 4, &good).unwrap());
    assert!(!file_ok(&f, 3, &sha1("0000000000000000000000000000000000000000")).unwrap());
    assert!(!file_ok(&dir.join("missing.zip"), 3, &good).unwrap());
    assert_eq!(
        part_path(&f).file_name().unwrap().to_str().unwrap(),
        "x.zip.part"
    );
}

#[test]
fn hex_round_trip() {
    let bytes = [0u8, 1, 0xab, 0xff];
    assert_eq!(hex(&bytes), "0001abff");
    assert_eq!(unhex("0001abff").unwrap(), bytes);
    assert!(unhex("abc").is_err());
    assert!(unhex("zz").is_err());
}
