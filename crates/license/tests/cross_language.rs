//! Cross-tool integration test: a token minted by the issuer-side
//! license signer (the production signing service) is verified by the
//! `sup-xml-license` verifier library.
//!
//! The fixture in `tests/fixtures/issuer_signed_token.txt` was produced
//! by signing the payload below with a throwaway test keypair (whose
//! public halves are pinned below): an Ed25519 signature and an
//! ML-DSA-44 (FIPS 204) signature over the same canonical payload bytes.
//!
//! Both Ed25519 and ML-DSA-44 (with `Signer::sign`) are deterministic
//! over the seed-derived signing key, so re-running with the same keys
//! and payload produces a byte-identical fixture — if a future `ml-dsa`
//! release changes that, this test will catch it.  The payload carries
//! `metadata.project.name = "sup-xml"`, matching the production binding.

use ed25519_dalek::VerifyingKey as Ed25519VerifyingKey;
use ml_dsa::{
    EncodedVerifyingKey as MlDsaEncodedVerifyingKey, MlDsa44,
    VerifyingKey as MlDsaVerifyingKey,
};
use sup_xml_license::verify_with_keys;

const TEST_ED25519_PUBLIC_KEY_HEX: &str =
    "64d463525918e00058869955e4bf711bc4ab745b37e72c57956e542ddb72fa1c";

const TEST_MLDSA44_PUBLIC_KEY_HEX: &str = "7de39efb89a4a85fac9999485d50632663a2ae6016e15531aecf765d31a2359ca2473d600fd6e73897ad71ff6c0ca256b87ea7ee051532b954dc792e7e1cd824a75442f26b6bc3accaafa685e7d7f6a96d37a02a6a4b51cef72fa9c5a7f1cd3ae41401bb8fd0fa4dc2976976f202f8149c6da756d78bdf61c1de89f9d4b1a92ea33a144308e624ac9d258643584012d294d37f030feffcb7015eb18d29b2063878f5e93477d789744ce5cbcf438e4ed1462d1370c97555a96fd1b01ae02090ea47854776fb1143d3ee7a6cc130c7f942a05040fe7ed41cb7df269d596331940e26be5a9c599c6912651cf8f83050faeefb0d070c17b04c22e473035d5c8b4ab9ffbf0e3c8ac3ba69125d0bb63d7834ab4c3611fa3935b75810be9cbc7196408995dc7ced81edaa680329f593da6e2657de929911c67872f3a051ce236447af64a58471bb40843c31e6b2905a98bd7f3770012fe924ed2cfb7b0f1cafb1e4335eae74c6bdea302e723546a0fcb4ebe4b6dd755f79eef70c40088071fb54915459eea3a899c795da3d187b6a6b5e0ddb0aee1a98830e51a772aa189a3d4cca4f505c02a5353f2616799ea271e98af7adbfadec3bcd57595aa2b27e5f6033a0b546c09c2d4a314e09ec1a0487d987a9684d32beec309baeea39245f2d7da69568feb79704a9a28b987bed5e15224f5f21fa9236f5a63a1ba43551566a887bda6f3b27ae10ed1459d75d45ff7f14522da1dc99de3934724762d63e3eff4612a28a7a22f2c723916e5bc4913989b4880cc42b8232c272c612ae24b5ce88c7ebeab2efd2e434be53b8b7046293a19980bd0eac5f77ac3ab135c6c9384b62e77536dcd77d885050fdf54d66f7b25ef02f0c158c0f31d58995be36ccff636d484a5c45b9a8337561a4a4a0099b823f60db83b63d17501080356d413a6e878772f99453dcca8249235f2e4a5ec56ad39f3a6a3a5f8552c4fd4c95d3800333d1c83321dc4d98d7cb97d809e1caa8a1aee4844bcd618e8f1f51336efe55a4d3987756bbae558599d58698f939e7509ddb3a864bd95bed91a92f49067ed1ab9a096ca1f0f50d98b46e7936449b1f2fa2210d99d2aba01510cdb5e0ab53220db409d241bddb1c759c2a5972efc716aa6306d18879167a1682a18b8b709d12d02b58a121b462a4d789a52d3f6aff5678b4b247d478384a5643cc87097f3fff80f2469f92ccf49795bd6e3fc58629274ea3257cea2a7bbb3fd4275512a0c0bb2cc86146a178adab982da726dbdcc16a5879fd9b6685cabd91fccdcec53b1e6cf2d509e02c96fe33beb615047d61319e5f7fb659eb40af4a6c67a6c43fa6bc7f7aaf792e619790676576fe0d70df3c5a49c371c253a76aeefbceb7a8bc2c34d03e1893a198dd67d587b4d1af247f4878b595e5c0234f856cd7771386f3018f38dfa1892508a746f7dc44f2333a1752b7a99ade8623d35a2afa1c7c16337fc5573a15615fa644dc9c98801e640a256c25feae46c3ed144a6cbb9432a80684bfdfa3f6cb4ef0b0304ffd6d93235c516438e459bb9ca9793e08c5110a5bf311019f5591a7d31195b7cb44ac7a9e60ac19929a364ab88773ca7ecfc2d8dfc155b771524f1a08e081355dae42cda57218e024bf214674f9e2380e0af9f04fece2e3f107b2cbeffb40941007cbba3d58c881f094d45e400df11c05bd34c7a547b894f4cea906bf07d82de820101711f29e35baea9f1ff817d6ae0dd76782d63bdf4a4bb54b5c922507b9c4681e8af3c29eda7dd1e20295f7c286b08f2220d34f1fab5e36c1cb113c2e7e0d2774a3d05adf6aeac1dcebde11efa716c87cce72cd265f94701fee9ef690e6ee";

