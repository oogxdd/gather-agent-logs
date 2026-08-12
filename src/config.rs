//! Where the collector finds its database URL and this machine's name.

use std::{env, fs, path::PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};

pub const DATABASE_URL_ENV: &str = "AGENT_LOGS_DATABASE_URL";
pub const HOST_ENV: &str = "AGENT_LOGS_HOST";
pub const CONFIG_ENV: &str = "AGENT_LOGS_CONFIG";

#[derive(Clone, Debug)]
pub struct Config {
    pub database_url: Option<String>,
    /// Name this machine's sessions are stored under.
    pub host: String,
    pub path: PathBuf,
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_path();
        let stored = read_config_file(&path)?;

        let database_url = env::var(DATABASE_URL_ENV)
            .ok()
            .or_else(|| string_at(&stored, "database_url"))
            .or_else(|| env::var("DATABASE_URL").ok())
            .filter(|url| !url.trim().is_empty());
        let host = env::var(HOST_ENV)
            .ok()
            .or_else(|| string_at(&stored, "host"))
            .filter(|host| !host.trim().is_empty())
            .unwrap_or_else(default_host);

        Ok(Self {
            database_url,
            host: host.trim().to_owned(),
            path,
        })
    }

    pub fn database_url(&self) -> Result<&str> {
        match self.database_url.as_deref() {
            Some(url) => Ok(url),
            None => bail!(
                "no database configured; run `agent-logs setup --database-url postgres://…` or set {DATABASE_URL_ENV}"
            ),
        }
    }

    /// Writes the values that were provided, leaving the rest of the file alone.
    pub fn save(&self, database_url: Option<&str>, host: Option<&str>) -> Result<()> {
        let mut stored = read_config_file(&self.path)?;
        if let Some(url) = database_url {
            stored.insert("database_url".to_owned(), Value::String(url.to_owned()));
        }
        if let Some(host) = host {
            stored.insert("host".to_owned(), Value::String(host.to_owned()));
        }

        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("could not create {}", parent.display()))?;
        }
        let body = serde_json::to_string_pretty(&Value::Object(stored))?;
        fs::write(&self.path, format!("{body}\n"))
            .with_context(|| format!("could not write {}", self.path.display()))?;
        restrict_permissions(&self.path)?;
        Ok(())
    }
}

pub fn config_path() -> PathBuf {
    if let Some(path) = env::var_os(CONFIG_ENV) {
        return PathBuf::from(path);
    }
    let base = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"));
    base.join("agent-logs").join("config.json")
}

/// The machine name sessions are grouped by. On a Sprite this is the sprite's
/// own name, which is exactly what makes sandbox sessions easy to tell apart.
pub fn default_host() -> String {
    hostname::get()
        .ok()
        .and_then(|name| name.into_string().ok())
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "unknown-host".to_owned())
}

fn read_config_file(path: &PathBuf) -> Result<Map<String, Value>> {
    let body = match fs::read_to_string(path) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("could not read {}", path.display()));
        }
    };
    if body.trim().is_empty() {
        return Ok(Map::new());
    }
    let value: Value = serde_json::from_str(&body)
        .with_context(|| format!("{} is not valid JSON", path.display()))?;
    match value {
        Value::Object(map) => Ok(map),
        _ => bail!("{} must contain a JSON object", path.display()),
    }
}

fn string_at(map: &Map<String, Value>, key: &str) -> Option<String> {
    map.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|value| !value.trim().is_empty())
}

/// The file holds a database password, so keep it readable by its owner only.
fn restrict_permissions(path: &PathBuf) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("could not restrict permissions on {}", path.display()))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Hides the password when a connection string is printed back to the user.
pub fn redact_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    let Some((credentials, tail)) = rest.split_once('@') else {
        return url.to_owned();
    };
    let user = credentials.split(':').next().unwrap_or_default();
    format!("{scheme}://{user}:***@{tail}")
}

#[cfg(test)]
mod tests {
    use super::redact_url;

    #[test]
    fn redacts_the_password_only() {
        assert_eq!(
            redact_url("postgresql://collector:s3cret@db.example.com/agent?sslmode=require"),
            "postgresql://collector:***@db.example.com/agent?sslmode=require"
        );
        assert_eq!(redact_url("postgres:///agent"), "postgres:///agent");
    }
}
