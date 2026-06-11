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
//! The certificate is located the way `supso_project` describes: the
//! `SUPSO_LICENSE_PATH` environment variable, then
//! `$HOME/.supso/license_certificates/`, then `./.supso/license_certificates/`.

use crate::error::{ErrorDomain, ErrorLevel, Result, XmlError};
use chrono::{DateTime, TimeZone, Utc};
use std::sync::OnceLock;
use supso_project::{Enforcement, Status, Supso};

/// The Supso project slug this build is licensed against. The certificate's
/// signed product binding must match this exactly.
const PROJECT_SLUG: &str = "sup-xml";

/// Cached verdict: `Ok(())` once a valid license has been seen, or the
/// human-readable reason it failed. `String` (not the borrowed
/// `supso_project::Error`) so it can live in a `'static` cache.
static VERDICT: OnceLock<std::result::Result<(), String>> = OnceLock::new();

/// `DateTime<Utc>` from `SystemTime` without pulling in `iana-time-zone`
/// (the chrono `clock` feature does). A library must not drag in OS-level
/// timezone resolution it doesn't need.
fn now() -> DateTime<Utc> {
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    DateTime::<Utc>::from_timestamp(dur.as_secs() as i64, dur.subsec_nanos())
        .unwrap_or_else(|| Utc.timestamp_opt(0, 0).unwrap())
}

fn verdict() -> &'static std::result::Result<(), String> {
    VERDICT.get_or_init(|| {
        let at = now();
        // `Silent`: this gate routes its grace notice through `log` so it
        // lands in the host's tracker, rather than letting the library
        // write to stderr unbidden.
        let status = Supso::project(PROJECT_SLUG)
            .enforcement(Enforcement::Silent)
            .check_at(at);
        match status {
            Status::Valid(_) => Ok(()),
            // A certificate honoured under the post-expiry grace period
            // still licenses the process, but the lapse is logged at
            // `error` so it lands in the host's tracker. The day counts
            // in the message change daily, so a fresh alert fires each
            // day the lapse persists rather than one that is silenced
            // after first sight.
            Status::Grace { .. } => {
                if let Some(notice) = status.grace_message(at) {
                    log::error!("{notice}");
                }
                Ok(())
            }
            Status::Unlicensed(e) => Err(e.to_string()),
        }
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
