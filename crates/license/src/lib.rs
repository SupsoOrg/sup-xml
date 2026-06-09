//! Offline hybrid (Ed25519 + ML-DSA-44) license-token verification.
//!
//! ## Token format
//!
//! Three URL-safe-base64 segments separated by `.`:
//!
//! ```text
//! base64url(payload_json) . base64url(ed25519_sig) . base64url(mldsa44_sig)
//! ```
//!
//! Both signatures cover the *encoded* payload bytes (the first
//! segment as ASCII), so the verifier never has to canonicalise
//! JSON to recompute the signed message.  The hybrid design means
//! a forger has to break **both** Ed25519 *and* ML-DSA-44 to mint a
//! valid token — Ed25519 protects against any unforeseen flaw in
//! the lattice scheme, ML-DSA-44 protects against a future quantum
//! adversary that can break ECC.
//!
//! ## Payload
//!
//! ```json
//! {
//!   "v": 1,
//!   "organization": { "id": "...", "name": "..." },
//!   "order":        { "id": "...", "expires_at": "2027-05-21T14:32:00Z" },
//!   "metadata":     { "project": { "name": "sup-xml" }, ... }
//! }
//! ```
//!
//! ## Acceptance criteria
//!
//! Verification accepts the token iff all of the following hold:
//!
//! 1. exactly three `.`-separated, non-empty segments,
//! 2. each segment decodes as URL-safe base64 (no padding),
//! 3. the Ed25519 signature verifies under at least one key in
//!    [`TRUSTED_ED25519_KEYS_HEX`] (strict / IETF-compliant
//!    verification),
//! 4. the ML-DSA-44 signature verifies under at least one key in
//!    [`TRUSTED_MLDSA44_KEYS_HEX`],
//! 5. the payload parses as JSON with `v == 1`,
//! 6. `metadata.project.name` equals [`EXPECTED_PROJECT_NAME`] (`"sup-xml"`),
//! 7. `order.expires_at` parses — as a full RFC 3339 timestamp, or a
//!    bare `YYYY-MM-DD` date taken as end-of-day (`23:59:59Z`) — and is
//!    strictly in the future at the wall-clock instant supplied to
//!    [`License::verify_at`].
//!
//! Both signature checks must succeed — "hybrid" here is `AND`, not
//! `OR`.  Either scheme being broken still leaves the other as a
//! barrier.
//!
//! ## License certificates on disk
//!
//! A *license certificate* file is a single token — its raw text, with
//! surrounding whitespace ignored.  The token is the whole certificate;
//! its signed payload is the only source of truth.
//! [`License::validate_certificate`] locates and verifies one without
//! the caller having to know where it lives: it searches, in order,
//!
//! 1. an explicit path (the `SUPSO_LICENSE_PATH` environment variable, or
//!    the `explicit` argument to [`validate_certificate_at`]),
//! 2. `$HOME/.supso/license_certificates/`,
//! 3. `./.supso/license_certificates/` (relative to the current
//!    working directory).
//!
//! Within a directory every non-hidden regular file is a candidate,
//! tried in filename order; the first one that verifies wins.  An
//! explicit path is authoritative — if it names a file or directory
//! with no valid certificate, the search does *not* fall through to
//! the default locations.
//!
//! ## Grace period
//!
//! A live (non-expired) certificate always wins outright.  When the
//! search finds none, but at least one candidate was valid in every
//! respect *except* that it had expired, the most-recently-expired such
//! certificate is honoured on a temporary basis if it lapsed within
//! [`GRACE_PERIOD_DAYS`] days of the supplied clock.  In that case the
//! returned [`LicenseCertificate`] carries [`LicenseStatus::Grace`]
//! rather than [`LicenseStatus::Valid`], so the caller can keep running
//! while surfacing the lapse (e.g. through its logging) and prompting a
//! renewal.  Once the grace window itself elapses, the certificate is
//! rejected like any other expired one.  Certificates that fail for any
//! other reason — bad signature, wrong project, unparseable payload —
//! never qualify for grace.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, NaiveDate, Utc};
use ed25519_dalek::{Signature as Ed25519Signature, VerifyingKey as Ed25519VerifyingKey};
use ml_dsa::{
    EncodedSignature as MlDsaEncodedSignature, EncodedVerifyingKey as MlDsaEncodedVerifyingKey,
    MlDsa44, Signature as MlDsaSignature, VerifyingKey as MlDsaVerifyingKey,
    signature::Verifier as _,
};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Trusted Ed25519 public keys, one per slice entry, as 64-char
/// lowercase hex (32 raw bytes each).
///
/// Listed as a slice so a new key can be rolled out alongside the
/// old one: ship a release that lists both, wait for outstanding
/// licenses to be re-issued under the new key, then drop the old
/// entry in a later release.
///
/// Default is empty — until populated, every token is rejected with
/// [`LicenseError::BadSignature`].  Paste the hex output of
/// `cargo run -p sup-xml-license --example generate_keypair` here.
pub const TRUSTED_ED25519_KEYS_HEX: &[&str] = &[
    "b1ef6d78434daaf5dae5e3df7996a3edc155198c71c92b91f539ce2fe649e9d4",
];

/// Trusted ML-DSA-44 public keys, one per slice entry, as
/// 2624-char lowercase hex (1312 raw bytes each).
///
/// Rotates independently of [`TRUSTED_ED25519_KEYS_HEX`]; a token
/// is accepted iff each scheme's signature verifies under at least
/// one key in its respective list.
pub const TRUSTED_MLDSA44_KEYS_HEX: &[&str] = &[
    "1be0e29d77a9998bb7661228a79e1d8118aa9ad88535720563d1aef9b8abc0d3d037cd984f3f652fc5512c935593dcd9f470540e125f0e9bcdc17c0bad721386bb3e3cd28cf895cbd0168cfcaee5131e581e2916abc7a1c25022f016c81781d16cd8b4cd8c36a7ced2a1753e4c01f69a05e9acb6f1926867a028f0a5ae6c1f22873c3df3d562f2ec844d422c8c68b2deba9257a13013bd009a227d7c40a3aa81129b6ed1f950a0cf75f3cfb9fc0e405d7fc929ecc9bf59dc566b075b3b80ad914fd150749b9022638e99a7d64b68ff82e295609dc2f6bd291463c417a1e69e7e145164fb3787c5bc90fe1dc4268bbd4d4e44c13d50120cf161466fbb53ea2a5f724ace3afef42eeeb0d631ad2b815b5442caddb509ed9d1cc8ab79ba4042fa168245d1446294677503eed33410a979a0646da3393e7c2a6e450c63c55ecc5bd92771ef7b278b3cef2d80fa306733fb2ecee4b2f4bb07c3b14ad29da4607b97e04cfefb76258000185c6d795ff91e9af4a0acf54a25ddbf14814540461474d67d6b9503d7e07e97907ef483593f97b9d039c4e548b25917d722c1c3e5ae7f81a1c232faf5a6978641230408cc01d70724f64e150836388dcadf60ae5be134ab5413228e5ab13402aa4331d144b586e1cb05d7a4034a9a5f71513d6029615197212b4a68684d05fedfe92bb05f24eccb6a9be9c1fa1003ba106718db9d39d4a08c5b6ae014d0086a957ed033e74af13d9a94d549524316d6aa544487eab716878a4a07ca3285724af050fccc8c3f8c3c82e2cd271133ca9562e71238e23baca5ec297ff50ebee8b2461607054d8f18d60b012b6eee2240adfa38b05f10ca679c271922907819974b5481d0aa7cab37c215123a6b6d7067856dfc475a8c2e3e54b658deb0709c8cb77b0a8508519d44b784f7584966d978d65b78e11e059c1e4b5441fd1625af92b2d647c5f0805343243b2225882b3c9beda4c1102aff1c084dac105da2d37813881fad33a860cc655bb45fac59d73ac2d92fb4fde59cbf10bdd07541e605d008467ea4c20e0c2796683499b19adb394dea4c8260c7438fb9efe2db2d65b9f7f4f2a8068a1010f70b7b2ae83c92dac6af023fe7f41eccba36d702b1bd73c88147f3ebc4f9e5416573321567b224a255e3edaf6afce2bce7b1d33955bb3288bf1da61709488c84e9321385e6ac71d120b752698f60eed6f1e75b12c04b30f998450cedecf91c6e611e7f23d5ca1252f89b699196f726931078f7e93d5a81b32613f7fe42563b907dae17822757bd66a0b1d3aecce3b5a01d317f085322c2df01cea971b0e3b58ec7cd80d681dafbc6ed2eeadcef36e8168336aa8954fd1cf6426702679d8e3f57a8e0b9cda12dd0ccf0aeb5a6f84ac5e2e0d4e43093063f1b8e1103359bc92d5a1891d8e34ec44a97469941d5b2665c5cfb910019840a8b8efd7e59e3bb727dbd09d3d68cf9b4f4c42687b762da645233bfb55b5fa523eca0241e4a6649e12aa41b5d52a2a6cdd043e1bcd500c6c99ba7fe33bd39a5f380be88c14ebaf01c24034b4b57b04a290336240ee06c0feaa495aee10f15006cff5750061bc93091660dd945efda00ddbd50cff8f69b85c18493ae21f70e1d9b29c6b90457177f9313764d505738b5474550b1e6969fac1aac7a3f25b6bef5d0be43eab4b464a8a86be08417935a8f5d097cc237a0f57e4330c18ef54d376e42a2b8d17a1c3e18a6a84c123cff67ea4f2e86284a34862f81c0c65cf79cb5c972de88a98e5c0ce89530739729610b39ac1f03f5158e26b81e364219ead057d43b222bbd93c39d3507c127ca78a7c6a85511ba92195b125f8ff70cb2d5c7b2",
];

