//! Uptime Kuma's GameDig monitors. Games that GameDig queries with Valve's
//! A2S protocol become Steam monitors; Minecraft (Java and Bedrock),
//! Satisfactory, FiveM/RedM and Mumble become game server monitors; the
//! rest are skipped with the game's name. Kuma 1.x stored GameDig 4.x ids
//! and 2.x stores 5.x ids, so both are looked up.

use serde_json::{Map, Value, json};

use super::{Row, b, host_port, raw, s};

mod games;

/// How GameDig queries a game (only the protocols that matter here are named).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// Source/A2S, which the Steam type speaks.
    Valve,
    Minecraft,
    Bedrock,
    Satisfactory,
    FiveM,
    /// `mumbleping`: the standard UDP ping.
    Mumble,
    /// `mumble`: the GTmurmur plugin's TCP query.
    MumbleGt,
    Other(&'static str),
}

/// One GameDig game.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Game {
    /// GameDig 5.x id.
    pub id: &'static str,
    pub name: &'static str,
    pub protocol: Protocol,
    /// Usual game (connect) port; 0 = none listed.
    pub port: u16,
    /// Query port independent of the game port; 0 = none.
    pub query_port: u16,
    /// Query port = game port + offset (GameDig tries them in order).
    pub query_offset: &'static [i32],
}

/// The game for a GameDig 5.x or 4.x id (case-insensitive).
pub fn lookup(id: &str) -> Option<&'static Game> {
    let id = id.trim().to_ascii_lowercase();
    let find = |id: &str| {
        games::GAMES
            .binary_search_by(|g| g.id.cmp(id))
            .ok()
            .map(|n| &games::GAMES[n])
    };
    find(&id).or_else(|| {
        let n = games::ALIASES
            .binary_search_by(|(old, _)| old.cmp(&id.as_str()))
            .ok()?;
        find(games::ALIASES[n].1)
    })
}

fn port_plus(port: u16, offset: i32) -> Option<u16> {
    u16::try_from(i32::from(port) + offset)
        .ok()
        .filter(|p| *p != 0)
}

/// The port a Steam monitor should query for `game`. GameDig queries the
/// given port alone when `given_only` is set (Kuma's default); otherwise it
/// tries the game's query port first, which is what the import keeps
/// (with a warning, since the Steam type queries one port only).
pub fn steam_port(
    game: &Game,
    port: Option<u16>,
    given_only: bool,
    warnings: &mut Vec<String>,
) -> u16 {
    let offset = game.query_offset.first().copied().unwrap_or(0);
    let Some(port) = port else {
        // No port: GameDig's own default.
        return [game.query_port]
            .into_iter()
            .chain(
                (game.port != 0)
                    .then(|| port_plus(game.port, offset))
                    .flatten(),
            )
            .find(|p| *p != 0)
            .unwrap_or(27015);
    };
    if given_only {
        return port;
    }
    let others = |skip: usize| -> String {
        let rest: Vec<String> = game
            .query_offset
            .iter()
            .skip(skip)
            .filter_map(|o| port_plus(port, *o))
            .map(|p| p.to_string())
            .collect();
        if rest.is_empty() {
            String::new()
        } else {
            format!(" (GameDig would also try {})", rest.join(", "))
        }
    };
    if offset != 0
        && let Some(query) = port_plus(port, offset)
    {
        warnings.push(format!(
            "GameDig was allowed to look beyond port {port}, so it queries {} on port {query} (game port {offset:+}){}; the Steam monitor checks port {query} only.",
            game.name,
            others(1)
        ));
        return query;
    }
    if game.query_port != 0 && game.query_port != port && port == game.port {
        warnings.push(format!(
            "GameDig was allowed to look beyond port {port}, so it also tries {}'s usual query port {}; the Steam monitor checks port {} only.",
            game.name, game.query_port, game.query_port
        ));
        return game.query_port;
    }
    port
}

