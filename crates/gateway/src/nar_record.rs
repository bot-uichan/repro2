use anyhow::Result;
use nix_derivation::{NixHash, StorePath};
use nix_narinfo::NarInfo;
use serde::{Deserialize, Serialize};

use crate::store_path_hash::StorePathHash;

#[derive(Clone, Deserialize, Serialize)]
pub struct RegistryNarRecord {
    #[serde(default)]
    pub user_id: Option<String>,
    pub drv_path: Option<String>,
    pub output_name: Option<String>,
    pub store_path_hash: String,
    pub store_path: String,
    pub nar_hash: String,
    pub nar_size: i64,
    pub cache_url: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct NarRecord {
    pub user_id: Option<String>,
    pub drv_path: Option<String>,
    pub output_name: Option<String>,
    pub store_path_hash: StorePathHash,
    pub store_path: StorePath,
    pub nar_hash: NixHash,
    pub nar_size: u64,
    pub cache_url: String,
}

impl NarRecord {
    pub fn matches_result(&self, other: &Self) -> bool {
        self.drv_path == other.drv_path
            && self.output_name == other.output_name
            && self.matches_nar(other)
    }

    pub fn matches_nar(&self, other: &Self) -> bool {
        self.store_path_hash == other.store_path_hash
            && self.store_path == other.store_path
            && self.nar_hash == other.nar_hash
            && self.nar_size == other.nar_size
    }
}

impl TryFrom<RegistryNarRecord> for NarRecord {
    type Error = anyhow::Error;

    fn try_from(model: RegistryNarRecord) -> Result<Self, Self::Error> {
        let store_path_hash = StorePathHash::try_from(model.store_path_hash)?;
        let store_path: StorePath = model.store_path.parse()?;
        anyhow::ensure!(
            store_path_hash == StorePathHash::try_from(&store_path)?,
            "store hash/path mismatch"
        );
        Ok(Self {
            user_id: model.user_id,
            drv_path: model.drv_path,
            output_name: model.output_name,
            store_path_hash,
            store_path,
            nar_hash: model.nar_hash.parse()?,
            nar_size: model.nar_size.try_into()?,
            cache_url: model.cache_url.unwrap_or_default(),
        })
    }
}

impl TryFrom<NarInfo> for NarRecord {
    type Error = anyhow::Error;

    fn try_from(value: NarInfo) -> Result<Self, Self::Error> {
        Ok(Self {
            user_id: None,
            drv_path: None,
            output_name: None,
            store_path_hash: StorePathHash::try_from(value.store_path())?.clone(),
            store_path: value.store_path().clone(),
            nar_hash: value.nar_hash().clone(),
            nar_size: value.nar_size(),
            cache_url: value.url().to_owned(),
        })
    }
}