/// Schema version this build of the library understands.  Tokens
/// with any other `v` are rejected unconditionally.
const SCHEMA_VERSION: u32 = 1;

/// The product this build is licensed for.  A token's
/// `project.name` must equal this, or it is rejected — a license
/// issued for another product (even by the same issuer) is not valid
/// here.  Pinned in code alongside the keys and schema version.
const EXPECTED_PROJECT_NAME: &str = "sup-xml";

/// A successfully-verified license.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct License {
    pub organization: Organization,
    pub project: Project,
    pub order: Order,
    /// Issuer-attached metadata.  The library reads `metadata.project`
    /// (surfaced as [`project`](Self::project)) for the product binding;
    /// the rest is free-form for human / billing consumption.
    pub metadata: BTreeMap<String, serde_json::Value>,
}

/// A verified license together with the certificate file it came
/// from.  Returned by [`License::validate_certificate`] /
/// [`validate_certificate_at`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LicenseCertificate {
    pub license: License,
    /// The certificate file that produced [`license`](Self::license).
    pub path: PathBuf,
    /// Whether this certificate is currently valid or is being honoured
    /// under the post-expiry grace period (see [`LicenseStatus`]).
    pub status: LicenseStatus,
}

/// Whether a located certificate is live or running on borrowed time.
///
/// A successful lookup ([`validate_certificate_at`] and friends) always
/// returns a certificate; this distinguishes a normal, non-expired one
/// from a lapsed one still inside its grace window.  See the module
/// docs' "Grace period" section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LicenseStatus {
    /// The certificate had not expired at the supplied clock instant.
    Valid,
    /// No live certificate was found, but this one — the most recently
    /// expired otherwise-valid candidate — lapsed within
    /// [`GRACE_PERIOD_DAYS`] days and is honoured until `grace_until`.
    Grace {
        /// When the certificate's `order.expires_at` fell due.
        expired_at: DateTime<Utc>,
        /// The instant the grace period ends (`expired_at` plus
        /// [`GRACE_PERIOD_DAYS`]); past this, the certificate is rejected.
        grace_until: DateTime<Utc>,
    },
}

/// How long a lapsed certificate is still honoured past its
/// `order.expires_at` before parsing is refused outright.  Within this
/// window a lookup succeeds with [`LicenseStatus::Grace`] so the host
/// can keep running while flagging the lapse; beyond it the certificate
/// is treated as plainly expired.
pub const GRACE_PERIOD_DAYS: i64 = 21;

/// The grace window as a [`chrono::Duration`].
fn grace_period() -> chrono::Duration {
    chrono::Duration::days(GRACE_PERIOD_DAYS)
}

impl LicenseStatus {
    /// A human-readable grace notice for [`Grace`](Self::Grace), or
    /// `None` when the status is [`Valid`](Self::Valid).
    ///
    /// The message quotes how many days ago the license lapsed and how
    /// many remain before parsing is refused, both measured against
    /// `now` (pass the same clock used to locate the certificate).
    /// Because the day counts roll over, the text changes each calendar
    /// day — a host that forwards it to an error tracker sees a fresh
    /// message daily rather than one alert that is silenced after first
    /// sight, keeping the lapse visible until the certificate is renewed.
    pub fn grace_message(&self, now: DateTime<Utc>) -> Option<String> {
        let Self::Grace { expired_at, grace_until } = self else {
            return None;
        };
        let days_ago = (now - *expired_at).num_days().max(0);
        let days_left = (*grace_until - now).num_days().max(0);
        Some(format!(
            "Your sup-xml license expired {days_ago} day{} ago — you're in a grace period, \
             but parsing will start failing in {days_left} day{}. Renew the license \
             certificate to avoid an interruption.",
            plural(days_ago),
            plural(days_left),
        ))
    }
}

/// Pluralizing suffix for a day count: `""` for exactly one, `"s"`
/// otherwise (including zero).
fn plural(n: i64) -> &'static str {
    if n == 1 { "" } else { "s" }
}

impl LicenseCertificate {
    /// The grace notice for this certificate against the current system
    /// clock, or `None` when it is [`LicenseStatus::Valid`].
    ///
    /// A convenience over [`LicenseStatus::grace_message`] for callers
    /// that verified with the system clock (the common case) and don't
    /// otherwise carry a `now` around.
    pub fn grace_notice(&self) -> Option<String> {
        self.status.grace_message(Utc::now())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Organization {
    pub id: String,
    pub name: String,
}

/// The product the license is issued for.  Verification requires
/// `name == `[`EXPECTED_PROJECT_NAME`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Order {
    pub id: String,
    pub expires_at: DateTime<Utc>,
}

/// Why a token was rejected.  Distinct variants per failure mode so
/// the caller can render an actionable error message.
#[derive(Debug)]
pub enum LicenseError {
    Malformed(&'static str),
    Base64(base64::DecodeError),
    BadEd25519Signature,
    BadMlDsaSignature,
    Json(serde_json::Error),
    UnsupportedSchema { found: u32 },
    BadTimestamp(chrono::ParseError),
    Expired {
        expires_at: DateTime<Utc>,
        now: DateTime<Utc>,
    },
    /// The token is issued for a different product — its `project.name`
    /// is not [`EXPECTED_PROJECT_NAME`] (or is absent, carried as an
    /// empty string).
    WrongProject {
        expected: &'static str,
        found: String,
    },
    /// A baked-in trusted-key entry is the wrong length or otherwise
    /// can't be decoded.  Indicates a build mistake (bad paste into
    /// `TRUSTED_*_KEYS_HEX`), not a bad token.
    BadTrustedKey(&'static str),
}

impl fmt::Display for LicenseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(why) => write!(f, "license token is malformed: {why}"),
            Self::Base64(e) => write!(f, "license token base64 decoding failed: {e}"),
            Self::BadEd25519Signature => write!(f, "license Ed25519 signature is invalid"),
            Self::BadMlDsaSignature => write!(f, "license ML-DSA-44 signature is invalid"),
            Self::Json(e) => write!(f, "license payload JSON is invalid: {e}"),
            Self::UnsupportedSchema { found } => write!(
                f,
                "license payload schema version {found} is not supported (expected {SCHEMA_VERSION})"
            ),
            Self::BadTimestamp(e) => write!(
                f,
                "license `order.expires_at` is not a valid RFC 3339 timestamp: {e}"
            ),
            Self::Expired { expires_at, now } => {
                write!(f, "license expired at {expires_at} (now: {now})")
            }
            Self::WrongProject { expected, found } => {
                let found = if found.is_empty() { "<none>" } else { found };
                write!(f, "license is not for project \"{expected}\" (found \"{found}\")")
            }
            Self::BadTrustedKey(why) => {
                write!(f, "license library has a malformed trusted key: {why}")
            }
        }
    }
}

