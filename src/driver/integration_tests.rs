//! Driver tests against real servers running in Docker.
//!
//! Everything else in the suite tests pure functions. These test the seams
//! the unit tests cannot reach: the wire protocol each driver actually
//! speaks, the metadata queries, and — the reason this file exists — whether
//! `ssl_mode = "require"` and `"verify"` really behave differently, which
//! fails *silently* when it is wrong.
//!
//! Every test is `#[ignore]`d, so `cargo test` stays hermetic and offline.
//! Run them with:
//!
//! ```text
//! cargo test --  --ignored --test-threads=2
//! ```
//!
//! Containers are torn down when the handle drops, including on panic.

use std::net::{IpAddr, Ipv4Addr};

use rcgen::{
    BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair, KeyUsagePurpose, SanType,
};
use testcontainers::{
    ContainerAsync, GenericImage, ImageExt,
    core::{ContainerPort, IntoContainerPort, WaitFor},
    runners::AsyncRunner,
};

use crate::config::{ConnectionConfig, DriverType, SslMode};
use crate::driver::{CollectionRef, Driver, Namespace, Page};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A connection pointing at a container's mapped port on the loopback host.
///
/// `host` is `localhost` rather than `127.0.0.1` because the TLS tests need a
/// name the server certificate can carry as a SAN.
fn cfg(driver: DriverType, port: u16) -> ConnectionConfig {
    ConnectionConfig {
        name: "it".to_string(),
        driver,
        host: "localhost".to_string(),
        port: Some(port),
        user: None,
        password: None,
        database: None,
        socket: None,
        ssl: false,
        ssl_mode: None,
        ssl_ca: None,
        ssl_cert: None,
        ssl_key: None,
        ssh: None,
    }
}

/// A self-signed CA plus a server certificate for `localhost` / `127.0.0.1`,
/// written to a scratch directory. Returns `(dir, ca_pem, cert_pem, key_pem)`.
///
/// Generated per run: no key material lives in the repository, and an expired
/// checked-in certificate can never rot the suite.
fn self_signed_pair() -> (tempdir::Dir, String, String, String) {
    let mut ca_params = CertificateParams::new(Vec::new()).expect("ca params");
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "dbx integration test CA");
    let ca_key = KeyPair::generate().expect("ca key");
    let ca_cert = ca_params.self_signed(&ca_key).expect("self-signed ca");

    let mut srv_params =
        CertificateParams::new(vec!["localhost".to_string()]).expect("server params");
    srv_params
        .subject_alt_names
        .push(SanType::IpAddress(IpAddr::V4(Ipv4Addr::LOCALHOST)));
    srv_params
        .distinguished_name
        .push(DnType::CommonName, "localhost");
    let srv_key = KeyPair::generate().expect("server key");
    let issuer = Issuer::from_params(&ca_params, &ca_key);
    let srv_cert = srv_params
        .signed_by(&srv_key, &issuer)
        .expect("server cert signed by ca");

    let dir = tempdir::Dir::new("tls");
    (
        dir,
        ca_cert.pem(),
        srv_cert.pem(),
        srv_key.serialize_pem(),
    )
}

/// A scratch directory that deletes itself, so a failing test cannot leave
/// private keys behind in `$TMPDIR`.
mod tempdir {
    pub struct Dir(pub std::path::PathBuf);

    impl Dir {
        pub fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "dbx-it-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            std::fs::create_dir_all(&path).expect("scratch dir");
            Self(path)
        }

