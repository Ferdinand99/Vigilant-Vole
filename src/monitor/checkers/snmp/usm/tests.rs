use super::*;

fn unhex(s: &str) -> Vec<u8> {
    hex::decode(s.split_whitespace().collect::<String>()).unwrap()
}

#[test]
fn rfc3414_key_localization_vectors() {
    // RFC 3414 A.3.1 (MD5) and A.3.2 (SHA-1): password "maplesyrup", engine 00…02.
    let engine = unhex("000000000000000000000002");
    let ku = Auth::Md5.password_to_key(b"maplesyrup");
    assert_eq!(hex::encode(&ku), "9faf3283884e92834ebc9847d8edd963");
    assert_eq!(hex::encode(Auth::Md5.localize(&ku, &engine)), "526f5eed9fcce26f8964c2930787d82b");
    let ku = Auth::Sha1.password_to_key(b"maplesyrup");
    assert_eq!(hex::encode(&ku), "9fb5cc0381497b3793528939ff788d5d79145211");
    assert_eq!(hex::encode(Auth::Sha1.localize(&ku, &engine)), "6695febc9288e36282235fc7151f128497b38f3f");
    // The cache gives back what it derived.
    let creds = Credentials { user: "u".into(), auth: Some((Auth::Md5, "maplesyrup".into())), privacy: None };
    for _ in 0..2 {
        let keys = creds.keys(&engine);
        assert_eq!(hex::encode(&keys.auth.unwrap().1), "526f5eed9fcce26f8964c2930787d82b");
    }
}

#[test]
fn privacy_keys_are_extended_the_blumenthal_way() {
    let engine = unhex("000000000000000000000002");
    let kul = derive_key(Auth::Md5, None, "maplesyrup", &engine);
    assert_eq!(derive_key(Auth::Md5, Some(Priv::Aes128), "maplesyrup", &engine), kul);
    assert_eq!(derive_key(Auth::Md5, Some(Priv::Des), "maplesyrup", &engine), kul);
    let long = derive_key(Auth::Md5, Some(Priv::Aes256), "maplesyrup", &engine);
    let h1 = Auth::Md5.hash(&kul);
    assert_eq!(long, [kul.as_slice(), &h1].concat());
    let mid = derive_key(Auth::Md5, Some(Priv::Aes192), "maplesyrup", &engine);
    assert_eq!(mid, long[..24]);
    // SHA-1 keys are 20 bytes; AES-256 takes 12 more from H(Kul).
    let kul = derive_key(Auth::Sha1, None, "maplesyrup", &engine);
    let long = derive_key(Auth::Sha1, Some(Priv::Aes256), "maplesyrup", &engine);
    assert_eq!(long, [kul.as_slice(), &Auth::Sha1.hash(&kul)[..12]].concat());
    // SHA-256 is long enough already.
    assert_eq!(derive_key(Auth::Sha256, Some(Priv::Aes256), "maplesyrup", &engine), derive_key(Auth::Sha256, None, "maplesyrup", &engine));
}

#[test]
fn aes_cfb_matches_nist_sp800_38a() {
    // F.3.13 CFB128-AES128.Encrypt (two blocks) and its inverse.
    let key = unhex("2b7e151628aed2a6abf7158809cf4f3c");
    let iv: [u8; 16] = unhex("000102030405060708090a0b0c0d0e0f").try_into().unwrap();
    let plain = unhex("6bc1bee22e409f96e93d7e117393172a ae2d8a571e03ac9c9eb76fac45af8e51");
    let mut data = plain.clone();
    aes_cfb(&key, &iv, &mut data, false);
    assert_eq!(hex::encode(&data), "3b3fd92eb72dad20333449f8e83cfb4ac8a64537a0b3a93fcde3cdad9f1ce58b");
    aes_cfb(&key, &iv, &mut data, true);
    assert_eq!(data, plain);
    // A short tail block is fine (CFB is a stream mode).
    let mut tail = plain[..20].to_vec();
    aes_cfb(&key, &iv, &mut tail, false);
    assert_eq!(hex::encode(&tail), "3b3fd92eb72dad20333449f8e83cfb4ac8a64537");
    // F.3.17 CFB128-AES256.Encrypt, first block.
    let key = unhex("603deb1015ca71be2b73aef0857d77811f352c073b6108d72d9810a30914dff4");
    let mut block = plain[..16].to_vec();
    aes_cfb(&key, &iv, &mut block, false);
    assert_eq!(hex::encode(&block), "dc7e84bfda79164b7ecd8486985d3860");
}

