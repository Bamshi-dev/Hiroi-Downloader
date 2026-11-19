use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::str::FromStr;

use sha2::Digest;

use crate::error::EngineError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashAlgorithm {
    Md5,
    Sha1,
    Sha256,
    Sha512,
}

impl HashAlgorithm {
    fn hex_len(self) -> usize {
        match self {
            Self::Md5 => 32,
            Self::Sha1 => 40,
            Self::Sha256 => 64,
            Self::Sha512 => 128,
        }
    }
}

impl fmt::Display for HashAlgorithm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Md5 => "md5",
            Self::Sha1 => "sha1",
            Self::Sha256 => "sha256",
            Self::Sha512 => "sha512",
        };
        f.write_str(name)
    }
}

/// Expected hash of a finished download, e.g. `sha256:9f86d0...`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checksum {
    algorithm: HashAlgorithm,
    expected: String,
}

impl Checksum {
    pub fn algorithm(&self) -> HashAlgorithm {
        self.algorithm
    }

    pub fn expected(&self) -> &str {
        &self.expected
    }

    /// Hashes `path` and compares. Blocking: call from a blocking thread.
    pub(crate) fn verify(&self, path: &Path) -> Result<(), EngineError> {
        let actual = hash_file(path, self.algorithm).map_err(|e| EngineError::io(path, e))?;
        if actual == self.expected {
            Ok(())
        } else {
            Err(EngineError::ChecksumMismatch {
                expected: self.expected.clone(),
                actual,
            })
        }
    }
}

impl FromStr for Checksum {
    type Err = EngineError;

    /// Accepts `algorithm:hex` or `algorithm=hex`.
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let invalid = || EngineError::InvalidChecksum(value.to_string());
        let (name, hex) = value.split_once([':', '=']).ok_or_else(invalid)?;
        let algorithm = match name.trim().to_ascii_lowercase().replace('-', "").as_str() {
            "md5" => HashAlgorithm::Md5,
            "sha1" => HashAlgorithm::Sha1,
            "sha256" => HashAlgorithm::Sha256,
            "sha512" => HashAlgorithm::Sha512,
            _ => return Err(invalid()),
        };
        let expected = hex.trim().to_ascii_lowercase();
        let is_hex = expected.bytes().all(|b| b.is_ascii_hexdigit());
        if expected.len() != algorithm.hex_len() || !is_hex {
            return Err(invalid());
        }
        Ok(Self {
            algorithm,
            expected,
        })
    }
}

impl fmt::Display for Checksum {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.algorithm, self.expected)
    }
}

fn hash_file(path: &Path, algorithm: HashAlgorithm) -> io::Result<String> {
    let file = File::open(path)?;
    match algorithm {
        HashAlgorithm::Md5 => hash_reader::<md5::Md5>(file),
        HashAlgorithm::Sha1 => hash_reader::<sha1::Sha1>(file),
        HashAlgorithm::Sha256 => hash_reader::<sha2::Sha256>(file),
        HashAlgorithm::Sha512 => hash_reader::<sha2::Sha512>(file),
    }
}

fn hash_reader<D: Digest>(mut reader: impl Read) -> io::Result<String> {
    let mut hasher = D::new();
    let mut buffer = vec![0; 1024 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_separators_and_any_case() {
        let hex = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let a: Checksum = format!("sha256:{hex}").parse().unwrap();
        let b: Checksum = format!("SHA-256={}", hex.to_uppercase()).parse().unwrap();
        assert_eq!(a, b);
        assert_eq!(a.algorithm(), HashAlgorithm::Sha256);
    }

    #[test]
    fn rejects_wrong_length_or_unknown_algorithm() {
        assert!("sha256:abc".parse::<Checksum>().is_err());
        assert!("crc32:00000000".parse::<Checksum>().is_err());
        assert!("sha1".parse::<Checksum>().is_err());
    }

    #[test]
    fn hashes_known_input() {
        let hash = hash_reader::<sha2::Sha256>(&b"abc"[..]).unwrap();
        assert_eq!(
            hash,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