        /// Write `content` and return the absolute path, as a `String` for the
        /// config fields (which hold paths, not bytes).
        pub fn write(&self, name: &str, content: &str) -> String {
            let p = self.0.join(name);
            std::fs::write(&p, content).expect("write fixture");
            p.to_string_lossy().into_owned()
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

/// Retry an async connect until it succeeds or the deadline passes.
///
/// Preferred over a log-line wait strategy: ClickHouse logs to files rather
/// than stdout, and "the port answers" is what the test actually needs.
async fn connect_retry<T, F, Fut>(what: &str, mut f: F) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    let mut last = String::new();
    while std::time::Instant::now() < deadline {
        match f().await {
            Ok(v) => return v,
            Err(e) => {
                last = format!("{e:#}");
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        }
    }
    panic!("{what} never became reachable: {last}");
}

/// Start a container and return it with its mapped host port.
async fn start(
    image: testcontainers::ContainerRequest<GenericImage>,
    port: ContainerPort,
) -> (ContainerAsync<GenericImage>, u16) {
    let container = image.start().await.expect(
        "failed to start container — these tests need Docker (they are #[ignore]d by default)",
    );
    let host_port = container
        .get_host_port_ipv4(port)
        .await
        .expect("container port not mapped");
    (container, host_port)
}

// ---------------------------------------------------------------------------
// Redis
// ---------------------------------------------------------------------------

fn redis_image() -> testcontainers::ContainerRequest<GenericImage> {
    GenericImage::new("redis", "7-alpine")
        .with_exposed_port(6379.tcp())
        .with_wait_for(WaitFor::message_on_stdout("Ready to accept connections"))
        .into()
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn redis_browses_and_executes() {
    let (_c, port) = start(redis_image(), 6379.tcp()).await;
    let drv = crate::driver::redis::RedisDriver::connect(&cfg(DriverType::Redis, port))
        .await
        .expect("connect");

    drv.ping().await.expect("ping");

    // Seed through the console path, which is also what we want to exercise.
    let db0 = Namespace("db0".to_string());
    drv.execute(&db0, "SET user:1 alice").await.expect("set");
    drv.execute(&db0, "SET user:2 bob").await.expect("set");
    drv.execute(&db0, r#"SET plain "a;b""#).await.expect("set");

    // A `;` inside a quoted value is data — the exact regression the
    // line-based splitter had to fix.
    let got = drv.execute(&db0, "GET plain").await.expect("get");
    assert!(
        format!("{:?}", got.records).contains("a;b"),
        "semicolon in value was mangled: {:?}",
        got.records
    );

    // Keys group into collections by their first `:` segment.
    let collections = drv.collections(&db0).await.expect("collections");
    assert!(
        collections.iter().any(|c| c.name == "user"),
        "no `user` prefix group in {collections:?}"
    );

    let page = drv
        .records(
            &CollectionRef {
                namespace: db0.clone(),
                name: "user".to_string(),
            },
            Page {
                offset: 0,
                limit: 10,
            },
        )
        .await
        .expect("records");
    assert_eq!(page.records.len(), 2, "got {:?}", page.records);
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn redis_select_is_refused_so_the_shared_connection_stays_put() {
    // Every pane multiplexes one socket; a console SELECT would move the
    // explorer's database with it.
    let (_c, port) = start(redis_image(), 6379.tcp()).await;
    let drv = crate::driver::redis::RedisDriver::connect(&cfg(DriverType::Redis, port))
        .await
        .expect("connect");

    let err = match drv.execute(&Namespace("db0".to_string()), "SELECT 3").await {
        Ok(_) => panic!("SELECT must be refused"),
        Err(e) => e,
    };
    assert!(
        err.to_string().contains("explorer tree"),
        "unexpected error: {err}"
    );
}

/// Redis speaking TLS with our self-signed CA. The official image is built
/// with TLS support, so it only needs the certificates and the flags.
fn redis_tls_image(
    dir: &tempdir::Dir,
    ca_pem: &str,
    cert_pem: &str,
    key_pem: &str,
) -> testcontainers::ContainerRequest<GenericImage> {
    let _ = dir;
    GenericImage::new("redis", "7-alpine")
        .with_exposed_port(6379.tcp())
        .with_wait_for(WaitFor::message_on_stdout("Ready to accept connections"))
        .with_copy_to("/tls/ca.crt", ca_pem.as_bytes().to_vec())
        .with_copy_to("/tls/server.crt", cert_pem.as_bytes().to_vec())
        .with_copy_to("/tls/server.key", key_pem.as_bytes().to_vec())
        .with_cmd([
            "redis-server",
            // Plain port off: a test that accidentally connects in the clear
            // must fail, not quietly pass.
            "--port",
            "0",
            "--tls-port",
            "6379",
            "--tls-cert-file",
            "/tls/server.crt",
            "--tls-key-file",
            "/tls/server.key",
            "--tls-ca-cert-file",
            "/tls/ca.crt",
            "--tls-auth-clients",
            "no",
        ])
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn redis_tls_modes_actually_differ() {
    let (dir, ca_pem, cert_pem, key_pem) = self_signed_pair();
    let ca_path = dir.write("ca.pem", &ca_pem);
    let (_c, port) = start(
        redis_tls_image(&dir, &ca_pem, &cert_pem, &key_pem),
        6379.tcp(),
    )
    .await;

    // 1. No TLS at all: the server only speaks TLS, so this must fail.
    let plain = crate::driver::redis::RedisDriver::connect(&cfg(DriverType::Redis, port)).await;
    assert!(plain.is_err(), "plaintext connect should not succeed");

    // 2. `verify` with no CA: the certificate is self-signed, so validating
    //    it against the system trust store must fail.
    let mut verify_no_ca = cfg(DriverType::Redis, port);
    verify_no_ca.ssl_mode = Some(SslMode::Verify);
    let err = match crate::driver::redis::RedisDriver::connect(&verify_no_ca).await {
        Ok(_) => panic!("verify without the CA must fail"),
        Err(e) => e.to_string(),
    };
    assert!(
        err.contains("certificate") || err.contains("UnknownIssuer") || err.contains("TLS"),
        "expected a certificate error, got: {err}"
    );

    // 3. `require`: encrypted, certificate not checked — the whole point of
    //    the mode, and what makes a self-signed server usable.
    let mut require = cfg(DriverType::Redis, port);
    require.ssl_mode = Some(SslMode::Require);
    let drv = crate::driver::redis::RedisDriver::connect(&require)
        .await
        .expect("require must connect to a self-signed server");
    drv.ping().await.expect("ping over require-TLS");

    // 4. `verify` + our CA: validated, and it passes.
    let mut verify = cfg(DriverType::Redis, port);
    verify.ssl_mode = Some(SslMode::Verify);
    verify.ssl_ca = Some(ca_path);
    let drv = crate::driver::redis::RedisDriver::connect(&verify)
        .await
        .expect("verify with the issuing CA must connect");
    drv.ping().await.expect("ping over verified TLS");
}

// ---------------------------------------------------------------------------
// MongoDB
// ---------------------------------------------------------------------------

/// Seeded through the image's init hook rather than a later `exec`: the hook
/// is guaranteed to finish before the server accepts outside connections.
const MONGO_SEED_JS: &str = r#"
db = db.getSiblingDB("shop");
db.users.insertMany([
  { name: "alice", age: 30 },
  { name: "bob", age: 20 }
]);
"#;

fn mongo_image() -> testcontainers::ContainerRequest<GenericImage> {
    GenericImage::new("mongo", "7")
        .with_exposed_port(27017.tcp())
        .with_wait_for(WaitFor::seconds(1))
        .with_copy_to(
            "/docker-entrypoint-initdb.d/seed.js",
            MONGO_SEED_JS.as_bytes().to_vec(),
        )
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn mongo_infers_schema_and_runs_the_console() {
    let (_c, port) = start(mongo_image(), 27017.tcp()).await;
    let mut c = cfg(DriverType::MongoDB, port);
    c.database = Some("shop".to_string());
    // The seeded collection is the readiness signal: the init hook runs
    // before the server takes outside connections, so once `users` is
    // listable the fixture is complete.
    let drv = connect_retry("mongo", || {
        let c = c.clone();
        async move {
            let drv = crate::driver::mongo::MongoDriver::connect(&c).await?;
            let found = drv.collections(&Namespace("shop".to_string())).await?;
            if found.iter().any(|c| c.name == "users") {
                Ok(drv)
            } else {
                anyhow::bail!("seed collection not visible yet")
            }
        }
    })
    .await;

    let shop = Namespace("shop".to_string());
    let collections = drv.collections(&shop).await.expect("collections");
    assert!(
        collections.iter().any(|c| c.name == "users"),
        "seeded collection missing: {collections:?}"
    );

    let cref = CollectionRef {
        namespace: shop.clone(),
        name: "users".to_string(),
    };
    let meta = drv.collection_meta(&cref).await.expect("meta");
    assert!(
        meta.columns.iter().any(|col| col.name == "name"),
        "schema inference missed a field: {:?}",
        meta.columns
    );

    let res = drv
        .execute(
            &shop,
            r#"{"collection": "users", "find": {"filter": {"age": {"$gte": 25}}}}"#,
        )
        .await
        .expect("console find");
    assert_eq!(res.records.len(), 1, "got {:?}", res.records);

    // The read-only promise, enforced against a live server.
    let err = match drv
        .execute(
            &shop,
            r#"{"collection": "users", "aggregate": [{"$match": {}}, {"$out": "copy"}]}"#,
        )
        .await
    {
        Ok(_) => panic!("$out must be rejected"),
        Err(e) => e,
    };
    assert!(
        err.to_string().contains("writes to a collection"),
        "unexpected error: {err}"
    );
}

// ---------------------------------------------------------------------------
// ClickHouse
// ---------------------------------------------------------------------------

/// The image disables network access for `default` unless credentials are
/// given, so every ClickHouse container here logs in as this user.
const CH_USER: &str = "dbx";
const CH_PASSWORD: &str = "dbx-test";

fn clickhouse_image() -> testcontainers::ContainerRequest<GenericImage> {
    GenericImage::new("clickhouse/clickhouse-server", "24.3-alpine")
        .with_exposed_port(8123.tcp())
        // ClickHouse logs to files, not stdout — readiness is polled through
        // the driver instead (see `connect_retry`).
        .with_wait_for(WaitFor::seconds(1))
        .with_env_var("CLICKHOUSE_USER", CH_USER)
        .with_env_var("CLICKHOUSE_PASSWORD", CH_PASSWORD)
}

/// A ClickHouse connection carrying the container's credentials.
fn ch_cfg(port: u16) -> ConnectionConfig {
    let mut c = cfg(DriverType::ClickHouse, port);
    c.user = Some(CH_USER.to_string());
    c.password = Some(CH_PASSWORD.to_string());
    c
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn clickhouse_creates_browses_and_types_columns() {
    let (_c, port) = start(clickhouse_image(), 8123.tcp()).await;
    let conn = ch_cfg(port);
    let drv = connect_retry("clickhouse", || {
        let conn = conn.clone();
        async move { crate::driver::clickhouse::ClickHouseDriver::connect(&conn).await }
    })
    .await;

    let default = Namespace("default".to_string());
    drv.execute(
        &default,
        "CREATE TABLE events (id UInt64, city String, at DateTime) ENGINE = MergeTree ORDER BY id",
    )
    .await
    .expect("create");
    drv.execute(
        &default,
        "INSERT INTO events VALUES (1, 'jakarta', now()), (2, 'bandung', now())",
    )
    .await
    .expect("insert");

    let collections = drv.collections(&default).await.expect("collections");
    assert!(
        collections.iter().any(|c| c.name == "events"),
        "created table missing: {collections:?}"
    );

    let meta = drv
        .collection_meta(&CollectionRef {
            namespace: default.clone(),
            name: "events".to_string(),
        })
        .await
        .expect("meta");
    let city = meta
        .columns
        .iter()
        .find(|c| c.name == "city")
        .expect("city column");
    assert_eq!(city.data_type, "String");

    let res = drv
        .execute(&default, "SELECT count() AS n FROM events")
        .await
        .expect("select");
    assert_eq!(res.records.len(), 1);
}

/// ClickHouse with HTTPS enabled, using the same generated CA.
fn clickhouse_tls_image(
    cert_pem: &str,
    key_pem: &str,
) -> testcontainers::ContainerRequest<GenericImage> {
    const TLS_XML: &str = r#"<clickhouse>
    <https_port>8443</https_port>
    <openSSL>
        <server>
            <certificateFile>/tls/server.crt</certificateFile>
            <privateKeyFile>/tls/server.key</privateKeyFile>
            <disableProtocols>sslv2,sslv3</disableProtocols>
        </server>
    </openSSL>
</clickhouse>
"#;
    GenericImage::new("clickhouse/clickhouse-server", "24.3-alpine")
        .with_exposed_port(8443.tcp())
        .with_wait_for(WaitFor::seconds(1))
        .with_env_var("CLICKHOUSE_USER", CH_USER)
        .with_env_var("CLICKHOUSE_PASSWORD", CH_PASSWORD)
        .with_copy_to("/tls/server.crt", cert_pem.as_bytes().to_vec())
        .with_copy_to("/tls/server.key", key_pem.as_bytes().to_vec())
        .with_copy_to(
            "/etc/clickhouse-server/config.d/dbx-tls.xml",
            TLS_XML.as_bytes().to_vec(),
        )
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn clickhouse_tls_modes_actually_differ() {
    let (dir, ca_pem, cert_pem, key_pem) = self_signed_pair();
    let ca_path = dir.write("ca.pem", &ca_pem);
    let (_c, port) = start(clickhouse_tls_image(&cert_pem, &key_pem), 8443.tcp()).await;

    // Wait for the server by connecting the way the app would, over the
    // very mode the test then exercises.
    let mut warm = ch_cfg(port);
    warm.ssl_mode = Some(SslMode::Require);
    connect_retry("clickhouse-tls", || {
        let warm = warm.clone();
        async move { crate::driver::clickhouse::ClickHouseDriver::connect(&warm).await }
    })
    .await;

    // `verify` with no CA: self-signed, so validation must fail.
    let mut verify_no_ca = ch_cfg(port);
    verify_no_ca.ssl_mode = Some(SslMode::Verify);
    assert!(
        crate::driver::clickhouse::ClickHouseDriver::connect(&verify_no_ca)
            .await
            .is_err(),
        "verify without the CA must fail"
    );

    // `require`: encrypted, certificate unchecked. Before the fix this
    // behaved identically to `verify` and could not reach this server at all.
    let mut require = ch_cfg(port);
    require.ssl_mode = Some(SslMode::Require);
    let drv = crate::driver::clickhouse::ClickHouseDriver::connect(&require)
        .await
        .expect("require must connect to a self-signed server");
    drv.ping().await.expect("ping over require-TLS");

    // `verify` + the issuing CA.
    let mut verify = ch_cfg(port);
    verify.ssl_mode = Some(SslMode::Verify);
    verify.ssl_ca = Some(ca_path);
    let drv = crate::driver::clickhouse::ClickHouseDriver::connect(&verify)
        .await
        .expect("verify with the issuing CA must connect");
    drv.ping().await.expect("ping over verified TLS");
}