#[test]
fn des_cbc_matches_the_classic_vector() {
    // FIPS 81 / "Now is the time for all " with key 0123456789abcdef, IV 1234567890abcdef.
    let key = unhex("0123456789abcdef");
    let iv: [u8; 8] = unhex("1234567890abcdef").try_into().unwrap();
    let mut data = b"Now is the time for all ".to_vec();
    des_cbc(&key, &iv, &mut data, false);
    assert_eq!(hex::encode(&data), "e5c7cdde872bf27c43e934008c389c0f683788499a7c05f6");
    des_cbc(&key, &iv, &mut data, true);
    assert_eq!(&data, b"Now is the time for all ");
}

#[test]
fn discovery_is_an_empty_unauthenticated_get() {
    let packet = discovery(0x1234, 0x99);
    let req = parse(&packet, None).unwrap();
    assert_eq!((req.msg_id, req.pdu_tag, req.authenticated, req.encrypted), (0x1234, GET_REQUEST, false, false));
    assert!(req.engine.id.is_empty() && req.user.is_empty());
    assert_eq!(msg_id(&packet), Some(0x1234));
    assert_eq!(msg_id(b"\x30\x03\x02\x01\x01"), None);
    // reportable, no auth, no priv
    assert_eq!(
        hex::encode(&packet),
        "3039020103300e02021234020220000401040201030410300e0400020100020100040004000400301204000400a00c020200990201000201003000"
    );
}

#[test]
fn signed_and_encrypted_messages_round_trip() {
    let engine = Engine { id: unhex("80001f8880b2c7a95e1b0a2e6800000000"), boots: 7, time: 123_456 };
    for (auth, privacy) in [
        (Auth::Md5, None),
        (Auth::Sha1, Some(Priv::Des)),
        (Auth::Sha224, Some(Priv::Aes128)),
        (Auth::Sha256, Some(Priv::Aes192)),
        (Auth::Sha384, Some(Priv::Aes256)),
        (Auth::Sha512, Some(Priv::Des)),
    ] {
        let creds = Credentials { user: "vole".into(), auth: Some((auth, "authpass123".into())), privacy: privacy.map(|p| (p, "privpass123".into())) };
        let keys = creds.keys(&engine.id);
        let oid = super::super::encode_oid("1.3.6.1.2.1.1.1.0").unwrap();
        let req = Request { msg_id: 42, request_id: 4242, engine: &engine, user: "vole", keys: Some(&keys), context: "ctx", oid: Some(&oid), salt: 0xDEAD_BEEF_0000_0001 };
        let packet = build(&req);
        let back = parse(&packet, Some(&keys)).unwrap();
        assert_eq!((back.msg_id, back.authenticated, back.encrypted, back.pdu_tag), (42, true, privacy.is_some(), GET_REQUEST), "{auth:?} {privacy:?}");
        assert_eq!(back.engine, engine);
        assert_eq!(super::super::first_varbind_oid(&back.pdu).as_deref(), Some("1.3.6.1.2.1.1.1.0"));

        // One flipped bit anywhere breaks the signature.
        let mut tampered = packet.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 1;
        assert!(parse(&tampered, Some(&keys)).unwrap_err().contains("authentication code is wrong"));
        // Other passwords: wrong digest, or (privacy only) undecryptable.
        let other = Credentials { privacy: privacy.map(|p| (p, "privpass124".into())), ..creds.clone() };
        if privacy.is_some() {
            assert!(parse(&packet, Some(&other.keys(&engine.id))).is_err());
        }
        assert!(parse(&packet, None).unwrap_err().contains("signed an answer"));
    }
}

