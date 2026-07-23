//! Connecting to PostgreSQL.
//!
//! Ports `GetConnection` (`db.go:75-97`). Specified in `spec/index.md` §6.
//!
//! Two things this does that fhirbase does not:
//!
//! - It logs a banner reporting the **actual** `sslmode` and never renders the
//!   password (defect X6).
//! - It checks the server version and refuses anything older than PostgreSQL 18
//!   (decision D8), rather than failing later with a confusing SQL error.

use tokio_postgres::Client;

use crate::config::{PgConfig, TlsNegotiation};
use crate::error::{Error, Result};

/// The oldest server this tool supports (decision D8).
///
/// 18 is required, not merely preferred: `uuidv7()` backs identifier generation
/// (D12) and `RETURNING OLD` backs history archival (D13).
pub const MINIMUM_SERVER_VERSION: u32 = 18;

/// Connects to PostgreSQL, honouring the configured `sslmode`.
///
/// The returned client owns its connection task, which is spawned here and runs
/// until the client is dropped.
///
/// # Errors
///
/// Returns [`Error::Db`] if the connection cannot be established, and
/// [`Error::UnsupportedServerVersion`] if the server is older than
/// [`MINIMUM_SERVER_VERSION`].
pub async fn connect(config: &PgConfig) -> Result<Client> {
    let client = connect_without_version_check(config).await?;
    check_server_version(&client).await?;
    println!("Connected to database {}", config.redacted_description());
    Ok(client)
}

/// Connects without asserting the server version.
///
/// Exists so the version check itself can be tested against whatever server is
/// available, and so a future `--no-version-check` escape hatch has somewhere
/// to hook in.
pub async fn connect_without_version_check(config: &PgConfig) -> Result<Client> {
    let mut pg = config.to_pg_config();
    let policy = config.tls();

    match policy.negotiation {
        TlsNegotiation::Never => {
            pg.ssl_mode(tokio_postgres::config::SslMode::Disable);
            spawn(pg.connect(tokio_postgres::NoTls).await)
        }

        TlsNegotiation::TlsFirst => {
            // tokio-postgres implements exactly libpq's `prefer`: offer TLS,
            // fall back to plaintext if the server declines.
            pg.ssl_mode(tokio_postgres::config::SslMode::Prefer);
            let connector = tls_connector(config)?;
            spawn(pg.connect(connector).await)
        }

        TlsNegotiation::Required => {
            pg.ssl_mode(tokio_postgres::config::SslMode::Require);
            let connector = tls_connector(config)?;
            spawn(pg.connect(connector).await)
        }

        TlsNegotiation::PlaintextFirst => {
            // libpq's `allow`: plaintext first, TLS only if that fails.
            // tokio-postgres has no equivalent — its `Prefer` is the opposite
            // order — so try both explicitly.
            let mut plain = pg.clone();
            plain.ssl_mode(tokio_postgres::config::SslMode::Disable);

            match plain.connect(tokio_postgres::NoTls).await {
                Ok(pair) => spawn(Ok(pair)),
                Err(plaintext_error) => {
                    pg.ssl_mode(tokio_postgres::config::SslMode::Require);
                    let connector = tls_connector(config)?;
                    match pg.connect(connector).await {
                        Ok(pair) => spawn(Ok(pair)),
                        Err(tls_error) => Err(Error::Db(format!(
                            "sslmode=allow: plaintext failed ({plaintext_error}), \
                             then TLS failed ({tls_error})"
                        ))),
                    }
                }
            }
        }
    }
}

