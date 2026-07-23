//! PostgreSQL connection settings.
//!
//! Ports `PgConnectionConfig` and `GetPgxConnectionConfig`
//! (`db.go:12-73`). Specified in `spec/index.md` §6.
//!
//! Precedence is explicit flag, then environment variable, then default. `clap`
//! gives us that for free through its `env` feature, so this module's job is to
//! translate the resulting settings into a [`tokio_postgres::Config`] plus a
//! [`TlsPolicy`], and to render a **redacted** description for logging.
//!
//! Everything here is a pure function, so all six `sslmode` values are testable
//! without a server.

use crate::cli::{ConnectionArgs, SslMode};

/// How TLS should be negotiated and how strictly the certificate is checked.
///
/// libpq's six `sslmode` values collapse onto two independent questions — is
/// TLS attempted, required, or refused, and is the certificate verified — and
/// this type keeps them separate so the mapping is explicit.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TlsPolicy {
    /// Whether and how hard to try TLS.
    pub negotiation: TlsNegotiation,
    /// Whether the certificate chain must validate.
    pub verify_certificate: bool,
    /// Whether the certificate's hostname must match.
    ///
    /// Only meaningful when `verify_certificate` is true. This is what
    /// separates `verify-ca` from `verify-full`.
    pub verify_hostname: bool,
}

/// Whether TLS is refused, attempted, or required.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TlsNegotiation {
    /// Plaintext only; never offer TLS.
    Never,
    /// Try plaintext first, fall back to TLS (`sslmode=allow`).
    ///
    /// `tokio-postgres` has no equivalent, so `db::connect` implements the
    /// fallback by attempting two connections.
    PlaintextFirst,
    /// Try TLS first, fall back to plaintext (`sslmode=prefer`).
    TlsFirst,
    /// TLS or nothing.
    Required,
}

impl From<SslMode> for TlsPolicy {
    fn from(mode: SslMode) -> Self {
        match mode {
            SslMode::Disable => Self {
                negotiation: TlsNegotiation::Never,
                verify_certificate: false,
                verify_hostname: false,
            },
            SslMode::Allow => Self {
                negotiation: TlsNegotiation::PlaintextFirst,
                verify_certificate: false,
                verify_hostname: false,
            },
            SslMode::Prefer => Self {
                negotiation: TlsNegotiation::TlsFirst,
                verify_certificate: false,
                verify_hostname: false,
            },
            SslMode::Require => Self {
                negotiation: TlsNegotiation::Required,
                verify_certificate: false,
                verify_hostname: false,
            },
            // libpq's verify-ca validates the chain but NOT the hostname.
            //
            // fhirbase gets this wrong: `db.go:54-58` handles "verify-ca" and
            // "verify-full" in one branch, setting `ServerName`, and Go's TLS
            // stack then verifies the hostname too. That makes verify-ca behave
            // as verify-full — stricter than asked for, so a connection libpq
            // would allow is refused. Defect X12.
            SslMode::VerifyCa => Self {
                negotiation: TlsNegotiation::Required,
                verify_certificate: true,
                verify_hostname: false,
            },
            SslMode::VerifyFull => Self {
                negotiation: TlsNegotiation::Required,
                verify_certificate: true,
                verify_hostname: true,
            },
        }
    }
}

/// A resolved set of connection settings.
#[derive(Debug)]
pub struct PgConfig {
    host: String,
    port: u16,
    username: String,
    database: String,
    password: crate::cli::Password,
    tls: TlsPolicy,
}

impl PgConfig {
    /// Builds the configuration from resolved command-line arguments.
    pub fn from_args(args: &ConnectionArgs) -> Self {
        Self {
            host: args.host.clone(),
            port: args.port,
            username: args.username.clone(),
            database: args.database.clone(),
            password: args.password.clone(),
            tls: TlsPolicy::from(args.sslmode),
        }
    }

    /// The TLS policy these settings imply.
    pub fn tls(&self) -> TlsPolicy {
        self.tls
    }

    /// Builds the driver configuration.
    ///
    /// `ssl_mode` is left at the driver's default and handled by
    /// [`crate::db`] instead, because libpq has six modes where
    /// `tokio-postgres` has three.
    pub fn to_pg_config(&self) -> tokio_postgres::Config {
        let mut config = tokio_postgres::Config::new();
        config
            .host(&self.host)
            .port(self.port)
            .user(&self.username)
            .application_name("fhirpg");

        if !self.database.is_empty() {
            config.dbname(&self.database);
        }
        if !self.password.is_empty() {
            config.password(self.password.expose());
        }

        config
    }

    /// A human-readable description of the connection, safe to log.
    ///
    /// fhirbase builds this banner with `sslmode=disable` hardcoded regardless
    /// of the real setting, **and interpolates the password in cleartext**
    /// (`db.go:79-80`, repeated at `web.go:140-141`). That is defect X6. This
    /// reports the actual mode and never renders the password.
    pub fn redacted_description(&self) -> String {
        format!(
            "dbname={} user={} host={} port={} sslmode={} password={}",
            if self.database.is_empty() {
                "<unset>"
            } else {
                &self.database
            },
            self.username,
            self.host,
            self.port,
            self.sslmode_name(),
            self.password,
        )
    }

