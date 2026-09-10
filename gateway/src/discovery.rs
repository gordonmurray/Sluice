//! Optional public Bazaar declarations. Product schemas live in operator configuration.
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
#[derive(Deserialize)]
#[serde(untagged)]
pub enum Config {
    Routes { routes: Vec<Route> },
    Legacy(Discovery),
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    pub path: String,
    #[serde(flatten)]
    pub declaration: Discovery,
}
impl Config {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let bytes = std::fs::read(path)?;
        anyhow::ensure!(bytes.len() <= 65_536, "discovery config exceeds 64 KiB");
        let config: Self = serde_json::from_slice(&bytes)?;
        config.validate()?;
        Ok(config)
    }
    fn validate(&self) -> anyhow::Result<()> {
        match self {
            Self::Legacy(config) => config.validate(),
            Self::Routes { routes } => {
                anyhow::ensure!(
                    !routes.is_empty() && routes.len() <= 32,
                    "discovery needs 1–32 routes"
                );
                let mut paths = std::collections::HashSet::new();
                for route in routes {
                    anyhow::ensure!(
                        valid_path(&route.path, true)
                            && !matches!(route.path.as_str(), "/healthz" | "/metrics"),
                        "invalid discovery route"
                    );
                    anyhow::ensure!(paths.insert(&route.path), "duplicate discovery route");
                    route.declaration.validate()?;
                }
                Ok(())
            }
        }
    }
}
fn valid_path(path: &str, parameters: bool) -> bool {
    path.starts_with('/')
        && path.len() <= 256
        && !path.contains("//")
        && path.split('/').skip(1).all(|segment| {
            let segment = if parameters && segment.starts_with('{') && segment.ends_with('}') {
                &segment[1..segment.len() - 1]
            } else {
                segment
            };
            !segment.is_empty()
                && segment
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
        })
}
/// Explicit opt-in: journal preparation can execute a POST before settlement.
/// Operators must only list endpoints without side effects.
pub fn read_only_post_paths(value: Option<&str>) -> anyhow::Result<Vec<String>> {
    let paths: Vec<String> = value
        .map(serde_json::from_str)
        .transpose()?
        .unwrap_or_default();
    anyhow::ensure!(
        paths.len() <= 32 && paths.iter().all(|p| valid_path(p, false)),
        "JOURNAL_READ_ONLY_POST_PATHS needs at most 32 exact paths in a JSON array"
    );
    Ok(paths)
}
impl Discovery {
    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.description.is_empty() && self.description.chars().count() <= 500,
            "discovery description must contain 1–500 characters"
        );
        anyhow::ensure!(
            self.bazaar.0["info"]["input"]["type"] == "http"
                && matches!(
                    self.bazaar.0["info"]["input"]["method"].as_str(),
                    Some("GET" | "POST")
                ),
            "discovery supports GET and POST HTTP declarations"
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
    fn validates_routes_and_post_opt_in() {
        let mut config = Discovery {
            description: "Read a record".into(),
            bazaar: Bazaar(
                json!({"info":{"input":{"type":"http","method":"POST"}},"schema":{"type":"object"}}),
            ),
        };
        assert!(config.validate().is_ok());
        config.description = "x".repeat(501);
        assert!(config.validate().is_err());
        assert!(read_only_post_paths(Some(r#"["/api/resolve"]"#)).is_ok());
        for bad in [
            r#"["/"]"#,
            r#"["/api/{id}"]"#,
            r#"["/api/../write"]"#,
            r#"["/api?x=1"]"#,
        ] {
            assert!(read_only_post_paths(Some(bad)).is_err());
        }
        let declaration = json!({"path":"/item/{id}","description":"Read", "bazaar":{"info":{"input":{"type":"http","method":"GET"}},"schema":{}}});
        let valid: Config =
            serde_json::from_value(json!({"routes":[declaration.clone()]})).unwrap();
        assert!(valid.validate().is_ok());
        let duplicate: Config =
            serde_json::from_value(json!({"routes":[declaration.clone(),declaration]})).unwrap();
        assert!(duplicate.validate().is_err());
    }
}
