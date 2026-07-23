//! Throwaway databases for the `#[ignore]`d test suite.
//!
//! Only compiled under `cfg(test)`. Each test gets its own database rather than
//! sharing one, because `init` creates 293 tables and asserting on
//! `information_schema` is only meaningful against a database nothing else has
//! touched.
//!
//! # Skip quietly, fail loudly
//!
//! There are two very different situations, and conflating them cost real time:
//!
//! - **`FHIRPG_TEST_DB` is unset.** The caller is on a machine with no
//!   PostgreSQL. [`create`] returns `None`, the test returns early, and
//!   `cargo test` stays hermetic. This is correct and intended.
//! - **`FHIRPG_TEST_DB` is set but something fails.** That is a broken test
//!   harness, and returning `None` would make the entire database suite report
//!   success while executing nothing. [`create`] panics instead, naming the
//!   underlying error.
//!
//! An earlier version returned `None` in both cases. It silently no-opped every
//! database test — including the D13 concurrency test whose result decides a
//! design decision — because `CREATE DATABASE` cannot run inside the implicit
//! transaction that `batch_execute` wraps a multi-statement string in. The
//! suite reported ten passes and had run nothing. [`self_tests`] now guards
//! against exactly that.

use tokio_postgres::Client;

/// The connection string for the test server, if one is configured.
fn dsn() -> Option<String> {
    match std::env::var("FHIRPG_TEST_DB") {
        Ok(value) if !value.trim().is_empty() => Some(value),
        _ => None,
    }
}

/// Connects to a database on the test server by name.
///
/// Panics rather than returning an error: the caller has already established
/// that a test server is configured, so a failure here is a broken harness.
async fn connect_to(dbname: &str, user: Option<(&str, &str)>) -> Client {
    let Some(dsn) = dsn() else {
        panic!("connect_to called with no FHIRPG_TEST_DB configured")
    };

    let mut config: tokio_postgres::Config = dsn
        .parse()
        .unwrap_or_else(|e| panic!("FHIRPG_TEST_DB is not a valid connection string: {e}"));
    config.dbname(dbname);
    if let Some((name, password)) = user {
        config.user(name).password(password);
    }

    let (client, connection) = config
        .connect(tokio_postgres::NoTls)
        .await
        .unwrap_or_else(|e| panic!("cannot connect to database {dbname}: {e}"));

    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

/// A client on the maintenance database, used to create and drop others.
pub async fn maintenance_client() -> Option<Client> {
    dsn()?;
    Some(connect_to("postgres", None).await)
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
        connect_to(&self.name, None).await
    }

    /// Connects to it as a specific role, for permission tests.
    pub async fn connect_as(&self, user: &str, password: &str) -> Client {
        connect_to(&self.name, Some((user, password))).await
    }

    /// Drops the database.
    ///
    /// Explicit rather than a `Drop` impl, because dropping a database is an
    /// async operation and `Drop` cannot await.
    pub async fn drop(self) {
        let Some(client) = maintenance_client().await else {
            return;
        };
        // One statement per call: see the note on `create`.
        let _ = client
            .simple_query(&format!(
                "DROP DATABASE IF EXISTS \"{}\" WITH (FORCE)",
                self.name
            ))
            .await;
    }
}

/// Creates a throwaway database named after the process and `suffix`.
///
/// Returns `None` only when `FHIRPG_TEST_DB` is unset. Any other failure
/// panics — see the module documentation.
pub async fn create(suffix: &str) -> Option<TestDb> {
    let client = maintenance_client().await?;

    // The process id keeps concurrent `cargo test` runs from colliding; the
    // suffix keeps tests within one run from colliding with each other.
    // PostgreSQL identifiers cap at 63 bytes, so keep suffixes short.
    let name = format!("fhirpg_t_{}_{suffix}", std::process::id());
    assert!(
        name.len() <= 63,
        "test database name {name} exceeds PostgreSQL's 63-byte identifier limit"
    );

    // ONE statement per call. `batch_execute` sends a multi-statement string
    // through the simple query protocol, which wraps it in an implicit
    // transaction — and DROP/CREATE DATABASE cannot run inside one.
    for statement in [
        format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"),
        format!("CREATE DATABASE \"{name}\""),
    ] {
        client
            .simple_query(&statement)
            .await
            .unwrap_or_else(|e| panic!("cannot prepare the test database: {statement}: {e}"));
    }

    Some(TestDb { name })
}

#[cfg(test)]
mod self_tests {
    /// Guards against the whole database suite silently no-opping.
    ///
    /// Every database test starts with
    /// `let Some(db) = testdb::create(..) else { return }`, which is what keeps
    /// `cargo test` hermetic — and which would also make a broken helper look
    /// like a passing suite. This test fails loudly instead. It has already
    /// caught that exact failure once.
    #[tokio::test]
    #[ignore = "needs PostgreSQL 18; set FHIRPG_TEST_DB"]
    async fn the_helper_can_actually_create_a_database() {
        assert!(
            std::env::var("FHIRPG_TEST_DB").is_ok(),
            "FHIRPG_TEST_DB must be set to run the --ignored suite"
        );
        let db = super::create("selfcheck")
            .await
            .expect("cannot create a test database; the whole db suite would be a no-op");
        let client = db.connect().await;
        let row = client.query_one("SELECT 42", &[]).await.unwrap();
        assert_eq!(row.get::<_, i32>(0), 42);
        db.drop().await;
    }
}
