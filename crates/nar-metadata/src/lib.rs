use nix_derivation::{NixHash, StorePath};
use serde::{Deserialize, Serialize};

fn full_store_path(value: &str) -> anyhow::Result<StorePath> {
    anyhow::ensure!(
        value.starts_with("/nix/store/"),
        "full /nix/store path required"
    );
    Ok(value.parse()?)
}

/// Validate the shared uncompressed backend without conflating hash encodings.
/// Returns canonical NAR hash for the registry identity key.
pub fn validate_candidate(
    metadata: Option<&Metadata>,
    artifact: Option<&Artifact>,
    store_path: &str,
    store_path_hash: &str,
    nar_hash: &str,
    nar_size: i64,
) -> anyhow::Result<String> {
    anyhow::ensure!(
        metadata.is_some() || artifact.is_none(),
        "artifact requires metadata"
    );
    let Some(metadata) = metadata else {
        return Ok(nar_hash.to_owned());
    };
    let path = full_store_path(store_path)?;
    anyhow::ensure!(
        path.to_basename().split_once('-').map(|v| v.0) == Some(store_path_hash),
        "store hash/path mismatch"
    );
    let hash: NixHash = nar_hash.parse()?;
    anyhow::ensure!(
        hash.to_nix_base16_string().starts_with("sha256:"),
        "SHA256 required"
    );
    let size = u64::try_from(nar_size)?;
    anyhow::ensure!(size > 0, "NarSize must be positive");
    for reference in &metadata.references {
        full_store_path(reference)?;
    }
    if let Some(deriver) = &metadata.deriver {
        full_store_path(deriver)?;
        anyhow::ensure!(deriver.ends_with(".drv"), "deriver must be a derivation");
    }
    if let Some(artifact) = artifact {
        anyhow::ensure!(
            artifact.compression == "none",
            "only uncompressed NAR supported"
        );
        anyhow::ensure!(
            artifact.file_hash.len() == 64
                && artifact
                    .file_hash
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "blob key must be lowercase SHA256 hex"
        );
        let file_hash: NixHash = format!("sha256:{}", artifact.file_hash).parse()?;
        anyhow::ensure!(
            file_hash == hash && artifact.file_size == size,
            "uncompressed download/NAR mismatch"
        );
    }
    Ok(hash.to_sri_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_sized_nar_cannot_produce_download_metadata() {
        let metadata = Metadata {
            references: vec![],
            deriver: None,
        };
        assert!(
            validate_candidate(
                Some(&metadata),
                None,
                "/nix/store/y1a49lg2ja68djssigz14lhdxvxcwbxa-hello",
                "y1a49lg2ja68djssigz14lhdxvxcwbxa",
                "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
                0
            )
            .is_err()
        );
    }
}

/// Store-object metadata, independent of download representation.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Metadata {
    pub references: Vec<String>,
    pub deriver: Option<String>,
}

impl Metadata {
    pub fn canonicalize(&mut self) {
        self.references.sort();
        self.references.dedup();
    }
}

/// Download metadata: the blob key is SHA256 of transmitted bytes, not a store hash.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub file_hash: String,
    pub file_size: u64,
    pub compression: String,
}
