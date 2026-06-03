//! One-off keypair generator for the license issuer.
//!
//! Produces fresh Ed25519 *and* ML-DSA-44 keypairs.  Run once, then:
//!
//!   * paste the two PUBLIC key hex strings into the
//!     `TRUSTED_ED25519_KEYS_HEX` / `TRUSTED_MLDSA44_KEYS_HEX`
//!     slices in `crates/license/src/lib.rs`,
//!   * store the two PRIVATE key hex strings in the issuing
//!     service's secret store (e.g. Rails encrypted credentials —
//!     `LICENSE_ED25519_PRIVATE_KEY_HEX` and
//!     `LICENSE_MLDSA44_SEED_HEX`).
//!
//! Run with:
//!
//! ```text
//! cargo run -p sup-xml-license --example generate_keypair
//! ```

use ed25519_dalek::SigningKey as Ed25519SigningKey;
use ml_dsa::{Generate, Keypair, MlDsa44, SigningKey as MlDsaSigningKey};
use rand::rngs::OsRng;

fn main() {
    // --- Ed25519 ---
    let ed_sk = Ed25519SigningKey::generate(&mut OsRng);
    let ed_pk = ed_sk.verifying_key();

    // --- ML-DSA-44 ---
    // The signing key is reconstructible from its 32-byte seed, so
    // we store the seed (not the expanded ~2.5KB secret).  Same
    // model as Ed25519 for symmetry on the issuer side.
    let pq_sk = <MlDsaSigningKey<MlDsa44> as Generate>::generate();
    let pq_seed = pq_sk.as_seed();
    let pq_pk = pq_sk.verifying_key();
    let pq_pk_encoded = pq_pk.encode();

    println!("=== PRIVATE (keep secret — issuer side only) ===");
    println!();
    println!("Ed25519 private key (32 bytes hex, 64 chars):");
    println!("  {}", hex(&ed_sk.to_bytes()));
    println!();
    println!("ML-DSA-44 seed (32 bytes hex, 64 chars):");
    println!("  {}", hex(pq_seed.as_ref()));
    println!();
    println!("=== PUBLIC (paste into crates/license/src/lib.rs) ===");
    println!();
    println!("Ed25519 public key (32 bytes hex, 64 chars):");
    println!("  {}", hex(ed_pk.as_bytes()));
    println!();
    println!("ML-DSA-44 public key (1312 bytes hex, 2624 chars):");
    println!("  {}", hex(pq_pk_encoded.as_slice()));
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}
