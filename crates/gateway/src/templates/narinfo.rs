use axum::{
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use nix_narinfo::NarInfo;
use serde::Deserialize;

use crate::store_path_hash::StorePathHash;

pub struct NarInfoResponse(pub NarInfo);

impl NarInfoResponse {
    pub fn from_blob(
        record: &crate::nar_record::NarRecord,
        metadata: &nar_metadata::Metadata,
        artifact: &nar_metadata::Artifact,
        url: String,
    ) -> anyhow::Result<Self> {
        let info = NarInfo::builder(
            record.store_path.clone(),
            url,
            record.nar_hash.clone(),
            record.nar_size,
        )
        .compression(nix_narinfo::Compression::None)
        .file_hash(Some(format!("sha256:{}", artifact.file_hash).parse()?))
        .file_size(Some(artifact.file_size))
        .references(
            metadata
                .references
                .iter()
                .map(|v| v.parse())
                .collect::<Result<Vec<_>, _>>()?,
        )
        .deriver(metadata.deriver.as_ref().map(|v| v.parse()).transpose()?)
        .build()?;
        Ok(Self(info))
    }

    pub fn from_upstream(upstream: NarInfo, url: String) -> Result<Self, anyhow::Error> {
        let info = NarInfo::builder_in(
            upstream.store_dir().clone(),
            upstream.store_path().clone(),
            url,
            upstream.nar_hash().clone(),
            upstream.nar_size(),
        )
        .compression(upstream.compression().clone())
        .references(upstream.references().iter().cloned())
        .deriver(upstream.deriver().cloned())
        .signatures(upstream.signatures().iter().cloned())
        .content_address(upstream.content_address().cloned())
        .file_hash(upstream.file_hash().cloned())
        .file_size(upstream.file_size())
        .extensions(upstream.extensions().iter().cloned())
        .build()?;

        Ok(Self(info))
    }
}

impl IntoResponse for NarInfoResponse {
    fn into_response(self) -> Response {
        (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/x-nix-narinfo")],
            self.0.to_canonical_bytes(),
        )
            .into_response()
    }
}

#[derive(Debug, Deserialize)]
#[serde(try_from = "String")]
pub struct NarInfoPath(StorePathHash);

impl TryFrom<String> for NarInfoPath {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let hash = value.strip_suffix(".narinfo").ok_or("not a narinfo path")?;

        StorePathHash::try_from(hash.to_owned())
            .map(Self)
            .map_err(|_| "invalid store hash")
    }
}

impl NarInfoPath {
    pub fn hash(&self) -> &StorePathHash {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::NarInfoPath;

    #[test]
    fn accepts_a_nix_store_hash_path() {
        let path =
            NarInfoPath::try_from("0123456789abcdfghijklmnpqrsvwxyz.narinfo".to_owned()).unwrap();

        assert_eq!(path.hash().as_str(), "0123456789abcdfghijklmnpqrsvwxyz");
    }

    #[test]
    fn rejects_an_invalid_nix_store_hash_path() {
        assert!(
            NarInfoPath::try_from("0123456789abcdefghijklmnopqrstuv.narinfo".to_owned()).is_err()
        );
    }
}
