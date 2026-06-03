//! Process-wide license gate.
//!
//! sup-xml requires a valid license certificate to parse documents.
//! The verification — locating a certificate, two signature checks, and
//! a JSON parse — runs **once per process**, lazily on the first parse,
//! and the verdict is cached for the process lifetime. Every subsequent
//! parse pays only an atomic load, so the gate adds no measurable
//! per-call overhead.
//!
//! Call [`verify_license`] at startup to verify eagerly and fail fast;
//! otherwise the first document parse triggers the check.
//!
//! The certificate is located the same way [`sup_xml_license::License::validate_certificate`]
//! describes: the `SUPSO_LICENSE` environment variable, then
//! `$HOME/.supso/license_certificates/`, then `./.supso/license_certificates/`.

use crate::error::{ErrorDomain, ErrorLevel, Result, XmlError};
use std::sync::OnceLock;

/// Cached verdict: `Ok(())` once a valid license has been seen, or the
/// human-readable reason it failed. `String` (not the borrowed
/// `CertificateError`) so it can live in a `'static` cache.
static VERDICT: OnceLock<std::result::Result<(), String>> = OnceLock::new();

fn verdict() -> &'static std::result::Result<(), String> {
    VERDICT.get_or_init(|| match sup_xml_license::License::validate_certificate() {
        Ok(cert) => {
            // A certificate honoured under the post-expiry grace period
            // still licenses the process, but the lapse is logged at
            // `error` so it lands in the host's tracker. The day counts
            // in the message change daily, so a fresh alert fires each
            // day the lapse persists rather than one that is silenced
            // after first sight.
            if let Some(notice) = cert.grace_notice() {
                log::error!("{notice}");
            }
            Ok(())
        }
        Err(e) => Err(e.to_string()),
    })
}

/// Verify the license now, caching the result, and return an error if no
/// valid, non-expired certificate is present.
///
/// Parsing performs this check lazily on first use regardless; calling
/// this at program startup surfaces a missing or expired license
/// immediately rather than at the first parse.
pub fn verify_license() -> Result<()> {
    ensure_licensed()
}

/// The gate every document-parse entry point calls. Cheap after the
/// first invocation (an atomic load plus a match).
pub(crate) fn ensure_licensed() -> Result<()> {
    match verdict() {
        Ok(()) => Ok(()),
        Err(reason) => Err(XmlError::new(
            ErrorDomain::None,
            ErrorLevel::Fatal,
            format!(
                "sup-xml: a valid license is required to parse documents — {reason}. \
                 Get a certificate (free for individuals and 30-day evaluation) at \
                 https://supso.org/projects/sup-xml"
            ),
        )),
    }
}
