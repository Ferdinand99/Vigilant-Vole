//! Targets of the service monitors (Redis, PostgreSQL, MQTT, …):
//! `[scheme://]host[:port][/path][?query]`.
//!
//! The scheme picks plain or TLS (`redis://` / `rediss://`), the path carries
//! what the service calls its "database" (Redis db index, PostgreSQL database,
//! AMQP vhost, gRPC service) and the query holds rare options. MQTT reads its
//! topic from the raw target instead (`mqtt::topic`), since topics use `#`.
//! MongoDB targets follow its connection-string format instead (several
//! hosts, `mongodb+srv://`): see [`parse_mongodb`].
//! Credentials never go into the target: they live in the monitor's user name,
//! password and secret fields, so they can be masked.

use crate::db::models::MonitorType;

/// How a service names itself in a target and where it listens.
pub struct Scheme {
    pub plain: &'static [&'static str],
    pub tls: &'static [&'static str],
    pub port: u16,
    pub tls_port: u16,
}

/// A parsed service target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// The scheme asked for TLS from the first byte.
    pub tls: bool,
    pub host: String,
    pub port: u16,
    /// Percent-decoded path without the leading `/` ("" when absent).
    pub path: String,
    pub query: Vec<(String, String)>,
    /// MongoDB: the other hosts of the seed list (`host` and `port` are the first).
    pub more_hosts: Vec<(String, u16)>,
    /// MongoDB: `mongodb+srv://`, so `host` is a DNS name to look up the
    /// hosts under (and `port` means nothing).
    pub srv: bool,
}

impl Endpoint {
    pub fn query(&self, key: &str) -> Option<&str> {
        self.query.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.as_str())
    }
}

/// What the monitor form shows for a service type.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Fields {
    /// User name + password (`basic_auth_user` / `basic_auth_pass`).
    pub credentials: bool,
    /// The password alone is enough (Redis `requirepass`).
    pub password_only: bool,
    /// `secret`: RADIUS shared secret, SNMP community (v3: privacy password).
    pub secret: bool,
    /// `keyword`: a text the reply must contain.
    pub keyword: bool,
    /// `ignore_tls`.
    pub tls: bool,
}

const NONE: Fields = Fields { credentials: false, password_only: false, secret: false, keyword: false, tls: false };

pub fn fields(kind: MonitorType) -> Fields {
    match kind {
        MonitorType::Tls | MonitorType::Smtp | MonitorType::Kafka | MonitorType::Grpc | MonitorType::GrpcKeyword => Fields { tls: true, ..NONE },
        MonitorType::Redis => Fields { credentials: true, password_only: true, tls: true, ..NONE },
        MonitorType::Postgres | MonitorType::Mysql | MonitorType::Rabbitmq | MonitorType::SqlServer | MonitorType::Mongodb => {
            Fields { credentials: true, tls: true, ..NONE }
        }
        MonitorType::Mqtt => Fields { credentials: true, keyword: true, tls: true, ..NONE },
        MonitorType::Radius => Fields { credentials: true, secret: true, ..NONE },
        // v1/v2c: the secret is the community. v3: user, authentication
        // password and (as the secret) privacy password.
        MonitorType::Snmp => Fields { credentials: true, secret: true, keyword: true, ..NONE },
        // The secret is the Satisfactory API token; the keyword applies to the server name.
        MonitorType::GameServer => Fields { secret: true, keyword: true, ..NONE },
        _ => NONE,
    }
}

