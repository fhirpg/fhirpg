//! Throwaway databases for the `#[ignore]`d test suite.
//!
//! Only compiled under `cfg(test)`. Every helper returns `None` when
//! `FHIRPG_TEST_DB` is unset, so a test that uses one degrades to a no-op
//! rather than failing on a machine with no PostgreSQL — `cargo test` must stay
//! hermetic (see `AGENTS/testing.md`).
//!
//! Each test gets its own database rather than sharing one, because `init`
//! creates 293 tables and asserting on `information_schema` is only meaningful
//! against a database nothing else has touched.

use tokio_postgres::Client;

/// The connection string for the test server, if one is configured.
fn dsn() -> Option<String> {
    std::env::var("FHIRPG_TEST_DB").ok()
}

/// Connects to a database on the test server by name.
async fn connect_to(dbname: &str, user: Option<(&str, &str)>) -> Option<Client> {
    let mut config: tokio_postgres::Config = dsn()?.parse().ok()?;
    config.dbname(dbname);
    if let Some((name, password)) = user {
        config.user(name).password(password);
    }

    let (client, connection) = config.connect(tokio_postgres::NoTls).await.ok()?;
    tokio::spawn(async move {
        let _ = connection.await;
    });
    Some(client)
}

/// A client on the maintenance database, used to create and drop others.
pub async fn maintenance_client() -> Option<Client> {
    connect_to("postgres", None).await
}

/// A throwaway database that cleans up after itself.
pub struct TestDb {
    name: String,
}

impl TestDb {
    /// The database's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Connects to it as the configured user.
    pub async fn connect(&self) -> Client {
        match connect_to(&self.name, None).await {
            Some(client) => client,
            None => panic!("cannot connect to the test database {}", self.name),
        }
    }

    /// Connects to it as a specific role, for permission tests.
    pub async fn connect_as(&self, user: &str, password: &str) -> Client {
        match connect_to(&self.name, Some((user, password))).await {
            Some(client) => client,
            None => panic!("cannot connect to {} as {user}", self.name),
        }
    }

    /// Drops the database.
    ///
    /// Explicit rather than a `Drop` impl, because dropping a database is an
    /// async operation and `Drop` cannot await.
    pub async fn drop(self) {
        let Some(client) = maintenance_client().await else {
            return;
        };
        let _ = client
            .batch_execute(&format!(
                "DROP DATABASE IF EXISTS \"{}\" WITH (FORCE)",
                self.name
            ))
            .await;
    }
}

/// Creates a throwaway database named after the process and `suffix`.
///
/// Returns `None` when `FHIRPG_TEST_DB` is unset.
pub async fn create(suffix: &str) -> Option<TestDb> {
    let client = maintenance_client().await?;
    // The process id keeps concurrent `cargo test` runs from colliding; the
    // suffix keeps tests within one run from colliding with each other.
    let name = format!("fhirpg_t_{}_{suffix}", std::process::id());

    client
        .batch_execute(&format!(
            "DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE); CREATE DATABASE \"{name}\""
        ))
        .await
        .ok()?;

    Some(TestDb { name })
}
