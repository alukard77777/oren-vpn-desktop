//! Refuses to launch a core that no longer matches the package's `SHA256SUMS.txt` — for
//! example after Mihomo's own `POST /upgrade` replaced its executable as SYSTEM.

use std::{collections::BTreeMap, fs::File, io::Read, path::Path};

use sha2::{Digest, Sha256};

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum CoreHashError {
    Manifest,
    Unlisted(String),
    Unreadable(String),
    Mismatch(String),
}

/// Checks every `relative` path (as written in the manifest, e.g. `cores/mihomo.exe`) under
/// `package_root` against `package_root/SHA256SUMS.txt` (`<sha256> *<relative>` lines).
pub(crate) fn verify_core_hashes(
    package_root: &Path,
    relative: &[&str],
) -> Result<(), CoreHashError> {
    let manifest = std::fs::read_to_string(package_root.join("SHA256SUMS.txt"))
        .map_err(|_| CoreHashError::Manifest)?;
    let expected = manifest
        .lines()
        .filter_map(|line| {
            let (hash, name) = line.split_once(' ')?;
            Some((
                name.trim_start_matches(['*', ' ']).to_owned(),
                hash.to_ascii_lowercase(),
            ))
        })
        .collect::<BTreeMap<_, _>>();
    for name in relative {
        let hash = expected
            .get(*name)
            .ok_or_else(|| CoreHashError::Unlisted((*name).to_owned()))?;
        let mut file = File::open(package_root.join(name))
            .map_err(|_| CoreHashError::Unreadable((*name).to_owned()))?;
        let mut digest = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = file
                .read(&mut buffer)
                .map_err(|_| CoreHashError::Unreadable((*name).to_owned()))?;
            if read == 0 {
                break;
            }
            digest.update(&buffer[..read]);
        }
        if format!("{:x}", digest.finalize()) != *hash {
            return Err(CoreHashError::Mismatch((*name).to_owned()));
        }
    }
    Ok(())
}
