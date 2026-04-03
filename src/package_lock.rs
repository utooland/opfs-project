use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::HashMap;

/// Represents package information in package-lock.json
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LockPackage {
    pub name: Option<String>,
    pub version: Option<String>,
    pub resolved: Option<String>,
    pub integrity: Option<String>,
    pub shasum: Option<String>,
    pub license: Option<String>,
    pub dependencies: Option<HashMap<String, String>>,
    #[serde(rename = "devDependencies")]
    pub dev_dependencies: Option<HashMap<String, String>>,
    #[serde(rename = "peerDependencies")]
    pub peer_dependencies: Option<HashMap<String, String>>,
    #[serde(rename = "optionalDependencies")]
    pub optional_dependencies: Option<HashMap<String, String>>,
    pub requires: Option<HashMap<String, String>>,
    pub bin: Option<serde_json::Value>,
    pub peer: Option<bool>,
    pub dev: Option<bool>,
    pub optional: Option<bool>,
    #[serde(rename = "hasInstallScript")]
    pub has_install_script: Option<bool>,
    pub workspaces: Option<Vec<String>>,
    /// OS constraints (e.g., ["darwin", "win32"])
    pub os: Option<serde_json::Value>,
    /// CPU constraints (e.g., ["arm64", "x64"])
    pub cpu: Option<serde_json::Value>,
}

impl LockPackage {
    /// Get package name, infer from path if not available.
    ///
    /// Returns `Cow::Borrowed` when possible to avoid heap allocation
    /// on hot paths (e.g. skipped packages during install).
    pub fn get_name<'a>(&'a self, path: &'a str) -> Cow<'a, str> {
        if let Some(name) = &self.name {
            Cow::Borrowed(name.as_str())
        } else if path.is_empty() {
            Cow::Borrowed("root")
        } else {
            // Extract package name from path.
            // Handle scoped packages: node_modules/@scope/pkg → @scope/pkg
            let parts: Vec<&str> = path.rsplitn(3, '/').collect();
            if parts.len() >= 2 && parts[1].starts_with('@') {
                Cow::Owned(format!("{}/{}", parts[1], parts[0]))
            } else {
                Cow::Borrowed(parts[0])
            }
        }
    }
    /// Get package version.
    ///
    /// Returns `Cow::Borrowed` to avoid cloning on hot paths.
    pub fn get_version(&self) -> Cow<'_, str> {
        match &self.version {
            Some(v) => Cow::Borrowed(v.as_str()),
            None => Cow::Borrowed("unknown"),
        }
    }
}

/// Represents complete package-lock.json file
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageLock {
    pub name: String,
    pub version: String,
    #[serde(rename = "lockfileVersion")]
    pub lockfile_version: u32,
    pub requires: bool,
    pub packages: HashMap<String, LockPackage>,

    pub dependencies: Option<HashMap<String, serde_json::Value>>,
}

impl PackageLock {
    /// Parse from json string
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }
}