/// Builds the TLS connector for the configured verification strictness.
///
/// `native-tls` was chosen over `rustls` because its two danger switches map
/// exactly onto libpq's four TLS modes, with no custom certificate verifier to
/// write and get wrong:
///
/// | `sslmode`     | chain verified | hostname verified |
/// | ------------- | -------------- | ----------------- |
/// | `prefer`      | no             | no                |
/// | `require`     | no             | no                |
/// | `verify-ca`   | yes            | no                |
/// | `verify-full` | yes            | yes               |
fn tls_connector(config: &PgConfig) -> Result<postgres_native_tls::MakeTlsConnector> {
    let policy = config.tls();

    let connector = native_tls::TlsConnector::builder()
        .danger_accept_invalid_certs(!policy.verify_certificate)
        .danger_accept_invalid_hostnames(!policy.verify_hostname)
        .build()
        .map_err(|e| Error::Config(format!("cannot build the TLS connector: {e}")))?;

    Ok(postgres_native_tls::MakeTlsConnector::new(connector))
}

/// Spawns the driver's connection task and hands back the client.
///
/// `tokio-postgres` splits a connection into a `Client` and a `Connection`
/// future that must be polled for the client to work at all. Forgetting to
/// spawn it produces a client that hangs on first use, so it is done in one
/// place.
fn spawn<S, T>(
    connected: std::result::Result<(Client, tokio_postgres::Connection<S, T>), tokio_postgres::Error>,
) -> Result<Client>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (client, connection) = connected.map_err(|e| Error::Db(e.to_string()))?;

    tokio::spawn(async move {
        if let Err(e) = connection.await {
            // Not fatal on its own: the command in flight will fail with its
            // own error. Reporting it here explains why.
            eprintln!("database connection closed: {e}");
        }
    });

    Ok(client)
}

/// Refuses a server older than [`MINIMUM_SERVER_VERSION`].
///
/// # Errors
///
/// Returns [`Error::UnsupportedServerVersion`] if the server is too old, and
/// [`Error::Db`] if the version cannot be read.
pub async fn check_server_version(client: &Client) -> Result<()> {
    // `server_version_num` is an integer like 180004 for 18.4, which avoids
    // parsing the display string — that carries distribution noise such as
    // "18.4 (Debian 18.4-1.pgdg13+1)".
    let row = client
        .query_one("SHOW server_version_num", &[])
        .await
        .map_err(|e| Error::Db(format!("cannot read the server version: {e}")))?;

    let raw: &str = row
        .try_get(0)
        .map_err(|e| Error::Db(format!("cannot read the server version: {e}")))?;

    let numeric: u32 = raw
        .trim()
        .parse()
        .map_err(|_| Error::Db(format!("cannot parse the server version {raw:?}")))?;

    let major = numeric / 10_000;
    if major < MINIMUM_SERVER_VERSION {
        return Err(Error::UnsupportedServerVersion {
            found: server_version_display(client).await.unwrap_or_else(|| raw.to_owned()),
        });
    }

    Ok(())
}

