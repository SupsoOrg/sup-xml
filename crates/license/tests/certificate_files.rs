//! Verifies on-disk certificate files (raw tokens) against the baked-in
//! production trusted keys.
//!
//! * `valid_certificate.cert` — a genuine token (org "Acme Corp",
//!   `metadata.project.name = "sup-xml"`, expiring 2030-01-01).
//! * `tampered_certificate.cert` — the same token with one byte of the
//!   Ed25519 signature segment flipped; verification must reject it.
//!
//! A fixed clock keeps these deterministic past the certificate's
//! expiry (`validate_certificate_at` rather than the system-clock
//! `validate_certificate`).

use chrono::{DateTime, Utc};
use std::path::{Path, PathBuf};
use sup_xml_license::{CertificateError, validate_certificate_at};

fn ts(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

#[test]
fn valid_token_certificate_verifies_against_baked_in_keys() {
    let cert = validate_certificate_at(
        Some(&fixture("valid_certificate.cert")),
        ts("2026-06-01T00:00:00Z"),
    )
    .expect("valid certificate should verify");

    assert_eq!(cert.license.organization.name, "Acme Corp");
    assert_eq!(cert.license.project.name, "sup-xml");
    assert_eq!(cert.license.order.expires_at, ts("2030-01-01T23:59:59Z"));
}

#[test]
fn test_signed_certificate_is_rejected_by_production_keys() {
    // `issuer_signed_token.txt` is signed by a throwaway TEST keypair
    // that is not in the baked-in production trust list.  Against the
    // real verifier it must fail at the signature check — proof that a
    // test certificate cannot be used in production.
    let err = validate_certificate_at(
        Some(&fixture("issuer_signed_token.txt")),
        ts("2026-06-01T00:00:00Z"),
    )
    .expect_err("a test-signed certificate must not verify under production keys");

    match err {
        CertificateError::NoneValid { tried } => {
            assert!(
                tried[0].1.contains("Ed25519"),
                "expected a signature rejection, got: {}",
                tried[0].1
            );
        }
        other => panic!("expected NoneValid, got {other:?}"),
    }
}

#[test]
fn corrupted_token_certificate_is_rejected() {
    let err = validate_certificate_at(
        Some(&fixture("tampered_certificate.cert")),
        ts("2026-06-01T00:00:00Z"),
    )
    .expect_err("corrupted certificate must be rejected");

    match err {
        CertificateError::NoneValid { tried } => {
            assert_eq!(tried.len(), 1);
            // A flipped signature byte fails the Ed25519 check.
            assert!(
                tried[0].1.contains("Ed25519"),
                "unexpected rejection reason: {}",
                tried[0].1
            );
        }
        other => panic!("expected NoneValid, got {other:?}"),
    }
}
