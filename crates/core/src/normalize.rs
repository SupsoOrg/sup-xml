//! Unicode normalization (Unicode Annex #15) for the XPath/XSLT
//! `fn:normalize-unicode` function, the `normalization-form`
//! serialization parameter, and NFC-folding of URI attribute values
//! under the html/xhtml `escape-uri-attributes` rule.
//!
//! The four standard forms (NFC/NFD/NFKC/NFKD) are delegated to the
//! `unicode-normalization` crate.  The XSLT-specific `fully-normalized`
//! form is not implemented; callers that need it fall back to NFC,
//! which is correct for text that does not begin with a combining mark.

use unicode_normalization::UnicodeNormalization;

/// A Unicode normalization form, parsed from a `fn:normalize-unicode`
/// argument or a `normalization-form` serialization parameter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NormForm {
    /// No normalization — `fn:normalize-unicode(s, '')` and the
    /// serialization default `none` both leave the string unchanged.
    None,
    Nfc,
    Nfd,
    Nfkc,
    Nfkd,
}

impl NormForm {
    /// Parse a form name, case-insensitively and ignoring surrounding
    /// whitespace (F&O §5.2.4).  The empty string and `none` denote no
    /// normalization.  `fully-normalized` is accepted as NFC (the
    /// XSLT-only form is otherwise unimplemented).  Returns `None` for
    /// an unrecognized form, which the caller reports as FOCH0003.
    pub fn parse(name: &str) -> Option<NormForm> {
        match name.trim().to_ascii_uppercase().as_str() {
            "" | "NONE"        => Some(NormForm::None),
            "NFC"              => Some(NormForm::Nfc),
            "NFD"              => Some(NormForm::Nfd),
            "NFKC"             => Some(NormForm::Nfkc),
            "NFKD"             => Some(NormForm::Nfkd),
            "FULLY-NORMALIZED" => Some(NormForm::Nfc),
            _                  => None,
        }
    }
}

/// Normalize `s` to the requested form.  `NormForm::None` returns `s`
/// unchanged.
pub fn normalize(s: &str, form: NormForm) -> String {
    match form {
        NormForm::None => s.to_string(),
        NormForm::Nfc  => s.nfc().collect(),
        NormForm::Nfd  => s.nfd().collect(),
        NormForm::Nfkc => s.nfkc().collect(),
        NormForm::Nfkd => s.nfkd().collect(),
    }
}

/// Convenience NFC normalization (the most common form — used by the
/// `escape-uri-attributes` URI folding).
pub fn nfc(s: &str) -> String {
    s.nfc().collect()
}