impl std::error::Error for LicenseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Base64(e) => Some(e),
            Self::Json(e) => Some(e),
            Self::BadTimestamp(e) => Some(e),
            _ => None,
        }
    }
}

impl From<base64::DecodeError> for LicenseError {
    fn from(e: base64::DecodeError) -> Self {
        Self::Base64(e)
    }
}

impl From<serde_json::Error> for LicenseError {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}

/// Why locating or validating a license certificate failed.
#[derive(Debug)]
pub enum CertificateError {
    /// No candidate certificate file existed in any searched location.
    /// `searched` lists the paths that were looked at (the explicit
    /// path, or the default `.supso/license_certificates/` dirs).
    NotFound { searched: Vec<PathBuf> },
    /// One or more certificate files were found, but none verified.
    /// Each entry pairs the file tried with a human-readable reason
    /// (an unreadable file or a failed [`LicenseError`]).
    NoneValid { tried: Vec<(PathBuf, String)> },
    /// A baked-in trusted key is malformed — a build mistake in this
    /// library, not a problem with any certificate on disk.
    BadTrustedKey(&'static str),
}

impl fmt::Display for CertificateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound { searched } => {
                write!(f, "no license certificate found (searched: ")?;
                for (i, p) in searched.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", p.display())?;
                }
                write!(f, ")")
            }
            Self::NoneValid { tried } => {
                write!(f, "no valid license certificate ({} tried): ", tried.len())?;
                for (i, (p, why)) in tried.iter().enumerate() {
                    if i > 0 {
                        write!(f, "; ")?;
                    }
                    write!(f, "{}: {why}", p.display())?;
                }
                Ok(())
            }
            Self::BadTrustedKey(why) => {
                write!(f, "license library has a malformed trusted key: {why}")
            }
        }
    }
}

impl std::error::Error for CertificateError {}

impl License {
    /// Verify and parse a token against the baked-in trusted keys
    /// using the current system clock.
    pub fn verify(token: &str) -> Result<Self, LicenseError> {
        Self::verify_at(token, Utc::now())
    }

    /// Verify and parse a token against the baked-in trusted keys,
    /// comparing `expires_at` against the supplied wall-clock
    /// instant.  Split out so tests don't have to mock the system
    /// clock.
    pub fn verify_at(token: &str, now: DateTime<Utc>) -> Result<Self, LicenseError> {
        let ed_keys = parsed_trusted_ed25519_keys()?;
        let pq_keys = parsed_trusted_mldsa44_keys()?;
        verify_with_keys(token, ed_keys, pq_keys, now)
    }

    /// Locate a license certificate on disk and verify it against the
    /// baked-in trusted keys using the current system clock.
    ///
    /// Honours the `SUPSO_LICENSE_PATH` environment variable as an explicit
    /// path; otherwise searches the default `.supso/license_certificates/`
    /// directories (see the module docs).  Returns the verified
    /// [`LicenseCertificate`] — the [`License`] plus the path it was
    /// read from — or a [`CertificateError`] describing why none was
    /// accepted.
    pub fn validate_certificate() -> Result<LicenseCertificate, CertificateError> {
        let explicit = std::env::var_os("SUPSO_LICENSE_PATH").map(PathBuf::from);
        validate_certificate_at(explicit.as_deref(), Utc::now())
    }
}

/// Core verification routine.  Public-but-hidden so tests and
/// downstream tooling can inject custom key sets without mutating
/// the baked-in tables.  End users call [`License::verify`] /
/// [`License::verify_at`].
#[doc(hidden)]
pub fn verify_with_keys(
    token: &str,
    ed25519_keys: &[Ed25519VerifyingKey],
    mldsa44_keys: &[MlDsaVerifyingKey<MlDsa44>],
    now: DateTime<Utc>,
) -> Result<License, LicenseError> {
    match verify_token(token, ed25519_keys, mldsa44_keys, now)? {
        VerifiedToken::Valid(license) => Ok(license),
        VerifiedToken::Expired { expires_at, .. } => {
            Err(LicenseError::Expired { expires_at, now })
        }
    }
}

/// A token that passed every check except, possibly, expiry.
///
/// The signature, schema, and project checks are identical to a full
/// verification; the only difference from [`verify_with_keys`] is that an
/// expired-but-otherwise-valid token is returned as
/// [`Expired`](VerifiedToken::Expired) — carrying its parsed [`License`]
/// — instead of collapsing to a bare error.  This lets the certificate
/// search recover the license behind a lapsed token to honour it under
/// the grace period.
enum VerifiedToken {
    Valid(License),
    Expired {
        license: License,
        expires_at: DateTime<Utc>,
    },
}

