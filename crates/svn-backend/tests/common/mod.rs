#![allow(dead_code)]

use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;

pub fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

pub fn fixture(name: &str) -> BufReader<File> {
    let p = fixture_path(name);
    BufReader::new(File::open(&p).unwrap_or_else(|e| panic!("open {}: {e}", p.display())))
}

pub fn ts(s: &str) -> Option<svn_backend::Timestamp> {
    Some(s.parse().unwrap())
}

pub const FAKE_ROOT: &str = "file:///tmp/gsvn-fixture/repo";
pub const UUID: &str = "01234567-89ab-cdef-0123-456789abcdef";