#[test]
fn report_oids_map_to_reasons() {
    assert_eq!(report_reason("1.3.6.1.6.3.15.1.1.1.0"), Some(ReportReason::UnsupportedSecurityLevel));
    assert_eq!(report_reason("1.3.6.1.6.3.15.1.1.2.0"), Some(ReportReason::NotInTimeWindow));
    assert_eq!(report_reason("1.3.6.1.6.3.15.1.1.3.0"), Some(ReportReason::UnknownUser));
    assert_eq!(report_reason("1.3.6.1.6.3.15.1.1.4.0"), Some(ReportReason::UnknownEngineId));
    assert_eq!(report_reason("1.3.6.1.6.3.15.1.1.5.0"), Some(ReportReason::WrongDigest));
    assert_eq!(report_reason("1.3.6.1.6.3.15.1.1.6.0"), Some(ReportReason::DecryptionError));
    assert_eq!(report_reason("1.3.6.1.6.3.11.2.1.3.0"), None);
    assert_eq!(ReportReason::UnknownUser.message("ops"), "The device doesn't know the SNMPv3 user \"ops\"");
    assert!(ReportReason::WrongDigest.message("ops").starts_with("Wrong authentication password"));
    assert!(ReportReason::DecryptionError.message("ops").starts_with("Wrong privacy password"));
}

#[test]
fn protocol_names_parse_leniently() {
    assert_eq!(Auth::parse("SHA-256"), Some(Auth::Sha256));
    assert_eq!(Auth::parse("sha"), Some(Auth::Sha1));
    assert_eq!(Auth::parse("sha3"), None);
    assert_eq!(Priv::parse("AES"), Some(Priv::Aes128));
    assert_eq!(Priv::parse("aes-256"), Some(Priv::Aes256));
    assert_eq!(Priv::parse("aes256c"), None);
    assert_eq!((Auth::Sha384.name(), Priv::Aes192.name()), ("SHA-384", "AES-192"));
}

/// Exchanges with net-snmp 5.9.4 (Alpine 3.22, `createUser <user> <auth>
/// "authpass123" <priv> "privpass123"`): its discovery report, our GET for
/// sysDescr.0 (msgID 0x0303, request 0x0404, salt 0x1122334455667788) and
/// its signed, encrypted response.
const GOLDEN: &[(&str, Auth, Priv, &str, &str, &str)] = &[
    (
        "md5_aes256",
        Auth::Md5,
        Priv::Aes256,
        "306e020103300f02020101020300ffe304010002010304223020041180001f88809fd2884ff098d76a00000000020101020201c60400040004003034041180001f88809fd2884ff098d76a000000000400a81d020202020201000201003011300f060a2b060106030f01010400410159",
        "30818a020103300e02020303020220000401070201030440303e041180001f88809fd2884ff098d76a00000000020101020201c6040a6d64355f616573323536040c74a2db2a161da494c5e9b7b80408112233445566778804335e0873e2747fbbb5e89d6091fb9dc2fc72a3d81fab94d23787431b1c2e096e519c4cfefa2a1657cf99d386ed3ae0f90936f332",
        "3081a6020103300f02020303020300ffe30401030201030440303e041180001f88809fd2884ff098d76a00000000020101020201c6040a6d64355f616573323536040cf253974f3d6f4f1a2a14db2704081449743b01ee97e4044e12115bee47812bb17f7319ea5e01a87d8fcd820748b1ecca60b330da37977c06a3a2fd2b4b4ed7b202c9ffb423322e8d087b6a0b180dcce7bd00007a435d8c00b53901711452e7dcd8bda126effb",
    ),
    (
        "sha_des",
        Auth::Sha1,
        Priv::Des,
        "306e020103300f02020101020300ffe304010002010304223020041180001f88809fd2884ff098d76a00000000020101020201c60400040004003034041180001f88809fd2884ff098d76a000000000400a81d020202020201000201003011300f060a2b060106030f0101040041015a",
        "30818c020103300e0202030302022000040107020103043d303b041180001f88809fd2884ff098d76a00000000020101020201c604077368615f646573040cb1886f359ce9aff022c9ea3c0408000000015566778804385278dcf1d0e5f975d6442de90dc2eb6b0fd57b34bb36d2b4604b803192d7f89f6a32df8c78b6d97464577b5c4f7ed4614b405d2be59eb745",
        "3081a5020103300f02020303020300ffe3040103020103043d303b041180001f88809fd2884ff098d76a00000000020101020201c604077368615f646573040c7c38efd40c829e35cad5f93a040800000001ceac778a0450f1281c33cc6a12bf2f2dbd974bfe0a2569d9e3e9e2595fab2884997295cf5c856bf0129a7aa8d4ad47d9084dab97e070acd772437cfd0b2457ef48faf3ac4d805fecde23b7cf46b541564ad9d5c8cd0b",
    ),
    (
        "sha512_aes",
        Auth::Sha512,
        Priv::Aes128,
        "306e020103300f02020101020300ffe304010002010304223020041180001f88809fd2884ff098d76a00000000020101020201c60400040004003034041180001f88809fd2884ff098d76a000000000400a81d020202020201000201003011300f060a2b060106030f0101040041015b",
        "3081ae020103300e020203030202200004010702010304643062041180001f88809fd2884ff098d76a00000000020101020201c6040a7368613531325f616573043052d9d9f073a04cb5c24e8dc74d8d09a1e0565f97554aa819a00b88cda9b2735bb83d1154c31eadce9b50e5aacc7f749d040811223344556677880433b76d4e8ed692a8fa62c3dff2053542b38e0b3d06c237a0056ccd7c70ac3e6544de197744601d1c15e17071c797aa0cdfda711f",
        "3081ca020103300f02020303020300ffe304010302010304643062041180001f88809fd2884ff098d76a00000000020101020201c6040a7368613531325f61657304302f7d0a0d5de9feb736f704d61c413f30941f02fcdc70a16af288ff9c90de933a9470e6ca9ce03cfdd5d1a2dfbdd121cc04081449743b01ee97e5044e37d75aa1ba0231fa7a20e5154df412b58a546ad23873469cdb107053c12b8daa1838030da388ce93b2ecbc83ccbb56db905bcc9d0d0741cdc66da0df2fec75a7c7687f3b40d2b08b450226b957cc",
    ),
];