/// Run the full token-verification pipeline, distinguishing a live token
/// from one that is valid in every respect but expired.  All rejection
/// reasons other than expiry surface as [`LicenseError`].
fn verify_token(
    token: &str,
    ed25519_keys: &[Ed25519VerifyingKey],
    mldsa44_keys: &[MlDsaVerifyingKey<MlDsa44>],
    now: DateTime<Utc>,
) -> Result<VerifiedToken, LicenseError> {
    let mut segments = token.split('.');
    let payload_b64 = segments
        .next()
        .ok_or(LicenseError::Malformed("missing payload segment"))?;
    let ed_sig_b64 = segments
        .next()
        .ok_or(LicenseError::Malformed("missing Ed25519 signature segment"))?;
    let pq_sig_b64 = segments
        .next()
        .ok_or(LicenseError::Malformed("missing ML-DSA-44 signature segment"))?;
    if segments.next().is_some() {
        return Err(LicenseError::Malformed("more than three segments"));
    }
    if payload_b64.is_empty() || ed_sig_b64.is_empty() || pq_sig_b64.is_empty() {
        return Err(LicenseError::Malformed("empty segment"));
    }

    // ---- Ed25519 signature check ----
    let ed_sig_bytes = URL_SAFE_NO_PAD.decode(ed_sig_b64.as_bytes())?;
    let ed_sig_array: [u8; 64] = ed_sig_bytes
        .as_slice()
        .try_into()
        .map_err(|_| LicenseError::Malformed("Ed25519 signature is not 64 bytes"))?;
    let ed_sig = Ed25519Signature::from_bytes(&ed_sig_array);
    let signed_msg = payload_b64.as_bytes();
    let ed_ok = ed25519_keys
        .iter()
        .any(|vk| vk.verify_strict(signed_msg, &ed_sig).is_ok());
    if !ed_ok {
        return Err(LicenseError::BadEd25519Signature);
    }

    // ---- ML-DSA-44 signature check ----
    let pq_sig_bytes = URL_SAFE_NO_PAD.decode(pq_sig_b64.as_bytes())?;
    let pq_sig_encoded = MlDsaEncodedSignature::<MlDsa44>::try_from(pq_sig_bytes.as_slice())
        .map_err(|_| LicenseError::Malformed("ML-DSA-44 signature is the wrong length"))?;
    let pq_sig = MlDsaSignature::<MlDsa44>::decode(&pq_sig_encoded)
        .ok_or(LicenseError::Malformed("ML-DSA-44 signature failed to decode"))?;
    let pq_ok = mldsa44_keys
        .iter()
        .any(|vk| vk.verify(signed_msg, &pq_sig).is_ok());
    if !pq_ok {
        return Err(LicenseError::BadMlDsaSignature);
    }

    // ---- payload checks ----
    let payload_bytes = URL_SAFE_NO_PAD.decode(payload_b64.as_bytes())?;
    let raw: RawPayload = serde_json::from_slice(&payload_bytes)?;

    if raw.v != SCHEMA_VERSION {
        return Err(LicenseError::UnsupportedSchema { found: raw.v });
    }

    let project_name = raw
        .metadata
        .get("project")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
        .unwrap_or_default()
        .to_string();
    if project_name != EXPECTED_PROJECT_NAME {
        return Err(LicenseError::WrongProject {
            expected: EXPECTED_PROJECT_NAME,
            found: project_name,
        });
    }

    let expires_at = parse_expiry(&raw.order.expires_at).map_err(LicenseError::BadTimestamp)?;

    let license = License {
        organization: Organization {
            id: raw.organization.id,
            name: raw.organization.name,
        },
        project: Project { name: project_name },
        order: Order {
            id: raw.order.id,
            expires_at,
        },
        metadata: raw.metadata,
    };

    if expires_at <= now {
        Ok(VerifiedToken::Expired { license, expires_at })
    } else {
        Ok(VerifiedToken::Valid(license))
    }
}

/// Parse `order.expires_at` into a UTC instant.
///
/// Two forms are accepted:
///
/// * a full RFC 3339 timestamp (`2027-05-29T23:59:59Z`, any offset),
///   used verbatim;
/// * a bare calendar date (`2027-05-29`), which expires at the **end of
///   that day** — `23:59:59Z` — so the license counts through the whole
///   day rather than from its first instant.
fn parse_expiry(s: &str) -> Result<DateTime<Utc>, chrono::ParseError> {
    match DateTime::parse_from_rfc3339(s) {
        Ok(dt) => Ok(dt.with_timezone(&Utc)),
        Err(rfc_err) => match NaiveDate::parse_from_str(s, "%Y-%m-%d") {
            Ok(date) => Ok(date
                .and_hms_opt(23, 59, 59)
                .expect("23:59:59 is a valid time of day")
                .and_utc()),
            // Neither form parsed — surface the RFC 3339 error, since the
            // full timestamp is the canonical shape.
            Err(_) => Err(rfc_err),
        },
    }
}

// ---- license certificates on disk ----

/// The subdirectory under each `.supso/` base that holds certificate
/// files.
pub const CERTIFICATE_SUBDIR: &str = "license_certificates";

/// Locate and verify a license certificate against the baked-in
/// trusted keys, comparing `expires_at` against `now`.
///
/// When `explicit` is `Some`, only that path is consulted (a single
/// certificate file, or a directory of them) — there is no fallback
/// to the defaults.  When `None`, the default
/// `.supso/license_certificates/` directories are searched in order
/// (`$HOME` first, then the current working directory).
///
/// Split out from [`License::validate_certificate`] so tests and
/// tooling can supply a fixed clock.
pub fn validate_certificate_at(
    explicit: Option<&Path>,
    now: DateTime<Utc>,
) -> Result<LicenseCertificate, CertificateError> {
    let ed_keys = parsed_trusted_ed25519_keys()
        .map_err(|e| CertificateError::BadTrustedKey(trusted_key_reason(&e)))?;
    let pq_keys = parsed_trusted_mldsa44_keys()
        .map_err(|e| CertificateError::BadTrustedKey(trusted_key_reason(&e)))?;
    locate_and_verify(explicit, &default_certificate_dirs(), ed_keys, pq_keys, now)
}

/// Locate and verify a certificate at an explicit path using the
/// current system clock — the [`validate_certificate_at`] companion to
/// [`License::validate_certificate`] for callers that already have a
/// path in hand (e.g. a CLI `--path` flag).
pub fn validate_certificate_path(path: &Path) -> Result<LicenseCertificate, CertificateError> {
    validate_certificate_at(Some(path), Utc::now())
}

/// The default certificate directories, in search order: the
/// per-user `$HOME/.supso/license_certificates/` (if a home directory
/// is known) followed by the project-local `./.supso/license_certificates/`.
fn default_certificate_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = home_dir() {
        dirs.push(home.join(".supso").join(CERTIFICATE_SUBDIR));
    }
    dirs.push(Path::new(".supso").join(CERTIFICATE_SUBDIR));
    dirs
}

/// Resolve the user's home directory from the environment without a
/// platform-crate dependency: `$HOME` on Unix, `%USERPROFILE%` on
/// Windows.  An unset or empty value yields `None`.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

/// The most-recently-expired otherwise-valid certificate seen so far,
/// retained so the search can fall back to the grace period when no live
/// certificate turns up.
struct GraceCandidate {
    license: License,
    path: PathBuf,
    expires_at: DateTime<Utc>,
}

/// Core certificate search.  Returns the first certificate that
/// verifies; failing that, the most-recently-expired otherwise-valid
/// certificate if it is still within the grace window; otherwise reports
/// every path tried (`NoneValid`) or, if nothing was even present, the
/// locations searched (`NotFound`).
fn locate_and_verify(
    explicit: Option<&Path>,
    default_dirs: &[PathBuf],
    ed_keys: &[Ed25519VerifyingKey],
    mldsa44_keys: &[MlDsaVerifyingKey<MlDsa44>],
    now: DateTime<Utc>,
) -> Result<LicenseCertificate, CertificateError> {
    let mut tried: Vec<(PathBuf, String)> = Vec::new();
    let mut grace: Option<GraceCandidate> = None;

    if let Some(path) = explicit {
        // An explicit path is authoritative — a directory is scanned,
        // anything else is treated as a single certificate file; in
        // neither case do we fall through to the defaults.
        if !path.exists() {
            return Err(CertificateError::NotFound { searched: vec![path.to_path_buf()] });
        }
        let found = if path.is_dir() {
            scan_dir(path, ed_keys, mldsa44_keys, now, &mut tried, &mut grace)
        } else {
            verify_file(path, ed_keys, mldsa44_keys, now, &mut tried, &mut grace)
        };
        return finalize(found, grace, tried, vec![path.to_path_buf()], now);
    }

    let mut found = None;
    for dir in default_dirs {
        if let Some(cert) = scan_dir(dir, ed_keys, mldsa44_keys, now, &mut tried, &mut grace) {
            found = Some(cert);
            break;
        }
    }

    finalize(found, grace, tried, default_dirs.to_vec(), now)
}

/// Resolve the search down to a single outcome: a live certificate wins;
/// otherwise a grace-eligible lapsed certificate is honoured; otherwise
/// the appropriate error is reported.
fn finalize(
    found: Option<LicenseCertificate>,
    grace: Option<GraceCandidate>,
    tried: Vec<(PathBuf, String)>,
    searched: Vec<PathBuf>,
    now: DateTime<Utc>,
) -> Result<LicenseCertificate, CertificateError> {
    if let Some(cert) = found {
        return Ok(cert);
    }

    if let Some(candidate) = grace {
        let grace_until = candidate.expires_at + grace_period();
        if now <= grace_until {
            return Ok(LicenseCertificate {
                license: candidate.license,
                path: candidate.path,
                status: LicenseStatus::Grace { expired_at: candidate.expires_at, grace_until },
            });
        }
    }

    if tried.is_empty() {
        Err(CertificateError::NotFound { searched })
    } else {
        Err(CertificateError::NoneValid { tried })
    }
}