pub fn scheme(kind: MonitorType) -> Option<&'static Scheme> {
    const fn s(plain: &'static [&'static str], tls: &'static [&'static str], port: u16, tls_port: u16) -> Scheme {
        Scheme { plain, tls, port, tls_port }
    }
    static REDIS: Scheme = s(&["redis"], &["rediss"], 6379, 6379);
    static POSTGRES: Scheme = s(&["postgres", "postgresql"], &[], 5432, 5432);
    static MYSQL: Scheme = s(&["mysql", "mariadb"], &[], 3306, 3306);
    static MQTT: Scheme = s(&["mqtt"], &["mqtts"], 1883, 8883);
    static KAFKA: Scheme = s(&["kafka"], &["kafkas"], 9092, 9093);
    static AMQP: Scheme = s(&["amqp"], &["amqps"], 5672, 5671);
    static GRPC: Scheme = s(&["grpc"], &["grpcs"], 50051, 443);
    static RADIUS: Scheme = s(&["radius"], &[], 1812, 1812);
    static SNMP: Scheme = s(&["snmp"], &[], 161, 161);
    static SQLSERVER: Scheme = s(&["sqlserver", "mssql"], &[], 1433, 1433);
    static MONGODB: Scheme = s(&["mongodb"], &["mongodb+srv"], 27017, 27017);
    // Each game has its own default port: see `game_server::parse_target`.
    static GAME: Scheme = s(&["minecraft", "bedrock", "satisfactory", "fivem", "mumble"], &[], 25565, 25565);
    match kind {
        MonitorType::Redis => Some(&REDIS),
        MonitorType::Postgres => Some(&POSTGRES),
        MonitorType::Mysql => Some(&MYSQL),
        MonitorType::Mqtt => Some(&MQTT),
        MonitorType::Kafka => Some(&KAFKA),
        MonitorType::Rabbitmq => Some(&AMQP),
        MonitorType::Grpc | MonitorType::GrpcKeyword => Some(&GRPC),
        MonitorType::Radius => Some(&RADIUS),
        MonitorType::Snmp => Some(&SNMP),
        MonitorType::SqlServer => Some(&SQLSERVER),
        MonitorType::Mongodb => Some(&MONGODB),
        MonitorType::GameServer => Some(&GAME),
        _ => None,
    }
}

/// Parse the target of a service monitor of type `kind`.
pub fn parse(kind: MonitorType, target: &str) -> Result<Endpoint, String> {
    let scheme = scheme(kind).ok_or_else(|| "This monitor type has no service target".to_string())?;
    if kind == MonitorType::Mongodb {
        return parse_mongodb(target);
    }
    if kind == MonitorType::GameServer {
        return super::game_server::endpoint(target);
    }
    parse_with(scheme, target)
}

pub fn parse_with(scheme: &Scheme, target: &str) -> Result<Endpoint, String> {
    let t = target.trim();
    if t.is_empty() {
        return Err("Target is empty".into());
    }
    let first = scheme.plain[0];
    let (name, rest) = match t.split_once("://") {
        Some((name, rest)) => (name.to_ascii_lowercase(), rest),
        None => (first.to_string(), t),
    };
    let tls = scheme.tls.contains(&name.as_str());
    if !tls && !scheme.plain.contains(&name.as_str()) {
        let all: Vec<String> = scheme.plain.iter().chain(scheme.tls).map(|s| format!("{s}://")).collect();
        return Err(format!("Unsupported scheme \"{name}://\" (use {})", all.join(" or ")));
    }
    // A fixed scheme makes the rest parse the same way for every service.
    let url = url::Url::parse(&format!("svc://{rest}"))
        .map_err(|_| format!("Invalid address \"{}\"", super::util::truncate(t, 80)))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err("Don't put credentials in the address: use the user name and password fields".into());
    }
    let host = url
        .host_str()
        .map(|h| h.trim_start_matches('[').trim_end_matches(']').to_string())
        .filter(|h| !h.is_empty())
        .ok_or_else(|| format!("Address \"{}\" has no host", super::util::truncate(t, 80)))?;
    if host.chars().any(char::is_whitespace) {
        return Err("The host can't contain spaces".into());
    }
    let port = url.port().unwrap_or(if tls { scheme.tls_port } else { scheme.port });
    let path = percent_decode(url.path().trim_start_matches('/'));
    let query = url.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
    Ok(Endpoint { tls, host, port, path, query, more_hosts: Vec::new(), srv: false })
}