#[test]
fn golden_exchanges_with_net_snmp() {
    let oid = super::super::encode_oid("1.3.6.1.2.1.1.1.0").unwrap();
    for (user, auth, privacy, report, get, response) in GOLDEN {
        // The discovery report names the engine (and counts unknownEngineIDs).
        let report = parse(&unhex(report), None).unwrap();
        assert_eq!((report.msg_id, report.pdu_tag, report.authenticated), (0x0101, REPORT, false));
        assert_eq!(report.engine, Engine { id: unhex("80001f88809fd2884ff098d76a00000000"), boots: 1, time: 454 });
        let reason = super::super::first_varbind_oid(&report.pdu).as_deref().and_then(report_reason);
        assert_eq!(reason, Some(ReportReason::UnknownEngineId));

        // Our GET is byte for byte what net-snmp accepted.
        let creds = Credentials { user: (*user).into(), auth: Some((*auth, "authpass123".into())), privacy: Some((*privacy, "privpass123".into())) };
        let keys = creds.keys(&report.engine.id);
        let req = Request { msg_id: 0x0303, request_id: 0x0404, engine: &report.engine, user, keys: Some(&keys), context: "", oid: Some(&oid), salt: 0x1122_3344_5566_7788 };
        assert_eq!(hex::encode(build(&req)), *get, "{user}");

        // Its answer verifies and decrypts with our keys.
        let answer = parse(&unhex(response), Some(&keys)).unwrap();
        assert_eq!((answer.msg_id, answer.pdu_tag, answer.authenticated, answer.encrypted), (0x0303, super::super::GET_RESPONSE, true, true), "{user}");
        let value = super::super::parse_pdu(&answer.pdu, 0x0404).unwrap();
        assert_eq!(value, super::super::Value::Text("Vigilant Vole SNMPv3 matrix".into()), "{user}");
        // With another privacy password it doesn't.
        let wrong = Credentials { privacy: Some((*privacy, "privpass124".into())), ..creds.clone() };
        assert!(parse(&unhex(response), Some(&wrong.keys(&report.engine.id))).is_err(), "{user}");
    }
}