/// Try every non-hidden regular file in `dir`, in filename order, and
/// return the first whose contents verify.  A missing or unreadable
/// directory contributes no candidates (the caller moves on to the
/// next search location).
fn scan_dir(
    dir: &Path,
    ed_keys: &[Ed25519VerifyingKey],
    mldsa44_keys: &[MlDsaVerifyingKey<MlDsa44>],
    now: DateTime<Utc>,
    tried: &mut Vec<(PathBuf, String)>,
    grace: &mut Option<GraceCandidate>,
) -> Option<LicenseCertificate> {
    let entries = fs::read_dir(dir).ok()?;
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file() && !is_hidden(p))
        .collect();
    // Directory iteration order is unspecified; sort so "first valid
    // certificate wins" is deterministic across runs and platforms.
    files.sort();
    files
        .iter()
        .find_map(|f| verify_file(f, ed_keys, mldsa44_keys, now, tried, grace))
}

/// Read `path`, verify its trimmed contents as a token, and on success
/// wrap the result in a [`LicenseCertificate`].  A token that is valid
/// but expired updates the most-recently-expired grace candidate;
/// read failures and verification failures are recorded in `tried`.  In
/// every non-live case the function yields `None` so the search
/// continues — grace is only consulted once nothing live is found.
fn verify_file(
    path: &Path,
    ed_keys: &[Ed25519VerifyingKey],
    mldsa44_keys: &[MlDsaVerifyingKey<MlDsa44>],
    now: DateTime<Utc>,
    tried: &mut Vec<(PathBuf, String)>,
    grace: &mut Option<GraceCandidate>,
) -> Option<LicenseCertificate> {
    let raw = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            tried.push((path.to_path_buf(), format!("could not read certificate: {e}")));
            return None;
        }
    };
    match verify_token(raw.trim(), ed_keys, mldsa44_keys, now) {
        Ok(VerifiedToken::Valid(license)) => Some(LicenseCertificate {
            license,
            path: path.to_path_buf(),
            status: LicenseStatus::Valid,
        }),
        Ok(VerifiedToken::Expired { license, expires_at }) => {
            tried.push((
                path.to_path_buf(),
                LicenseError::Expired { expires_at, now }.to_string(),
            ));
            // Retain the latest-expiring candidate so a lapse is judged
            // against the freshest license, independent of scan order.
            if grace.as_ref().is_none_or(|g| expires_at > g.expires_at) {
                *grace = Some(GraceCandidate { license, path: path.to_path_buf(), expires_at });
            }
            None
        }
        Err(e) => {
            tried.push((path.to_path_buf(), e.to_string()));
            None
        }
    }
}

fn is_hidden(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with('.'))
}

/// Extract the `&'static str` reason from a trusted-key parse failure
/// so it can cross into [`CertificateError::BadTrustedKey`].  The
/// trusted-key path only ever produces [`LicenseError::BadTrustedKey`].
fn trusted_key_reason(e: &LicenseError) -> &'static str {
    match e {
        LicenseError::BadTrustedKey(s) => s,
        _ => "unexpected trusted-key parse error",
    }
}

// ---- baked-in trusted-key parsing ----
//
// Keys are stored as hex string literals so they're readable in
// `src/lib.rs` and don't require an out-of-band binary fixture.  We
// parse them lazily on first verification and cache the parsed
// `VerifyingKey` instances in a `OnceLock`.

fn parsed_trusted_ed25519_keys()
-> Result<&'static [Ed25519VerifyingKey], LicenseError> {
    static CACHE: OnceLock<Result<Vec<Ed25519VerifyingKey>, LicenseError>> = OnceLock::new();
    match CACHE.get_or_init(|| {
        TRUSTED_ED25519_KEYS_HEX
            .iter()
            .map(|hex| {
                let mut bytes = [0u8; 32];
                hex_decode_into(hex, &mut bytes)
                    .map_err(|_| LicenseError::BadTrustedKey("Ed25519 key hex is malformed"))?;
                Ed25519VerifyingKey::from_bytes(&bytes)
                    .map_err(|_| LicenseError::BadTrustedKey("Ed25519 key bytes are invalid"))
            })
            .collect()
    }) {
        Ok(v) => Ok(v.as_slice()),
        Err(e) => Err(clone_trusted_key_error(e)),
    }
}

fn parsed_trusted_mldsa44_keys()
-> Result<&'static [MlDsaVerifyingKey<MlDsa44>], LicenseError> {
    static CACHE: OnceLock<Result<Vec<MlDsaVerifyingKey<MlDsa44>>, LicenseError>> =
        OnceLock::new();
    match CACHE.get_or_init(|| {
        TRUSTED_MLDSA44_KEYS_HEX
            .iter()
            .map(|hex| {
                let mut bytes = vec![0u8; 1312];
                hex_decode_into(hex, &mut bytes).map_err(|_| {
                    LicenseError::BadTrustedKey("ML-DSA-44 key hex is malformed")
                })?;
                let encoded = MlDsaEncodedVerifyingKey::<MlDsa44>::try_from(bytes.as_slice())
                    .map_err(|_| {
                        LicenseError::BadTrustedKey("ML-DSA-44 key is the wrong length")
                    })?;
                Ok(MlDsaVerifyingKey::<MlDsa44>::decode(&encoded))
            })
            .collect()
    }) {
        Ok(v) => Ok(v.as_slice()),
        Err(e) => Err(clone_trusted_key_error(e)),
    }
}

/// `LicenseError` doesn't implement `Clone` (the inner `serde_json`
/// / `base64` / `chrono` errors don't either).  The trusted-key
/// path only ever produces `BadTrustedKey(&'static str)`, which is
/// trivially cloneable; this helper extracts and re-wraps it so we
/// can return a fresh value from the cached `Result`.
fn clone_trusted_key_error(e: &LicenseError) -> LicenseError {
    match e {
        LicenseError::BadTrustedKey(s) => LicenseError::BadTrustedKey(s),
        _ => LicenseError::BadTrustedKey("unexpected trusted-key parse error"),
    }
}

fn hex_decode_into(hex: &str, out: &mut [u8]) -> Result<(), ()> {
    if hex.len() != out.len() * 2 {
        return Err(());
    }
    for (i, pair) in hex.as_bytes().chunks(2).enumerate() {
        let hi = hex_nibble(pair[0])?;
        let lo = hex_nibble(pair[1])?;
        out[i] = (hi << 4) | lo;
    }
    Ok(())
}

fn hex_nibble(c: u8) -> Result<u8, ()> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(()),
    }
}

// ---------- raw JSON shape (deserialisation only) ----------

#[derive(Deserialize)]
struct RawPayload {
    v: u32,
    organization: RawOrganization,
    order: RawOrder,
    // The product binding lives at `metadata.project.name`; verification
    // reads it from here (see `verify_with_keys`).
    #[serde(default)]
    metadata: BTreeMap<String, serde_json::Value>,
}

#[derive(Deserialize)]
struct RawOrganization {
    id: String,
    name: String,
}

