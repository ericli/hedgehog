use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};

pub type EnvMap = BTreeMap<String, String>;

/// Reads `KEY=value` pairs from a dotenv file.
pub fn read_dotenv_file(path: &Path) -> Result<EnvMap> {
    let mut env = EnvMap::new();
    for item in dotenvy::from_path_iter(path)
        .with_context(|| format!("could not read {}", path.display()))?
    {
        let (key, value) = item.with_context(|| format!("invalid line in {}", path.display()))?;
        env.insert(key, value);
    }
    Ok(env)
}

/// Reads the app folder's `.env`, if it has one.
pub fn read_app_dotenv(dir: &Path) -> Result<EnvMap> {
    let path = dir.join(".env");
    if path.is_file() {
        read_dotenv_file(&path)
    } else {
        Ok(EnvMap::new())
    }
}

/// Merges env layers; later layers win. hedgehog uses, lowest first: the
/// app's `.env`, hedgehog's own values (`PORT`, `DATABASE_URL`, ...), the
/// manifest's `[env]`, then values set with `hedgehog env set`.
pub fn merge(layers: &[&EnvMap]) -> EnvMap {
    let mut env = EnvMap::new();
    for layer in layers {
        env.extend(
            layer
                .iter()
                .map(|(key, value)| (key.clone(), value.clone())),
        );
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> EnvMap {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn later_layers_win() {
        let dotenv = map(&[
            ("PORT", "3000"),
            ("API_KEY", "dev"),
            ("DATABASE_URL", "postgres://localhost/dev"),
        ]);
        let hedgehog = map(&[
            ("PORT", "8080"),
            ("DATABASE_URL", "postgres://hedgehog-postgres/app"),
        ]);
        let manifest = map(&[("NODE_ENV", "production")]);
        let user = map(&[("API_KEY", "prod")]);
        let env = merge(&[&dotenv, &hedgehog, &manifest, &user]);
        assert_eq!(env["PORT"], "8080");
        assert_eq!(env["DATABASE_URL"], "postgres://hedgehog-postgres/app");
        assert_eq!(env["API_KEY"], "prod");
        assert_eq!(env["NODE_ENV"], "production");
    }

    #[test]
    fn reads_dotenv() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_app_dotenv(dir.path()).unwrap().is_empty());
        std::fs::write(dir.path().join(".env"), "# comment\nA=1\nB=\"two words\"\n").unwrap();
        let env = read_app_dotenv(dir.path()).unwrap();
        assert_eq!(env, map(&[("A", "1"), ("B", "two words")]));
    }
}