/// Maps a Kuma `gamedig` row to our type and target. `Err` = why it's skipped.
pub(super) fn map(
    row: &Row,
    hostname: &str,
    port: Option<i64>,
    opts: &mut Map<String, Value>,
    warnings: &mut Vec<String>,
) -> Result<(&'static str, String), String> {
    let id = s(row, &["game"]).ok_or("It has no game set.")?;
    let game = lookup(&id).ok_or_else(|| format!("GameDig game “{id}” isn’t known."))?;
    let host = hostname.trim();
    if host.is_empty() {
        return Err("It has no host name.".into());
    }
    let port = port.and_then(|p| u16::try_from(p).ok()).filter(|p| *p != 0);
    let given_only = b(row, &["gamedig_given_port_only", "gamedigGivenPortOnly"]).unwrap_or(true);
    let token = raw(row, &["gamedig_token", "gamedigToken"]);
    // A bare IPv6 address needs brackets in a URL-style target even without a port.
    let with_port = |p: Option<u16>| match p {
        None if host.contains(':') && !host.starts_with('[') => format!("[{host}]"),
        p => host_port(host, p.map(i64::from)),
    };
    let (kind, target) = match game.protocol {
        Protocol::Valve => (
            "steam",
            with_port(Some(steam_port(game, port, given_only, warnings))),
        ),
        // Without a port (or with the default one) a name gets the game's SRV lookup, as in GameDig.
        Protocol::Minecraft => (
            "game_server",
            format!("minecraft://{}", with_port(port.filter(|p| *p != 25565))),
        ),
        Protocol::Bedrock => ("game_server", format!("bedrock://{}", with_port(port))),
        Protocol::Satisfactory => ("game_server", format!("satisfactory://{}", with_port(port))),
        Protocol::FiveM => ("game_server", format!("fivem://{}", with_port(port))),
        Protocol::Mumble => ("game_server", format!("mumble://{}", with_port(port))),
        Protocol::MumbleGt => {
            warnings.push("it’s checked with Mumble’s standard UDP ping on the voice port instead of the GTmurmur plugin’s query.".into());
            ("game_server", format!("mumble://{}", with_port(port)))
        }
        Protocol::Other(proto) => {
            return Err(format!(
                "GameDig game “{}” ({}) is queried with its {proto} protocol, which isn’t supported. Supported: Valve/Source games (as Steam monitors), Minecraft Java and Bedrock, Satisfactory, FiveM/RedM and Mumble.",
                game.name, game.id
            ));
        }
    };
    match token {
        Some(token) if game.protocol == Protocol::Satisfactory => {
            opts.insert("secret".into(), json!(token));
        }
        Some(_) => warnings.push(format!(
            "its GameDig token isn’t used by the {} check.",
            game.name
        )),
        None => {}
    }
    if port.is_none() {
        warnings.push(format!(
            "it has no port, so {}’s usual port is used.",
            game.name
        ));
    }
    Ok((kind, target))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_are_sorted_for_binary_search() {
        assert!(games::GAMES.windows(2).all(|w| w[0].id < w[1].id));
        assert!(games::ALIASES.windows(2).all(|w| w[0].0 < w[1].0));
        assert!(
            games::ALIASES.iter().all(
                |(old, new)| lookup(old).is_some_and(|g| g.id == *new) && lookup(new).is_some()
            )
        );
        assert_eq!(
            games::GAMES
                .iter()
                .filter(|g| g.protocol == Protocol::Valve)
                .count(),
            137
        );
    }

    #[test]
    fn v4_and_v5_ids_find_the_same_game() {
        for (v4, v5) in [
            ("tf2", "teamfortress2"),
            ("7d2d", "sdtd"),
            ("arkse", "ase"),
            ("minecraftbe", "mbe"),
            ("fivem", "gta5f"),
            ("redm", "rdr2r"),
            ("cs2", "counterstrike2"),
        ] {
            assert_eq!(lookup(v4).map(|g| g.id), Some(v5), "{v4}");
            assert_eq!(lookup(v5).map(|g| g.id), Some(v5), "{v5}");
        }
        // Kuma's own migration wrote these misspelled ids.
        assert_eq!(lookup("theforrest").map(|g| g.name), Some("The Forest"));
        assert_eq!(lookup(" Rust ").map(|g| g.protocol), Some(Protocol::Valve));
        assert!(lookup("nope").is_none());
    }

    #[test]
    fn steam_ports_follow_gamedig() {
        let game = |id| lookup(id).unwrap();
        let mut w = Vec::new();
        // Given port only (Kuma's default): the port as written.
        assert_eq!(steam_port(game("rust"), Some(28015), true, &mut w), 28015);
        assert!(w.is_empty());
        // Otherwise the first query offset.
        assert_eq!(
            steam_port(game("rust"), Some(28015), false, &mut w),
            28015,
            "Rust has no offset"
        );
        assert_eq!(
            steam_port(game("forrest"), Some(27015), false, &mut w),
            27016
        );
        assert!(
            w[0].contains("The Forest on port 27016 (game port +1)"),
            "{w:?}"
        );
        // V Rising lists two offsets.
        assert_eq!(steam_port(game("vrising"), Some(9876), false, &mut w), 9877);
        assert!(w[1].contains("also try 9891"), "{w:?}");
        // Onset's offset is negative.
        assert_eq!(steam_port(game("onset"), Some(7777), false, &mut w), 7776);
        // A fixed query port applies on the game's usual port.
        assert_eq!(steam_port(game("ase"), Some(7777), false, &mut w), 27015);
        assert!(w[3].contains("usual query port 27015"), "{w:?}");
        assert_eq!(steam_port(game("ase"), Some(7779), false, &mut w), 7779);
        // No port at all: GameDig's default.
        assert_eq!(steam_port(game("ase"), None, true, &mut w), 27015);
        assert_eq!(steam_port(game("rust"), None, true, &mut w), 28015);
        assert_eq!(steam_port(game("theforest"), None, false, &mut w), 27016);
        assert_eq!(steam_port(game("brink"), None, true, &mut w), 27015);
        assert_eq!(
            steam_port(game("teamfortress2"), None, false, &mut w),
            27015
        );
        assert_eq!(w.len(), 4);
    }

    #[test]
    fn kuma_gamedig_monitors_import_as_steam_or_game_server() {
        let json = r##"{
          "version": "2.0.0",
          "monitorList": [
            {"id": 1, "name": "CS2", "type": "gamedig", "game": "counterstrike2", "hostname": "cs.example.com", "port": 27015, "gamedigGivenPortOnly": true},
            {"id": 2, "name": "TF2 v4", "type": "gamedig", "game": "tf2", "hostname": "tf.example.com", "port": 27016, "gamedigGivenPortOnly": false},
            {"id": 3, "name": "Forest", "type": "gamedig", "game": "forrest", "hostname": "10.0.0.2", "port": 27015, "gamedigGivenPortOnly": 0},
            {"id": 4, "name": "ARK", "type": "gamedig", "game": "arkse", "hostname": "ark.lan", "port": 7777, "gamedigGivenPortOnly": false},
            {"id": 5, "name": "MC", "type": "gamedig", "game": "minecraft", "hostname": "mc.example.com", "port": 25565},
            {"id": 6, "name": "MC custom", "type": "gamedig", "game": "minecraft", "hostname": "mc.example.com", "port": 25570},
            {"id": 7, "name": "Bedrock", "type": "gamedig", "game": "minecraftbe", "hostname": "be.example.com", "port": 19132},
            {"id": 8, "name": "FiveM", "type": "gamedig", "game": "gta5f", "hostname": "rp.example.com", "port": 30120, "gamedigToken": "unused"},
            {"id": 9, "name": "RedM v4", "type": "gamedig", "game": "redm", "hostname": "2001:db8::9", "port": 30120},
            {"id": 17, "name": "MC v6", "type": "gamedig", "game": "minecraft", "hostname": "2001:db8::17", "port": 25565},
            {"id": 10, "name": "Factory", "type": "gamedig", "game": "satisfactory", "hostname": "factory.lan", "port": 7777, "gamedigToken": " tok "},
            {"id": 11, "name": "Voice", "type": "gamedig", "game": "mumble", "hostname": "voice.lan", "port": 64738},
            {"id": 12, "name": "GT", "type": "gamedig", "game": "mgm", "hostname": "voice.lan"},
            {"id": 13, "name": "BF4", "type": "gamedig", "game": "bf4", "hostname": "bf.example.com", "port": 25200},
            {"id": 14, "name": "Mystery", "type": "gamedig", "game": "zzz", "hostname": "x.example.com", "port": 1},
            {"id": 15, "name": "No game", "type": "gamedig", "hostname": "x.example.com", "port": 1},
            {"id": 16, "name": "No host", "type": "gamedig", "game": "rust", "port": 28015}
          ]
        }"##;
        let import = crate::kuma_import::read_json(json).unwrap();
        let docs = import.doc.monitors.as_ref().unwrap();
        let doc = |name: &str| {
            docs.iter()
                .find(|m| m.name == name)
                .unwrap_or_else(|| panic!("no monitor {name}: {:?}", import.skipped))
        };
        for (name, kind, target) in [
            ("CS2", "steam", "cs.example.com:27015"),
            ("TF2 v4", "steam", "tf.example.com:27016"),
            ("Forest", "steam", "10.0.0.2:27016"),
            ("ARK", "steam", "ark.lan:27015"),
            ("MC", "game_server", "minecraft://mc.example.com"),
            (
                "MC custom",
                "game_server",
                "minecraft://mc.example.com:25570",
            ),
            ("Bedrock", "game_server", "bedrock://be.example.com:19132"),
            ("FiveM", "game_server", "fivem://rp.example.com:30120"),
            ("RedM v4", "game_server", "fivem://[2001:db8::9]:30120"),
            ("MC v6", "game_server", "minecraft://[2001:db8::17]"),
            ("Factory", "game_server", "satisfactory://factory.lan:7777"),
            ("Voice", "game_server", "mumble://voice.lan:64738"),
            ("GT", "game_server", "mumble://voice.lan"),
        ] {
            let d = doc(name);
            assert_eq!(
                (d.monitor_type.as_str(), d.target.as_str()),
                (kind, target),
                "{name}"
            );
            let parsed = crate::db::models::MonitorType::parse(kind).unwrap();
            assert!(
                crate::api::monitors::form::validate_target(parsed, target).is_ok(),
                "{name}: {target}"
            );
        }
        assert_eq!(doc("Factory").options["secret"], " tok ");
        assert!(!doc("FiveM").options.contains_key("secret"));

        let warned = |name: &str, text: &str| {
            import
                .warnings
                .iter()
                .any(|w| w.starts_with(&format!("“{name}”")) && w.contains(text))
        };
        assert!(
            warned("Forest", "queries The Forest on port 27016"),
            "{:?}",
            import.warnings
        );
        assert!(
            warned("ARK", "usual query port 27015"),
            "{:?}",
            import.warnings
        );
        assert!(warned("FiveM", "token isn’t used"), "{:?}", import.warnings);
        assert!(
            warned("GT", "GTmurmur") && warned("GT", "no port"),
            "{:?}",
            import.warnings
        );
        assert!(
            !import.warnings.iter().any(|w| w.starts_with("“CS2”")
                || w.starts_with("“TF2 v4”")
                || w.starts_with("“MC”")),
            "{:?}",
            import.warnings
        );

        let reason = |name: &str| {
            import
                .skipped
                .iter()
                .find(|s| s.name == name)
                .map(|s| s.reason.clone())
                .unwrap_or_default()
        };
        assert!(
            reason("BF4").contains("“Battlefield 4” (battlefield4)")
                && reason("BF4").contains("battlefield protocol"),
            "{}",
            reason("BF4")
        );
        assert!(reason("Mystery").contains("“zzz” isn’t known"));
        assert!(reason("No game").contains("no game"));
        assert!(reason("No host").contains("no host"));
    }
}
