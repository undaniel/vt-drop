//! Streaming SHA-256 hashing.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::util::hex_lower;

const BUFFER_SIZE: usize = 1024 * 1024;

pub fn hash_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    hash_reader(&mut file)
}

pub fn hash_reader(reader: &mut impl Read) -> io::Result<String> {
    hash_reader_with(reader, |_| true)
}

/// Like `hash_file`, reporting the bytes hashed so far after each block.
/// Returning false from `progress` stops with `ErrorKind::Interrupted`.
pub fn hash_file_with(path: &Path, progress: impl FnMut(u64) -> bool) -> io::Result<String> {
    let mut file = File::open(path)?;
    hash_reader_with(&mut file, progress)
}

fn hash_reader_with(
    reader: &mut impl Read,
    mut progress: impl FnMut(u64) -> bool,
) -> io::Result<String> {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; BUFFER_SIZE];
    let mut done = 0u64;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        done += read as u64;
        if !progress(done) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelado"));
        }
    }
    Ok(hex_lower(&hasher.finalize()))
}

pub fn hash_bytes(bytes: &[u8]) -> String {
    hex_lower(&Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const EICAR: &[u8] = b"X5O!P%@AP[4\\PZX54(P^)7CC)7}$EICAR-STANDARD-ANTIVIRUS-TEST-FILE!$H+H*";
    const EICAR_SHA256: &str = "275a021bbfb6489e54d471899f7db9d1663fc695ec2fe2a2c4538aabf651fd0f";

    #[test]
    fn eicar_sha256_matches_virustotal_reference() {
        assert_eq!(hash_bytes(EICAR), EICAR_SHA256);
    }

    #[test]
    fn streaming_and_one_shot_agree() {
        let mut cursor = Cursor::new(EICAR);
        assert_eq!(hash_reader(&mut cursor).unwrap(), EICAR_SHA256);
    }

    #[test]
    fn empty_input_hash() {
        assert_eq!(
            hash_bytes(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