#[derive(Deserialize)]
struct RawOrder {
    id: String,
    expires_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer as Ed25519SignerTrait, SigningKey as Ed25519SigningKey};
    use ml_dsa::{
        Generate, Keypair, SigningKey as MlDsaSigningKey, signature::Signer as _,
    };
    use rand::rngs::OsRng;

    struct Issuer {
        ed: Ed25519SigningKey,
        pq: MlDsaSigningKey<MlDsa44>,
    }

    impl Issuer {
        fn generate() -> Self {
            // `ml-dsa`'s `Generate` impl pulls fresh randomness via
            // `getrandom` internally; ed25519 takes an explicit RNG.
            Self {
                ed: Ed25519SigningKey::generate(&mut OsRng),
                pq: <MlDsaSigningKey<MlDsa44> as Generate>::generate(),
            }
        }

        fn ed_verifying(&self) -> Ed25519VerifyingKey {
            self.ed.verifying_key()
        }

        fn pq_verifying(&self) -> MlDsaVerifyingKey<MlDsa44> {
            self.pq.verifying_key()
        }

        fn mint(&self, payload: serde_json::Value) -> String {
            let payload_json = serde_json::to_vec(&payload).unwrap();
            let payload_b64 = URL_SAFE_NO_PAD.encode(&payload_json);

            let ed_sig = self.ed.sign(payload_b64.as_bytes());
            let ed_sig_b64 = URL_SAFE_NO_PAD.encode(ed_sig.to_bytes());

            let pq_sig = self.pq.sign(payload_b64.as_bytes());
            let pq_sig_encoded: MlDsaEncodedSignature<MlDsa44> = pq_sig.encode();
            let pq_sig_b64 = URL_SAFE_NO_PAD.encode(pq_sig_encoded.as_slice());

            format!("{payload_b64}.{ed_sig_b64}.{pq_sig_b64}")
        }
    }

    fn ts(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn valid_payload() -> serde_json::Value {
        serde_json::json!({
            "v": 1,
            "organization": { "id": "org_test", "name": "Test Co" },
            "order":        { "id": "ord_test", "expires_at": "2030-01-01T00:00:00Z" },
            "metadata":     {
                "support_tier": "standard",
                "seats": 5,
                "project": { "name": "sup-xml" }
            }
        })
    }

    #[test]
    fn round_trip_accepts_valid_token() {
        let issuer = Issuer::generate();
        let token = issuer.mint(valid_payload());

        let license = verify_with_keys(
            &token,
            &[issuer.ed_verifying()],
            &[issuer.pq_verifying()],
            ts("2026-01-01T00:00:00Z"),
        )
        .unwrap();

        assert_eq!(license.organization.id, "org_test");
        assert_eq!(license.organization.name, "Test Co");
        assert_eq!(license.project.name, "sup-xml");
        assert_eq!(license.order.id, "ord_test");
        assert_eq!(license.order.expires_at, ts("2030-01-01T00:00:00Z"));
        assert_eq!(
            license.metadata.get("support_tier").unwrap(),
            &serde_json::json!("standard")
        );
    }

    #[test]
    fn rejects_token_with_only_ed25519_signature() {
        // Strip the third segment so the token looks like the old
        // pre-quantum format.  Hybrid verifier must refuse.
        let issuer = Issuer::generate();
        let token = issuer.mint(valid_payload());
        let two_segments = token.rsplit_once('.').unwrap().0.to_string();

        let err = verify_with_keys(
            &two_segments,
            &[issuer.ed_verifying()],
            &[issuer.pq_verifying()],
            ts("2026-01-01T00:00:00Z"),
        )
        .unwrap_err();
        assert!(matches!(err, LicenseError::Malformed(_)));
    }

    #[test]
    fn rejects_tampered_payload() {
        let issuer = Issuer::generate();
        let token = issuer.mint(valid_payload());

        let mut chars: Vec<char> = token.chars().collect();
        chars[0] = if chars[0] == 'A' { 'B' } else { 'A' };
        let tampered: String = chars.into_iter().collect();

        let err = verify_with_keys(
            &tampered,
            &[issuer.ed_verifying()],
            &[issuer.pq_verifying()],
            ts("2026-01-01T00:00:00Z"),
        )
        .unwrap_err();
        // The flipped byte may surface as a signature failure, a
        // base64 decode failure, or (in the unlikely case it lands
        // inside an already-valid base64 region of the payload) a
        // JSON parse failure.  All are correct reject outcomes.
        assert!(matches!(
            err,
            LicenseError::BadEd25519Signature
                | LicenseError::BadMlDsaSignature
                | LicenseError::Base64(_)
                | LicenseError::Json(_)
        ));
    }

    #[test]
    fn rejects_ed25519_signature_from_wrong_key() {
        let issuer = Issuer::generate();
        let attacker = Issuer::generate();
        // Token signed entirely by the attacker — Ed25519 sig is
        // wrong relative to the real Ed25519 trust list.
        let token = attacker.mint(valid_payload());

        let err = verify_with_keys(
            &token,
            &[issuer.ed_verifying()],
            &[issuer.pq_verifying(), attacker.pq_verifying()],
            ts("2026-01-01T00:00:00Z"),
        )
        .unwrap_err();
        assert!(matches!(err, LicenseError::BadEd25519Signature));
    }

    #[test]
    fn rejects_mldsa44_signature_from_wrong_key() {
        let issuer = Issuer::generate();
        let attacker = Issuer::generate();
        let token = attacker.mint(valid_payload());

        let err = verify_with_keys(
            &token,
            &[issuer.ed_verifying(), attacker.ed_verifying()],
            &[issuer.pq_verifying()],
            ts("2026-01-01T00:00:00Z"),
        )
        .unwrap_err();
        assert!(matches!(err, LicenseError::BadMlDsaSignature));
    }

    #[test]
    fn accepts_signatures_from_any_listed_key_per_scheme() {
        let old_issuer = Issuer::generate();
        let new_issuer = Issuer::generate();
        let token = old_issuer.mint(valid_payload());

        let license = verify_with_keys(
            &token,
            &[new_issuer.ed_verifying(), old_issuer.ed_verifying()],
            &[new_issuer.pq_verifying(), old_issuer.pq_verifying()],
            ts("2026-01-01T00:00:00Z"),
        )
        .unwrap();
        assert_eq!(license.organization.id, "org_test");
    }

    #[test]
    fn rejects_expired_token() {
        let issuer = Issuer::generate();
        let token = issuer.mint(valid_payload());

        let err = verify_with_keys(
            &token,
            &[issuer.ed_verifying()],
            &[issuer.pq_verifying()],
            ts("2031-01-01T00:00:00Z"),
        )
        .unwrap_err();
        assert!(matches!(err, LicenseError::Expired { .. }));
    }

    #[test]
    fn bare_date_expiry_counts_through_end_of_day() {
        let issuer = Issuer::generate();
        let mut payload = valid_payload();
        payload["order"]["expires_at"] = serde_json::json!("2027-05-29");
        let token = issuer.mint(payload);

        // Midday on the expiry date — still valid, and normalised to the
        // end of that day.
        let lic = verify_with_keys(
            &token,
            &[issuer.ed_verifying()],
            &[issuer.pq_verifying()],
            ts("2027-05-29T12:00:00Z"),
        )
        .unwrap();
        assert_eq!(lic.order.expires_at, ts("2027-05-29T23:59:59Z"));

        // The first instant of the next day — expired.
        let err = verify_with_keys(
            &token,
            &[issuer.ed_verifying()],
            &[issuer.pq_verifying()],
            ts("2027-05-30T00:00:00Z"),
        )
        .unwrap_err();
        assert!(matches!(err, LicenseError::Expired { .. }));
    }

    #[test]
    fn unparseable_expiry_is_rejected() {
        let issuer = Issuer::generate();
        let mut payload = valid_payload();
        payload["order"]["expires_at"] = serde_json::json!("sometime next year");
        let token = issuer.mint(payload);

        let err = verify_with_keys(
            &token,
            &[issuer.ed_verifying()],
            &[issuer.pq_verifying()],
            ts("2026-01-01T00:00:00Z"),
        )
        .unwrap_err();
        assert!(matches!(err, LicenseError::BadTimestamp(_)));
    }

    #[test]
    fn rejects_license_for_a_different_project() {
        let issuer = Issuer::generate();
        let mut payload = valid_payload();
        payload["metadata"]["project"]["name"] = serde_json::json!("some-other-product");
        let token = issuer.mint(payload);

        let err = verify_with_keys(
            &token,
            &[issuer.ed_verifying()],
            &[issuer.pq_verifying()],
            ts("2026-01-01T00:00:00Z"),
        )
        .unwrap_err();
        assert!(matches!(err, LicenseError::WrongProject { .. }));
    }

    #[test]
    fn rejects_license_with_no_project() {
        let issuer = Issuer::generate();
        let mut payload = valid_payload();
        payload["metadata"].as_object_mut().unwrap().remove("project");
        let token = issuer.mint(payload);

        let err = verify_with_keys(
            &token,
            &[issuer.ed_verifying()],
            &[issuer.pq_verifying()],
            ts("2026-01-01T00:00:00Z"),
        )
        .unwrap_err();
        assert!(matches!(err, LicenseError::WrongProject { found, .. } if found.is_empty()));
    }

    #[test]
    fn rejects_unsupported_schema_version() {
        let issuer = Issuer::generate();
        let mut payload = valid_payload();
        payload["v"] = serde_json::json!(2);
        let token = issuer.mint(payload);

        let err = verify_with_keys(
            &token,
            &[issuer.ed_verifying()],
            &[issuer.pq_verifying()],
            ts("2026-01-01T00:00:00Z"),
        )
        .unwrap_err();
        assert!(matches!(err, LicenseError::UnsupportedSchema { found: 2 }));
    }

    #[test]
    fn rejects_malformed_token() {
        let issuer = Issuer::generate();
        let err = verify_with_keys(
            "not-a-token",
            &[issuer.ed_verifying()],
            &[issuer.pq_verifying()],
            ts("2026-01-01T00:00:00Z"),
        )
        .unwrap_err();
        assert!(matches!(err, LicenseError::Malformed(_)));
    }

    #[test]
    fn rejects_missing_organization_field() {
        let issuer = Issuer::generate();
        let token = issuer.mint(serde_json::json!({
            "v": 1,
            "order": { "id": "ord_test", "expires_at": "2030-01-01T00:00:00Z" },
            "metadata": {}
        }));

        let err = verify_with_keys(
            &token,
            &[issuer.ed_verifying()],
            &[issuer.pq_verifying()],
            ts("2026-01-01T00:00:00Z"),
        )
        .unwrap_err();
        assert!(matches!(err, LicenseError::Json(_)));
    }

    #[test]
    fn baked_in_keys_reject_token_from_untrusted_issuer() {
        // A well-formed token minted by anyone other than the real
        // issuer must be rejected by the baked-in production keys.
        let issuer = Issuer::generate();
        let token = issuer.mint(valid_payload());
        let err = License::verify_at(&token, ts("2026-01-01T00:00:00Z")).unwrap_err();
        assert!(matches!(err, LicenseError::BadEd25519Signature));
    }

    #[test]
    fn baked_in_trusted_keys_parse() {
        // Guard against a malformed paste into the TRUSTED_*_KEYS_HEX
        // tables: every entry must decode to a usable verifying key.
        let ed = parsed_trusted_ed25519_keys().expect("Ed25519 trust list parses");
        let pq = parsed_trusted_mldsa44_keys().expect("ML-DSA-44 trust list parses");
        assert_eq!(ed.len(), TRUSTED_ED25519_KEYS_HEX.len());
        assert_eq!(pq.len(), TRUSTED_MLDSA44_KEYS_HEX.len());
        assert!(!ed.is_empty(), "production Ed25519 key must be configured");
        assert!(!pq.is_empty(), "production ML-DSA-44 key must be configured");
    }

    // ---- certificate-on-disk tests ----
    //
    // These drive `locate_and_verify` directly with the issuer's keys
    // injected, so they exercise the search/scan logic without relying
    // on the (empty) baked-in trust list or on `$HOME` / the cwd.

    /// A fresh, empty scratch directory unique to this process + tag.
    fn temp_dir(tag: &str) -> PathBuf {
        let mut d = std::env::temp_dir();
        d.push(format!("supso-license-test-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn certificate_found_and_verified_in_directory() {
        let issuer = Issuer::generate();
        let dir = temp_dir("found");
        fs::write(dir.join("acme.cert"), issuer.mint(valid_payload())).unwrap();

        let cert = locate_and_verify(
            None,
            std::slice::from_ref(&dir),
            &[issuer.ed_verifying()],
            &[issuer.pq_verifying()],
            ts("2026-01-01T00:00:00Z"),
        )
        .unwrap();

        assert_eq!(cert.license.organization.id, "org_test");
        assert_eq!(cert.path, dir.join("acme.cert"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn explicit_certificate_file_validates_and_trims_whitespace() {
        let issuer = Issuer::generate();
        let dir = temp_dir("explicit");
        let file = dir.join("license.cert");
        // Trailing newline is common when a token is written by a shell
        // redirect; it must be ignored.
        fs::write(&file, format!("{}\n", issuer.mint(valid_payload()))).unwrap();

        let cert = locate_and_verify(
            Some(&file),
            &[],
            &[issuer.ed_verifying()],
            &[issuer.pq_verifying()],
            ts("2026-01-01T00:00:00Z"),
        )
        .unwrap();

        assert_eq!(cert.path, file);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn scan_skips_invalid_certificate_and_finds_valid_one() {
        let issuer = Issuer::generate();
        let dir = temp_dir("mixed");
        // Filename order matters: the broken one sorts first and must
        // be skipped rather than aborting the search.
        fs::write(dir.join("00-broken.cert"), "garbage").unwrap();
        fs::write(dir.join("99-good.cert"), issuer.mint(valid_payload())).unwrap();

        let cert = locate_and_verify(
            None,
            std::slice::from_ref(&dir),
            &[issuer.ed_verifying()],
            &[issuer.pq_verifying()],
            ts("2026-01-01T00:00:00Z"),
        )
        .unwrap();

        assert_eq!(cert.path, dir.join("99-good.cert"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_and_empty_locations_report_not_found() {
        let issuer = Issuer::generate();
        let empty = temp_dir("empty");
        let missing = empty.join("does-not-exist");

        let err = locate_and_verify(
            None,
            &[missing.clone(), empty.clone()],
            &[issuer.ed_verifying()],
            &[issuer.pq_verifying()],
            ts("2026-01-01T00:00:00Z"),
        )
        .unwrap_err();

        match err {
            CertificateError::NotFound { searched } => {
                assert_eq!(searched, vec![missing, empty.clone()]);
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&empty);
    }

    #[test]
    fn present_but_unverifiable_certificate_reports_none_valid() {
        let issuer = Issuer::generate();
        let dir = temp_dir("invalid");
        fs::write(dir.join("bad.cert"), "not-a-token").unwrap();

        let err = locate_and_verify(
            None,
            std::slice::from_ref(&dir),
            &[issuer.ed_verifying()],
            &[issuer.pq_verifying()],
            ts("2026-01-01T00:00:00Z"),
        )
        .unwrap_err();

        match err {
            CertificateError::NoneValid { tried } => {
                assert_eq!(tried.len(), 1);
                assert_eq!(tried[0].0, dir.join("bad.cert"));
            }
            other => panic!("expected NoneValid, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn expired_certificate_is_not_accepted() {
        let issuer = Issuer::generate();
        let dir = temp_dir("expired");
        fs::write(dir.join("a.cert"), issuer.mint(valid_payload())).unwrap();

        // valid_payload() expires 2030; verify well after that.
        let err = locate_and_verify(
            None,
            std::slice::from_ref(&dir),
            &[issuer.ed_verifying()],
            &[issuer.pq_verifying()],
            ts("2031-01-01T00:00:00Z"),
        )
        .unwrap_err();

        assert!(matches!(err, CertificateError::NoneValid { .. }));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn hidden_files_are_not_treated_as_certificates() {
        let issuer = Issuer::generate();
        let dir = temp_dir("hidden");
        // Even a perfectly valid token hidden in a dotfile is ignored.
        fs::write(dir.join(".license.cert"), issuer.mint(valid_payload())).unwrap();

        let err = locate_and_verify(
            None,
            std::slice::from_ref(&dir),
            &[issuer.ed_verifying()],
            &[issuer.pq_verifying()],
            ts("2026-01-01T00:00:00Z"),
        )
        .unwrap_err();

        assert!(matches!(err, CertificateError::NotFound { .. }));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn explicit_missing_path_reports_not_found_not_none_valid() {
        let issuer = Issuer::generate();
        let err = locate_and_verify(
            Some(Path::new("/no/such/supso/license.cert")),
            &[],
            &[issuer.ed_verifying()],
            &[issuer.pq_verifying()],
            ts("2026-01-01T00:00:00Z"),
        )
        .unwrap_err();

        assert!(matches!(err, CertificateError::NotFound { .. }));
    }

    #[test]
    fn default_dirs_end_with_project_local_path() {
        let dirs = default_certificate_dirs();
        let last = dirs.last().unwrap();
        assert_eq!(last, &Path::new(".supso").join(CERTIFICATE_SUBDIR));
    }

    // ---- grace-period tests ----

    /// `valid_payload()` with its expiry overridden.
    fn payload_expiring(expires_at: &str) -> serde_json::Value {
        let mut p = valid_payload();
        p["order"]["expires_at"] = serde_json::json!(expires_at);
        p
    }

    #[test]
    fn live_certificate_wins_over_an_expired_one() {
        let issuer = Issuer::generate();
        let dir = temp_dir("live-wins");
        // The expired one sorts first; the live one must still win, and
        // no grace status is reported when something live exists.
        fs::write(dir.join("00-old.cert"), issuer.mint(payload_expiring("2026-01-01T00:00:00Z")))
            .unwrap();
        fs::write(dir.join("99-current.cert"), issuer.mint(valid_payload())).unwrap();

        let cert = locate_and_verify(
            None,
            std::slice::from_ref(&dir),
            &[issuer.ed_verifying()],
            &[issuer.pq_verifying()],
            ts("2026-06-01T00:00:00Z"),
        )
        .unwrap();

        assert_eq!(cert.path, dir.join("99-current.cert"));
        assert_eq!(cert.status, LicenseStatus::Valid);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn recently_expired_certificate_is_honoured_under_grace() {
        let issuer = Issuer::generate();
        let dir = temp_dir("grace-honoured");
        fs::write(dir.join("a.cert"), issuer.mint(payload_expiring("2026-01-01T00:00:00Z")))
            .unwrap();

        // Nine days past expiry — inside the 21-day window.
        let cert = locate_and_verify(
            None,
            std::slice::from_ref(&dir),
            &[issuer.ed_verifying()],
            &[issuer.pq_verifying()],
            ts("2026-01-10T00:00:00Z"),
        )
        .unwrap();

        assert_eq!(cert.path, dir.join("a.cert"));
        assert_eq!(
            cert.status,
            LicenseStatus::Grace {
                expired_at: ts("2026-01-01T00:00:00Z"),
                grace_until: ts("2026-01-22T00:00:00Z"),
            }
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn grace_period_lapses_after_the_window() {
        let issuer = Issuer::generate();
        let dir = temp_dir("grace-lapsed");
        fs::write(dir.join("a.cert"), issuer.mint(payload_expiring("2026-01-01T00:00:00Z")))
            .unwrap();

        // Thirty-one days past expiry — beyond the 21-day window, so the
        // certificate is rejected like any plainly-expired one.
        let err = locate_and_verify(
            None,
            std::slice::from_ref(&dir),
            &[issuer.ed_verifying()],
            &[issuer.pq_verifying()],
            ts("2026-02-01T00:00:00Z"),
        )
        .unwrap_err();

        assert!(matches!(err, CertificateError::NoneValid { .. }));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn grace_tracks_the_most_recently_expired_regardless_of_order() {
        let issuer = Issuer::generate();
        let dir = temp_dir("grace-most-recent");
        // The fresher expiry sorts *last* by filename — the grace pick
        // must be by expiry, not scan order.
        fs::write(dir.join("a-older.cert"), issuer.mint(payload_expiring("2026-01-01T00:00:00Z")))
            .unwrap();
        fs::write(dir.join("z-newer.cert"), issuer.mint(payload_expiring("2026-01-10T00:00:00Z")))
            .unwrap();

        let cert = locate_and_verify(
            None,
            std::slice::from_ref(&dir),
            &[issuer.ed_verifying()],
            &[issuer.pq_verifying()],
            ts("2026-01-15T00:00:00Z"),
        )
        .unwrap();

        assert_eq!(cert.path, dir.join("z-newer.cert"));
        assert_eq!(
            cert.status,
            LicenseStatus::Grace {
                expired_at: ts("2026-01-10T00:00:00Z"),
                grace_until: ts("2026-01-31T00:00:00Z"),
            }
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_bad_signature_never_grants_grace() {
        // An expired token signed by an untrusted issuer fails the
        // signature check, so it is *not* a grace candidate even though
        // its expiry is recent.
        let real = Issuer::generate();
        let impostor = Issuer::generate();
        let dir = temp_dir("grace-bad-sig");
        fs::write(dir.join("a.cert"), impostor.mint(payload_expiring("2026-01-01T00:00:00Z")))
            .unwrap();

        let err = locate_and_verify(
            None,
            std::slice::from_ref(&dir),
            &[real.ed_verifying()],
            &[real.pq_verifying()],
            ts("2026-01-10T00:00:00Z"),
        )
        .unwrap_err();

        match err {
            CertificateError::NoneValid { tried } => {
                assert!(tried[0].1.contains("Ed25519"), "expected a signature rejection");
            }
            other => panic!("expected NoneValid, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn grace_message_reports_days_elapsed_and_remaining() {
        let status = LicenseStatus::Grace {
            expired_at: ts("2026-01-01T00:00:00Z"),
            grace_until: ts("2026-01-22T00:00:00Z"),
        };
        // Two days after expiry, nineteen left in the window.
        let msg = status.grace_message(ts("2026-01-03T00:00:00Z")).unwrap();
        assert!(msg.contains("expired 2 days ago"), "got: {msg}");
        assert!(msg.contains("failing in 19 days"), "got: {msg}");

        // The count rolls to singular at exactly one day.
        let one = LicenseStatus::Grace {
            expired_at: ts("2026-01-01T00:00:00Z"),
            grace_until: ts("2026-01-22T00:00:00Z"),
        };
        let msg = one.grace_message(ts("2026-01-21T00:00:00Z")).unwrap();
        assert!(msg.contains("failing in 1 day"), "got: {msg}");
        assert!(!msg.contains("1 days"), "expected singular day, got: {msg}");

        assert!(LicenseStatus::Valid.grace_message(ts("2026-01-03T00:00:00Z")).is_none());
    }
}