    /// The libpq name of the effective `sslmode`.
    fn sslmode_name(&self) -> &'static str {
        match (
            self.tls.negotiation,
            self.tls.verify_certificate,
            self.tls.verify_hostname,
        ) {
            (TlsNegotiation::Never, _, _) => "disable",
            (TlsNegotiation::PlaintextFirst, _, _) => "allow",
            (TlsNegotiation::TlsFirst, _, _) => "prefer",
            (TlsNegotiation::Required, false, _) => "require",
            (TlsNegotiation::Required, true, false) => "verify-ca",
            (TlsNegotiation::Required, true, true) => "verify-full",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Password};
    use clap::Parser;

    fn args_from(argv: &[&str]) -> ConnectionArgs {
        Cli::try_parse_from(argv)
            .unwrap_or_else(|e| panic!("{e}"))
            .connection
    }

    #[test]
    fn every_sslmode_maps_to_the_libpq_semantics() {
        // Spec §6. The table is the specification; if a row here changes, the
        // spec changes with it.
        let cases: &[(SslMode, TlsNegotiation, bool, bool, &str)] = &[
            (SslMode::Disable, TlsNegotiation::Never, false, false, "disable"),
            (SslMode::Allow, TlsNegotiation::PlaintextFirst, false, false, "allow"),
            (SslMode::Prefer, TlsNegotiation::TlsFirst, false, false, "prefer"),
            (SslMode::Require, TlsNegotiation::Required, false, false, "require"),
            (SslMode::VerifyCa, TlsNegotiation::Required, true, false, "verify-ca"),
            (SslMode::VerifyFull, TlsNegotiation::Required, true, true, "verify-full"),
        ];

        for &(mode, negotiation, verify_cert, verify_host, name) in cases {
            let policy = TlsPolicy::from(mode);
            assert_eq!(policy.negotiation, negotiation, "{mode:?} negotiation");
            assert_eq!(policy.verify_certificate, verify_cert, "{mode:?} cert");
            assert_eq!(policy.verify_hostname, verify_host, "{mode:?} hostname");

            let args = args_from(&["fhirpg", "-s", name, "init"]);
            let config = PgConfig::from_args(&args);
            assert_eq!(config.sslmode_name(), name, "round trip for {name}");
        }
    }

    #[test]
    fn verify_ca_does_not_check_the_hostname() {
        // Defect X12: fhirbase folds verify-ca into verify-full, so it checks
        // the hostname too and refuses connections libpq would allow.
        let policy = TlsPolicy::from(SslMode::VerifyCa);
        assert!(policy.verify_certificate);
        assert!(
            !policy.verify_hostname,
            "verify-ca must validate the chain but not the hostname"
        );
    }

    #[test]
    fn explicit_flags_win_over_defaults() {
        let args = args_from(&[
            "fhirpg", "-n", "db.example", "-p", "6543", "-U", "alice", "-d", "clinic", "init",
        ]);
        let config = PgConfig::from_args(&args);
        let described = config.redacted_description();
        assert!(described.contains("host=db.example"), "{described}");
        assert!(described.contains("port=6543"), "{described}");
        assert!(described.contains("user=alice"), "{described}");
        assert!(described.contains("dbname=clinic"), "{described}");
    }

    #[test]
    fn an_unset_database_is_shown_as_such() {
        let config = PgConfig::from_args(&args_from(&["fhirpg", "init"]));
        assert!(config.redacted_description().contains("dbname=<unset>"));
    }

    #[test]
    fn the_description_reports_the_real_sslmode() {
        // fhirbase always prints sslmode=disable here, whatever was asked for.
        for name in [
            "disable",
            "allow",
            "prefer",
            "require",
            "verify-ca",
            "verify-full",
        ] {
            let config = PgConfig::from_args(&args_from(&["fhirpg", "-s", name, "init"]));
            let described = config.redacted_description();
            assert!(
                described.contains(&format!("sslmode={name}")),
                "expected sslmode={name} in {described}"
            );
        }
    }

    #[test]
    fn the_password_never_appears_in_any_rendering() {
        // Defect X6, the reason `Password` is a newtype at all.
        let args = args_from(&["fhirpg", "-W", "hunter2", "-d", "clinic", "init"]);
        let config = PgConfig::from_args(&args);

        let rendered = format!(
            "{} {:?} {} {:?}",
            config.redacted_description(),
            config,
            args.password,
            args.password
        );
        assert!(
            !rendered.contains("hunter2"),
            "the password leaked into: {rendered}"
        );
        assert!(config.redacted_description().contains("password=<redacted>"));
    }

    #[test]
    fn the_driver_config_carries_the_settings_through() {
        let args = args_from(&[
            "fhirpg", "-n", "db.example", "-p", "6543", "-U", "alice", "-d", "clinic", "-W",
            "hunter2", "init",
        ]);
        let pg = PgConfig::from_args(&args).to_pg_config();

        assert_eq!(pg.get_ports(), [6543]);
        assert_eq!(pg.get_user(), Some("alice"));
        assert_eq!(pg.get_dbname(), Some("clinic"));
        assert_eq!(pg.get_application_name(), Some("fhirpg"));
        assert_eq!(pg.get_password(), Some("hunter2".as_bytes()));
    }

    #[test]
    fn an_empty_database_or_password_is_omitted_from_the_driver_config() {
        // libpq treats an unset dbname as "same as the user name"; passing an
        // empty string instead would try to open a database literally named "".
        let pg = PgConfig::from_args(&args_from(&["fhirpg", "init"])).to_pg_config();
        assert_eq!(pg.get_dbname(), None);
        assert_eq!(pg.get_password(), None);
    }

    #[test]
    fn a_password_set_only_in_the_environment_is_still_redacted() {
        let password = Password::from("from-env".to_owned());
        assert!(!format!("{password} {password:?}").contains("from-env"));
    }
}
