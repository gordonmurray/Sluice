//! Optional public Bazaar declaration. Product schemas live in operator configuration.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use x402_types::scheme::ExtensionKey;

#[derive(Clone, Deserialize)]
pub struct Discovery {
    pub description: String,
    pub bazaar: Bazaar,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(transparent)]
pub struct Bazaar(pub Value);
impl ExtensionKey for Bazaar {
    const EXTENSION_KEY: &'static str = "bazaar";
}
impl Discovery {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let bytes = std::fs::read(path)?;
        anyhow::ensure!(bytes.len() <= 16_384, "discovery config exceeds 16 KiB");
        let config: Self = serde_json::from_slice(&bytes)?;
        config.validate()?;
        Ok(config)
    }
    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.description.is_empty() && self.description.chars().count() <= 500,
            "discovery description must contain 1–500 characters"
        );
        anyhow::ensure!(
            self.bazaar.0["info"]["input"]["type"] == "http"
                && self.bazaar.0["info"]["input"]["method"] == "GET",
            "discovery currently supports GET HTTP declarations only"
        );
        anyhow::ensure!(
            self.bazaar.0["schema"].is_object(),
            "discovery requires an input/output JSON schema"
        );
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn bounded_get_declaration() {
        let mut config = Discovery {
            description: "Read a record".into(),
            bazaar: Bazaar(
                json!({"info":{"input":{"type":"http","method":"GET"}},"schema":{"type":"object"}}),
            ),
        };
        assert!(config.validate().is_ok());
        config.description = "x".repeat(501);
        assert!(config.validate().is_err());
        config.description = "Read a record".into();
        config.bazaar.0["info"]["input"]["method"] = json!("POST");
        assert!(config.validate().is_err());
    }
}