/// A MongoDB connection string without credentials:
/// `mongodb://host1[:port][,host2…][/database][?options]` or
/// `mongodb+srv://name[/database][?options]`. TLS comes from `tls=true`
/// (or `ssl=true`) and is on by default for `mongodb+srv://`.
pub fn parse_mongodb(target: &str) -> Result<Endpoint, String> {
    let t = target.trim();
    if t.is_empty() {
        return Err("Target is empty".into());
    }
    let (srv, rest) = match t.split_once("://") {
        Some((name, rest)) => match name.to_ascii_lowercase().as_str() {
            "mongodb" => (false, rest),
            "mongodb+srv" => (true, rest),
            other => return Err(format!("Unsupported scheme \"{other}://\" (use mongodb:// or mongodb+srv://)")),
        },
        None => (false, t),
    };
    let split = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(split);
    if authority.contains('@') {
        return Err("Don't put credentials in the address: use the user name and password fields".into());
    }
    let (path, query) = match tail.split_once('?') {
        Some((path, query)) => (path, query),
        None => (tail, ""),
    };
    let path = percent_decode(path.trim_start_matches('/'));
    if path.contains('/') {
        return Err("The path is the database name and can't contain \"/\"".into());
    }
    let query: Vec<(String, String)> = url::form_urlencoded::parse(query.as_bytes()).map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
    let mut hosts = Vec::new();
    for raw in authority.split(',') {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err(format!("Address \"{}\" has an empty host", super::util::truncate(t, 80)));
        }
        if raw.chars().any(char::is_whitespace) {
            return Err("The host can't contain spaces".into());
        }
        if srv && raw.trim_start_matches('[').contains(':') {
            return Err("A mongodb+srv:// address is a single host name without a port".into());
        }
        let (host, port) = super::util::split_host_port(raw, Some(27017))?;
        hosts.push((host, port));
    }
    if srv && hosts.len() != 1 {
        return Err("A mongodb+srv:// address is a single host name without a port".into());
    }
    let ep = Endpoint { tls: false, host: String::new(), port: 0, path, query, more_hosts: Vec::new(), srv };
    let flag = |key: &str| ep.query(key).map(|v| v.trim().eq_ignore_ascii_case("true"));
    let tls = flag("tls").or_else(|| flag("ssl")).unwrap_or(srv);
    if flag("directConnection") == Some(true) && (srv || hosts.len() > 1) {
        return Err("directConnection=true needs exactly one host (and no mongodb+srv://)".into());
    }
    let mut hosts = hosts.into_iter();
    let (host, port) = hosts.next().ok_or_else(|| format!("Address \"{}\" has no host", super::util::truncate(t, 80)))?;
    Ok(Endpoint { tls, host, port, more_hosts: hosts.collect(), ..ep })
}

pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = s.get(i + 1..i + 3)
            && let Ok(b) = u8::from_str_radix(hex, 16)
        {
            out.push(b);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_host_uses_the_plain_defaults() {
        let e = parse(MonitorType::Redis, "cache.local").unwrap();
        assert_eq!((e.tls, e.host.as_str(), e.port, e.path.as_str()), (false, "cache.local", 6379, ""));
        let e = parse(MonitorType::Mqtt, "broker:1884").unwrap();
        assert_eq!((e.tls, e.port), (false, 1884));
    }

    #[test]
    fn tls_scheme_and_default_port() {
        let e = parse(MonitorType::Mqtt, "mqtts://broker.example.com").unwrap();
        assert_eq!((e.tls, e.port), (true, 8883));
        let e = parse(MonitorType::Redis, "rediss://cache:6380/3").unwrap();
        assert_eq!((e.tls, e.port, e.path.as_str()), (true, 6380, "3"));
    }

    #[test]
    fn path_is_percent_decoded_and_query_is_split() {
        let e = parse(MonitorType::Rabbitmq, "amqp://mq/%2Fprod").unwrap();
        assert_eq!(e.path, "/prod");
        let e = parse(MonitorType::Mqtt, "mqtt://b/home/sensor/temp").unwrap();
        assert_eq!(e.path, "home/sensor/temp");
        let e = parse(MonitorType::Postgres, "postgres://db:5432/app?sslmode=require").unwrap();
        assert_eq!((e.path.as_str(), e.query("SSLMODE")), ("app", Some("require")));
    }

    #[test]
    fn ipv6_literals_lose_their_brackets() {
        let e = parse(MonitorType::Redis, "[::1]:6380").unwrap();
        assert_eq!((e.host.as_str(), e.port), ("::1", 6380));
    }

    #[test]
    fn bad_targets_are_refused() {
        assert!(parse(MonitorType::Redis, "").is_err());
        assert!(parse(MonitorType::Redis, "http://cache").unwrap_err().contains("Unsupported scheme"));
        assert!(parse(MonitorType::Redis, "redis://user:pw@cache").unwrap_err().contains("credentials"));
        assert!(parse(MonitorType::Redis, "redis://:6379").is_err());
        assert!(parse(MonitorType::Redis, "redis://host:99999").is_err());
        assert!(parse(MonitorType::Http, "x").is_err());
    }

    #[test]
    fn sqlserver_uses_port_1433() {
        let e = parse(MonitorType::SqlServer, "sqlserver://db.local/app?encrypt=strict").unwrap();
        assert_eq!((e.tls, e.host.as_str(), e.port, e.path.as_str(), e.query("Encrypt")), (false, "db.local", 1433, "app", Some("strict")));
        assert_eq!(parse(MonitorType::SqlServer, "mssql://db:14330").unwrap().port, 14330);
    }

    #[test]
    fn mongodb_seed_lists_and_srv_names() {
        let e = parse(MonitorType::Mongodb, "mongodb://a.local,b.local:27018,[::1]/app?replicaSet=rs0&tls=true").unwrap();
        assert_eq!((e.host.as_str(), e.port, e.tls, e.srv, e.path.as_str()), ("a.local", 27017, true, false, "app"));
        assert_eq!(e.more_hosts, vec![("b.local".to_string(), 27018), ("::1".to_string(), 27017)]);
        assert_eq!(e.query("replicaset"), Some("rs0"));

        let e = parse(MonitorType::Mongodb, "mongodb+srv://cluster0.example.net/?authSource=admin").unwrap();
        assert_eq!((e.host.as_str(), e.tls, e.srv, e.path.as_str()), ("cluster0.example.net", true, true, ""));
        let e = parse(MonitorType::Mongodb, "mongodb+srv://cluster0.example.net/?tls=false").unwrap();
        assert!(!e.tls, "tls=false turns the SRV default off");
        let e = parse(MonitorType::Mongodb, "mongo.local").unwrap();
        assert_eq!((e.host.as_str(), e.port, e.tls), ("mongo.local", 27017, false));
        assert!(parse(MonitorType::Mongodb, "mongodb://h?ssl=true").unwrap().tls);
    }

    #[test]
    fn bad_mongodb_targets_are_refused() {
        let err = |t: &str| parse(MonitorType::Mongodb, t).unwrap_err();
        assert!(err("mongodb://user:pw@h").contains("credentials"));
        assert!(err("mongodb+srv://h.example.com:27017").contains("without a port"));
        assert!(err("mongodb+srv://a.example.com,b.example.com").contains("without a port"));
        assert!(err("mongodb://a,,b").contains("empty host"));
        assert!(err("http://h").contains("Unsupported scheme"));
        assert!(err("mongodb://a,b/?directConnection=true").contains("directConnection"));
        assert!(err("mongodb://a:99999").contains("port"));
        assert!(err("").contains("empty"));
    }

    #[test]
    fn every_service_type_has_a_scheme() {
        for kind in MonitorType::all() {
            let service = fields(*kind) != NONE && !matches!(kind, MonitorType::Tls | MonitorType::Smtp);
            assert_eq!(scheme(*kind).is_some(), service || matches!(kind, MonitorType::Kafka | MonitorType::Grpc), "{kind}");
        }
    }
}
