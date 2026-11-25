//! `hiroi checksum`: a file's hash, or whether it matches one.

use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use hiroi_engine::{Checksum, HashAlgorithm, hash_file};

/// Prints the hash of `file`, or compares it with `expected`, which is
/// `sha256:9f86...` or plain hex (the algorithm then follows from its
/// length).
pub fn run(file: &Path, algorithm: &str, expected: Option<&str>) -> Result<ExitCode> {
    let Some(expected) = expected else {
        let algorithm = HashAlgorithm::from_name(algorithm)
            .with_context(|| format!("unknown hash `{algorithm}`; md5, sha1, sha256, or sha512"))?;
        let hash = hash(file, algorithm)?;
        println!("{algorithm}:{hash}  {}", file.display());
        return Ok(ExitCode::SUCCESS);
    };
    let checksum: Checksum = with_algorithm(expected)?
        .parse()
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let actual = hash(file, checksum.algorithm())?;
    if actual.eq_ignore_ascii_case(checksum.expected()) {
        println!("OK: {} matches", file.display());
        Ok(ExitCode::SUCCESS)
    } else {
        println!(
            "MISMATCH: {} is {}:{actual}, not {}:{}",
            file.display(),
            checksum.algorithm(),
            checksum.algorithm(),
            checksum.expected()
        );
        Ok(ExitCode::FAILURE)
    }
}

/// `9f86...` becomes `sha256:9f86...`, judged by its length, as hashes
/// are often published without their name.
fn with_algorithm(expected: &str) -> Result<String> {
    let expected = expected.trim();
    if expected.contains(':') {
        return Ok(expected.to_string());
    }
    let name = match expected.len() {
        32 => "md5",
        40 => "sha1",
        64 => "sha256",
        128 => "sha512",
        _ => bail!("`{expected}` is not a hash; give it as sha256:HEX"),
    };
    Ok(format!("{name}:{expected}"))
}

fn hash(file: &Path, algorithm: HashAlgorithm) -> Result<String> {
    hash_file(file, algorithm).with_context(|| format!("cannot read {}", file.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_hashes_are_named_by_their_length() {
        assert_eq!(
            with_algorithm(&"a".repeat(64)).unwrap(),
            format!("sha256:{}", "a".repeat(64))
        );
        assert_eq!(
            with_algorithm(&"a".repeat(32)).unwrap(),
            format!("md5:{}", "a".repeat(32))
        );
        assert!(with_algorithm("abc").is_err());
        assert_eq!(with_algorithm("sha1:abc").unwrap(), "sha1:abc");
    }

    #[test]
    fn files_are_hashed_and_compared() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        std::fs::write(&file, b"test").unwrap();
        let sha256 = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
        assert_eq!(
            run(&file, "sha256", Some(sha256)).unwrap(),
            ExitCode::SUCCESS
        );
        let wrong = format!("sha256:{}", "0".repeat(64));
        assert_eq!(
            run(&file, "sha256", Some(&wrong)).unwrap(),
            ExitCode::FAILURE
        );
    }
}
