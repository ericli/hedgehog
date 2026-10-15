//! The daemon's SQLite database: registered apps, env vars, paired devices,
//! and settings. Queries are tiny, so a mutex around one connection is plenty.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use anyhow::{Context, Result};
use hedgehog_core::api::{AppStatus, DeviceInfo};
use hedgehog_core::env::EnvMap;
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::secrets::now;

const MIGRATIONS: &[&str] = &[
    r#"
    CREATE TABLE apps (
        name TEXT PRIMARY KEY,
        source_dir TEXT NOT NULL,
        public INTEGER,
        stopped INTEGER NOT NULL DEFAULT 0,
        status TEXT NOT NULL DEFAULT 'new',
        container TEXT,
        image TEXT,
        builder TEXT,
        revision TEXT,
        deployed_at INTEGER,
        last_error TEXT,
        created_at INTEGER NOT NULL
    );
    CREATE TABLE env (
        app TEXT NOT NULL,
        key TEXT NOT NULL,
        value TEXT NOT NULL,
        PRIMARY KEY (app, key)
    );
    CREATE TABLE devices (
        id TEXT PRIMARY KEY,
        label TEXT,
        token_hash TEXT NOT NULL UNIQUE,
        created_at INTEGER NOT NULL
    );
    CREATE TABLE pair_codes (
        code_hash TEXT PRIMARY KEY,
        label TEXT,
        expires_at INTEGER NOT NULL
    );
    CREATE TABLE settings (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
"#,
    r#"
    ALTER TABLE devices ADD COLUMN admin INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE pair_codes ADD COLUMN admin INTEGER NOT NULL DEFAULT 0;
"#,
];

#[derive(Debug, Clone, PartialEq)]
pub struct AppRecord {
    pub name: String,
    pub source_dir: PathBuf,
    /// `hedgehog public` override; `None` means "use the manifest".
    pub public: Option<bool>,
    /// The user stopped the app; don't bring it back up.
    pub stopped: bool,
    pub status: AppStatus,
    /// Name of the container currently serving traffic.
    pub container: Option<String>,
    pub image: Option<String>,
    pub builder: Option<String>,
    pub revision: Option<String>,
    pub deployed_at: Option<i64>,
    pub last_error: Option<String>,
}

impl AppRecord {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            name: row.get("name")?,
            source_dir: PathBuf::from(row.get::<_, String>("source_dir")?),
            public: row.get("public")?,
            stopped: row.get("stopped")?,
            status: AppStatus::parse(&row.get::<_, String>("status")?),
            container: row.get("container")?,
            image: row.get("image")?,
            builder: row.get("builder")?,
            revision: row.get("revision")?,
            deployed_at: row.get("deployed_at")?,
            last_error: row.get("last_error")?,
        })
    }
}