#[test]
fn issuer_minted_token_verifies_and_parses() {
    let token = include_str!("fixtures/issuer_signed_token.txt").trim();

    let ed_key = decode_ed25519_key(TEST_ED25519_PUBLIC_KEY_HEX);
    let pq_key = decode_mldsa44_key(TEST_MLDSA44_PUBLIC_KEY_HEX);

    let now = chrono::DateTime::parse_from_rfc3339("2026-05-21T00:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);

    let license = verify_with_keys(token, &[ed_key], &[pq_key], now)
        .expect("Ruby-side fixture should verify against the matching public keys");

    assert_eq!(license.organization.id, "org_cross_lang");
    assert_eq!(license.organization.name, "Cross-Language Co");
    assert_eq!(license.project.name, "sup-xml");
    assert_eq!(license.order.id, "ord_cross_lang");
    assert_eq!(
        license.order.expires_at,
        chrono::DateTime::parse_from_rfc3339("2030-06-15T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc)
    );
    assert_eq!(
        license.metadata.get("support_tier").unwrap(),
        &serde_json::json!("pro")
    );
}

fn decode_ed25519_key(hex: &str) -> Ed25519VerifyingKey {
    let bytes = hex_decode_exact::<32>(hex);
    Ed25519VerifyingKey::from_bytes(&bytes).expect("Ed25519 test key should be valid")
}

fn decode_mldsa44_key(hex: &str) -> MlDsaVerifyingKey<MlDsa44> {
    let bytes = hex_decode_vec(hex, 1312);
    let encoded = MlDsaEncodedVerifyingKey::<MlDsa44>::try_from(bytes.as_slice())
        .expect("ML-DSA-44 test key should be the correct length");
    MlDsaVerifyingKey::<MlDsa44>::decode(&encoded)
}

fn hex_decode_exact<const N: usize>(hex: &str) -> [u8; N] {
    let mut out = [0u8; N];
    assert_eq!(hex.len(), N * 2, "hex string is the wrong length for {N}-byte buffer");
    for (i, pair) in hex.as_bytes().chunks(2).enumerate() {
        out[i] = (nibble(pair[0]) << 4) | nibble(pair[1]);
    }
    out
}

fn hex_decode_vec(hex: &str, n: usize) -> Vec<u8> {
    assert_eq!(hex.len(), n * 2, "hex string is the wrong length for {n}-byte buffer");
    let mut out = vec![0u8; n];
    for (i, pair) in hex.as_bytes().chunks(2).enumerate() {
        out[i] = (nibble(pair[0]) << 4) | nibble(pair[1]);
    }
    out
}

fn nibble(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => panic!("invalid hex digit: {c}"),
    }
}