/// The server's human-readable version, for error messages only.
async fn server_version_display(client: &Client) -> Option<String> {
    let row = client.query_one("SHOW server_version", &[]).await.ok()?;
    row.try_get::<_, &str>(0).ok().map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Cli;
    use clap::Parser;

    /// The connection string for the test database, if one is configured.
    ///
    /// Set by `podman compose up -d`; see `AGENTS/testing.md`.
    fn test_db() -> Option<String> {
        std::env::var("FHIRPG_TEST_DB").ok()
    }

    fn config_from(argv: &[&str]) -> PgConfig {
        let cli = Cli::try_parse_from(argv).unwrap_or_else(|e| panic!("{e}"));
        PgConfig::from_args(&cli.connection)
    }

    /// Builds a config pointed at the test database.
    fn test_config() -> Option<PgConfig> {
        let dsn = test_db()?;
        let parsed: tokio_postgres::Config = dsn.parse().ok()?;

        let host = parsed.get_hosts().first().map(|h| match h {
            tokio_postgres::config::Host::Tcp(t) => t.clone(),
            tokio_postgres::config::Host::Unix(p) => p.display().to_string(),
        })?;
        let port = parsed.get_ports().first().copied().unwrap_or(5432);
        let user = parsed.get_user().unwrap_or("postgres").to_owned();
        let dbname = parsed.get_dbname().unwrap_or("postgres").to_owned();
        let password = parsed
            .get_password()
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .unwrap_or_default();

        Some(config_from(&[
            "fhirpg",
            "-n",
            &host,
            "-p",
            &port.to_string(),
            "-U",
            &user,
            "-d",
            &dbname,
            "-W",
            &password,
            "-s",
            "disable",
            "init",
        ]))
    }

    #[test]
    fn the_tls_connector_builds_for_every_verification_setting() {
        // Hermetic: builds the connector without connecting to anything.
        for mode in ["prefer", "require", "verify-ca", "verify-full"] {
            let config = config_from(&["fhirpg", "-s", mode, "init"]);
            assert!(
                tls_connector(&config).is_ok(),
                "cannot build a TLS connector for sslmode={mode}"
            );
        }
    }

    #[tokio::test]
    #[ignore = "needs PostgreSQL 18; set FHIRPG_TEST_DB"]
    async fn connects_and_accepts_the_server_version() {
        let Some(config) = test_config() else {
            panic!("FHIRPG_TEST_DB is set but could not be parsed");
        };
        let client = connect(&config).await.expect("should connect");
        let row = client.query_one("SELECT 1 + 1", &[]).await.unwrap();
        let sum: i32 = row.get(0);
        assert_eq!(sum, 2);
    }

    #[tokio::test]
    #[ignore = "needs PostgreSQL 18; set FHIRPG_TEST_DB"]
    async fn the_server_is_postgresql_18_or_newer() {
        let Some(config) = test_config() else {
            panic!("FHIRPG_TEST_DB is set but could not be parsed");
        };
        let client = connect_without_version_check(&config).await.unwrap();
        check_server_version(&client)
            .await
            .expect("decision D8 requires PostgreSQL 18 or newer");

        let row = client.query_one("SHOW server_version_num", &[]).await.unwrap();
        let raw: &str = row.get(0);
        let major: u32 = raw.parse::<u32>().unwrap() / 10_000;
        assert!(major >= MINIMUM_SERVER_VERSION, "server major is {major}");
    }

    #[tokio::test]
    #[ignore = "needs PostgreSQL 18; set FHIRPG_TEST_DB"]
    async fn the_features_decisions_d9_d12_and_d13_rely_on_are_present() {
        let Some(config) = test_config() else {
            panic!("FHIRPG_TEST_DB is set but could not be parsed");
        };
        let client = connect(&config).await.unwrap();

        // D9: gen_random_uuid() is core from PostgreSQL 13, so pgcrypto is not
        // needed. D12: uuidv7() arrived in 18.
        for probe in ["SELECT gen_random_uuid()", "SELECT uuidv7()"] {
            client
                .query_one(probe, &[])
                .await
                .unwrap_or_else(|e| panic!("{probe} failed: {e}"));
        }

        // D13: RETURNING OLD/NEW on an upsert, with OLD null on a true insert.
        client
            .batch_execute("CREATE TEMP TABLE d13_probe (id text primary key, v int)")
            .await
            .unwrap();
        let inserted = client
            .query_one(
                "INSERT INTO d13_probe VALUES ('a', 1) \
                 ON CONFLICT (id) DO UPDATE SET v = EXCLUDED.v \
                 RETURNING old.v AS was, new.v AS now",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(inserted.get::<_, Option<i32>>("was"), None);
        assert_eq!(inserted.get::<_, Option<i32>>("now"), Some(1));

        let updated = client
            .query_one(
                "INSERT INTO d13_probe VALUES ('a', 2) \
                 ON CONFLICT (id) DO UPDATE SET v = EXCLUDED.v \
                 RETURNING old.v AS was, new.v AS now",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(updated.get::<_, Option<i32>>("was"), Some(1));
        assert_eq!(updated.get::<_, Option<i32>>("now"), Some(2));
    }
}