/// What a successful deploy leaves behind.
#[derive(Debug, Clone)]
pub struct Deployed {
    pub container: String,
    pub image: String,
    pub builder: String,
    pub revision: Option<String>,
}

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let conn =
            Connection::open(path).with_context(|| format!("could not open {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        for (index, migration) in MIGRATIONS.iter().enumerate().skip(version as usize) {
            conn.execute_batch(migration)
                .with_context(|| format!("database migration {} failed", index + 1))?;
            conn.pragma_update(None, "user_version", index as i64 + 1)?;
        }
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    // Apps

    pub fn apps(&self) -> Result<Vec<AppRecord>> {
        let conn = self.conn();
        let mut statement = conn.prepare("SELECT * FROM apps ORDER BY name")?;
        let apps = statement
            .query_map([], AppRecord::from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(apps)
    }

    pub fn app(&self, name: &str) -> Result<Option<AppRecord>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT * FROM apps WHERE name = ?1",
                [name],
                AppRecord::from_row,
            )
            .optional()?)
    }

    /// Registers an app, or updates its folder if it already exists.
    pub fn upsert_app(&self, name: &str, source_dir: &Path) -> Result<()> {
        self.conn().execute(
            "INSERT INTO apps (name, source_dir, created_at) VALUES (?1, ?2, ?3)
             ON CONFLICT (name) DO UPDATE SET source_dir = excluded.source_dir",
            params![name, source_dir.to_string_lossy(), now()],
        )?;
        Ok(())
    }

    pub fn delete_app(&self, name: &str) -> Result<()> {
        let conn = self.conn();
        conn.execute("DELETE FROM apps WHERE name = ?1", [name])?;
        conn.execute("DELETE FROM env WHERE app = ?1", [name])?;
        Ok(())
    }

    pub fn set_status(&self, name: &str, status: AppStatus, error: Option<&str>) -> Result<()> {
        self.conn().execute(
            "UPDATE apps SET status = ?2, last_error = ?3 WHERE name = ?1",
            params![name, status.as_str(), error],
        )?;
        Ok(())
    }

    pub fn set_deployed(&self, name: &str, deployed: &Deployed) -> Result<()> {
        self.conn().execute(
            "UPDATE apps SET status = 'running', last_error = NULL, stopped = 0,
                container = ?2, image = ?3, builder = ?4, revision = ?5, deployed_at = ?6
             WHERE name = ?1",
            params![
                name,
                deployed.container,
                deployed.image,
                deployed.builder,
                deployed.revision,
                now()
            ],
        )?;
        Ok(())
    }

    pub fn set_container(&self, name: &str, container: Option<&str>) -> Result<()> {
        self.conn().execute(
            "UPDATE apps SET container = ?2 WHERE name = ?1",
            params![name, container],
        )?;
        Ok(())
    }

    pub fn set_public(&self, name: &str, public: Option<bool>) -> Result<()> {
        self.conn().execute(
            "UPDATE apps SET public = ?2 WHERE name = ?1",
            params![name, public],
        )?;
        Ok(())
    }

    pub fn set_stopped(&self, name: &str, stopped: bool) -> Result<()> {
        let status = if stopped { "stopped" } else { "running" };
        self.conn().execute(
            "UPDATE apps SET stopped = ?2, status = ?3 WHERE name = ?1",
            params![name, stopped, status],
        )?;
        Ok(())
    }

    // Env

    pub fn env(&self, app: &str) -> Result<EnvMap> {
        let conn = self.conn();
        let mut statement = conn.prepare("SELECT key, value FROM env WHERE app = ?1")?;
        let env = statement
            .query_map([app], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<EnvMap>>()?;
        Ok(env)
    }

    pub fn update_env(&self, app: &str, set: &EnvMap, unset: &[String]) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        for (key, value) in set {
            tx.execute(
                "INSERT INTO env (app, key, value) VALUES (?1, ?2, ?3)
                 ON CONFLICT (app, key) DO UPDATE SET value = excluded.value",
                params![app, key, value],
            )?;
        }
        for key in unset {
            tx.execute(
                "DELETE FROM env WHERE app = ?1 AND key = ?2",
                params![app, key],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    // Settings

    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn()
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()?)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn().execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn delete_setting(&self, key: &str) -> Result<()> {
        self.conn()
            .execute("DELETE FROM settings WHERE key = ?1", [key])?;
        Ok(())
    }

    /// Returns the setting, creating it with `init` the first time.
    pub fn setting_or_init(&self, key: &str, init: impl FnOnce() -> String) -> Result<String> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2) ON CONFLICT (key) DO NOTHING",
            params![key, init()],
        )?;
        Ok(
            conn.query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| {
                row.get(0)
            })?,
        )
    }

    // Devices

    pub fn add_device(&self, id: &str, grant: &PairGrant, token_hash: &str) -> Result<()> {
        self.conn().execute(
            "INSERT INTO devices (id, label, admin, token_hash, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, grant.label, grant.admin, token_hash, now()],
        )?;
        Ok(())
    }

    /// Every paired device's token hash, id, and role, for the gateway.
    pub fn device_auth(&self) -> Result<Vec<(String, DeviceAuth)>> {
        let conn = self.conn();
        let mut statement = conn.prepare("SELECT token_hash, id, admin FROM devices")?;
        let devices = statement
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    DeviceAuth {
                        id: row.get(1)?,
                        admin: row.get(2)?,
                    },
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(devices)
    }

    pub fn devices(&self) -> Result<Vec<DeviceInfo>> {
        let conn = self.conn();
        let mut statement =
            conn.prepare("SELECT id, label, admin, created_at FROM devices ORDER BY created_at")?;
        let devices = statement
            .query_map([], |row| {
                Ok(DeviceInfo {
                    id: row.get(0)?,
                    label: row.get(1)?,
                    admin: row.get(2)?,
                    created_at: row.get(3)?,
                    current: false,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(devices)
    }

    /// Returns whether a device was removed.
    pub fn remove_device(&self, id: &str) -> Result<bool> {
        Ok(self
            .conn()
            .execute("DELETE FROM devices WHERE id = ?1", [id])?
            > 0)
    }

    // Pairing codes

    pub fn add_pair_code(&self, code_hash: &str, grant: &PairGrant, expires_at: i64) -> Result<()> {
        let conn = self.conn();
        conn.execute("DELETE FROM pair_codes WHERE expires_at < ?1", [now()])?;
        conn.execute(
            "INSERT INTO pair_codes (code_hash, label, admin, expires_at) VALUES (?1, ?2, ?3, ?4)",
            params![code_hash, grant.label, grant.admin, expires_at],
        )?;
        Ok(())
    }

    /// Consumes a pairing code. Returns what it grants if it was valid.
    pub fn take_pair_code(&self, code_hash: &str) -> Result<Option<PairGrant>> {
        let conn = self.conn();
        let found: Option<(PairGrant, i64)> = conn
            .query_row(
                "DELETE FROM pair_codes WHERE code_hash = ?1 RETURNING label, admin, expires_at",
                [code_hash],
                |row| {
                    Ok((
                        PairGrant {
                            label: row.get(0)?,
                            admin: row.get(1)?,
                        },
                        row.get(2)?,
                    ))
                },
            )
            .optional()?;
        Ok(found.and_then(|(grant, expires_at)| (expires_at >= now()).then_some(grant)))
    }
}

/// What a pairing code gives the device that redeems it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PairGrant {
    pub label: Option<String>,
    /// Can use the dashboard (manage deployments), not just open apps.
    pub admin: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceAuth {
    pub id: String,
    pub admin: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_lifecycle() {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_app("budget", Path::new("/apps/budget"))
            .unwrap();
        let app = store.app("budget").unwrap().unwrap();
        assert_eq!(app.status, AppStatus::New);
        assert_eq!(app.public, None);

        store
            .set_deployed(
                "budget",
                &Deployed {
                    container: "hedgehog-budget-1".into(),
                    image: "hedgehog/budget:1".into(),
                    builder: "railpack".into(),
                    revision: Some("abc".into()),
                },
            )
            .unwrap();
        let app = store.app("budget").unwrap().unwrap();
        assert_eq!(app.status, AppStatus::Running);
        assert_eq!(app.container.as_deref(), Some("hedgehog-budget-1"));
        assert!(app.deployed_at.is_some());

        store.set_public("budget", Some(true)).unwrap();
        store.set_stopped("budget", true).unwrap();
        let app = store.app("budget").unwrap().unwrap();
        assert_eq!(app.public, Some(true));
        assert!(app.stopped);
        assert_eq!(app.status, AppStatus::Stopped);

        store.delete_app("budget").unwrap();
        assert!(store.apps().unwrap().is_empty());
    }

    #[test]
    fn env_updates() {
        let store = Store::open_in_memory().unwrap();
        let set = EnvMap::from([("A".into(), "1".into()), ("B".into(), "2".into())]);
        store.update_env("app", &set, &[]).unwrap();
        store
            .update_env(
                "app",
                &EnvMap::from([("A".into(), "3".into())]),
                &["B".into()],
            )
            .unwrap();
        assert_eq!(
            store.env("app").unwrap(),
            EnvMap::from([("A".into(), "3".into())])
        );
    }

    #[test]
    fn settings_init_once() {
        let store = Store::open_in_memory().unwrap();
        let first = store.setting_or_init("k", || "one".into()).unwrap();
        let second = store.setting_or_init("k", || "two".into()).unwrap();
        assert_eq!(first, "one");
        assert_eq!(second, "one");
    }

    #[test]
    fn pair_codes_are_single_use_and_expire() {
        let store = Store::open_in_memory().unwrap();
        let phone = PairGrant {
            label: Some("phone".into()),
            admin: true,
        };
        store.add_pair_code("live", &phone, now() + 60).unwrap();
        store
            .add_pair_code("old", &PairGrant::default(), now() - 1)
            .unwrap();
        assert_eq!(store.take_pair_code("live").unwrap(), Some(phone));
        assert_eq!(store.take_pair_code("live").unwrap(), None);
        assert_eq!(store.take_pair_code("old").unwrap(), None);
        assert_eq!(store.take_pair_code("missing").unwrap(), None);
    }

    #[test]
    fn devices() {
        let store = Store::open_in_memory().unwrap();
        let grant = PairGrant {
            label: Some("phone".into()),
            admin: true,
        };
        store.add_device("d1", &grant, "hash1").unwrap();
        store
            .add_device("d2", &PairGrant::default(), "hash2")
            .unwrap();
        let auth = store.device_auth().unwrap();
        assert!(auth.contains(&(
            "hash1".to_string(),
            DeviceAuth {
                id: "d1".into(),
                admin: true
            }
        )));
        let devices = store.devices().unwrap();
        assert_eq!(devices[0].label.as_deref(), Some("phone"));
        assert!(devices[0].admin);
        assert!(!devices[1].admin);
        assert!(store.remove_device("d1").unwrap());
        assert!(!store.remove_device("d1").unwrap());
    }

    #[test]
    fn migrates_version_one_databases() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(MIGRATIONS[0]).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        conn.execute(
            "INSERT INTO devices (id, label, token_hash, created_at) VALUES ('old', NULL, 'h', 0)",
            [],
        )
        .unwrap();
        let store = Store::init(conn).unwrap();
        let devices = store.devices().unwrap();
        assert_eq!(devices.len(), 1);
        assert!(!devices[0].admin, "existing devices stay app-only");
    }
}
