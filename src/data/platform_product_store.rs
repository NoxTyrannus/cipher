use crate::common::types::ThoughtId;
use crate::common::{AgentError, Result, UtcTimestamp};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

const PRODUCTS_DIR: &str = "products";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProductType {
    Execution,
    Insight,
    Memory,
}

impl ProductType {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProductType::Execution => "execution_product",
            ProductType::Insight => "insight_product",
            ProductType::Memory => "memory_output",
        }
    }
}

pub struct PlatformProductStore {
    root: PathBuf,
}

impl PlatformProductStore {
    pub fn open(data_dir: &Path) -> Result<Self> {
        let root = data_dir.join(PRODUCTS_DIR);
        crate::data::permissions::ensure_private_directory(&root)?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn product_path(
        &self,
        ptype: ProductType,
        thought_id: &ThoughtId,
        occurred_at: &UtcTimestamp,
    ) -> PathBuf {
        let (year, month, day) = occurred_at.date_components();
        self.root
            .join(ptype.as_str())
            .join(format!("{year:04}"))
            .join(format!("{month:02}"))
            .join(format!("{day:02}"))
            .join(format!("{}_{}", occurred_at.path_component(), thought_id))
            .with_extension("json")
    }

    pub fn write<T: Serialize>(
        &self,
        ptype: ProductType,
        thought_id: &ThoughtId,
        occurred_at: &UtcTimestamp,
        product: &T,
    ) -> Result<PathBuf> {
        let final_path = self.product_path(ptype, thought_id, occurred_at);
        if let Some(parent) = final_path.parent() {
            crate::data::permissions::ensure_private_directory(parent)?;
        }

        let content = serde_json::to_string_pretty(product)
            .map_err(|e| AgentError::Parse(format!("serialize {}: {e}", ptype.as_str())))?;

        let tmp_path = final_path.with_extension("json.tmp");
        fs::write(&tmp_path, content.as_bytes()).map_err(|e| {
            AgentError::Io(format!("write product tmp {}: {e}", tmp_path.display()))
        })?;
        crate::data::permissions::secure_existing_file(&tmp_path)?;

        fs::rename(&tmp_path, &final_path)
            .map_err(|e| AgentError::Io(format!("rename product {}: {e}", final_path.display())))?;
        crate::data::permissions::secure_existing_file(&final_path)?;

        Ok(final_path)
    }
}

#[cfg(test)]
mod tests {}
