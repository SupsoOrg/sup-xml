#![forbid(unsafe_code)]

/// Options for the lenient HTML5 parser.
///
/// Construct via [`HtmlParseOptions::default()`] and override individual
/// fields.  Defaults are tuned for the common case (browser-equivalent
/// output, lenient recovery).
///
/// HTML parsing differs from XML in that *recovery is the normal mode*
/// — most real-world HTML is malformed and the WHATWG spec mandates
/// "do something sensible anyway."  `recovery_mode` therefore defaults
/// to `true` (inverted from [`crate::ParseOptions`]).
#[derive(Debug, Clone)]
pub struct HtmlParseOptions {
    /// Reject inputs deeper than this — DoS protection.  html5ever
    /// itself has no built-in depth limit; we enforce it inside the
    /// sink.  Default: 256.
    pub max_element_depth: u32,

    /// Maximum total bytes of accumulated text content across the
    /// whole document.  Caps adversarial inputs that try to blow up
    /// memory through repeated entity expansion or massive text
    /// nodes.  Default: 10 MB.
    pub max_text_bytes: u64,

    /// Treat the parser as if scripting were enabled.  Affects how
    /// `<noscript>` content is parsed: when `true`, `<noscript>`
    /// content is treated as raw text and not parsed as elements
    /// (matching what a browser with JS enabled would see).  When
    /// `false`, `<noscript>` content is parsed normally.  Default:
    /// `true` — most scrapers want the JS-enabled view.
    pub scripting_enabled: bool,

    /// Discard a leading byte-order mark if present in the input.
    /// Default: `true`.
    pub discard_bom: bool,

    /// Treat input as if it came from an iframe `srcdoc` attribute.
    /// Affects quirks-mode determination from DOCTYPE.  Default:
    /// `false`.
    pub iframe_srcdoc: bool,

    /// Continue past parse errors instead of returning `Err`.
    /// Recovery is the *normal* mode for HTML — most callers want
    /// this on.  Default: `true` (inverted vs. [`crate::ParseOptions`],
    /// which defaults to strict because XML is a strict format).
    ///
    /// When `false`, the first parse error reported by html5ever
    /// causes the parser to return `Err` from `finish()`.  Useful
    /// for HTML linters and strict validators.
    pub recovery_mode: bool,

    /// Override the WHATWG byte-stream encoding-sniffing result with
    /// a caller-supplied label (e.g. from an HTTP `Content-Type`
    /// header).  Wins over BOM and meta-charset detection.
    ///
    /// Accepts any WHATWG encoding label — `"UTF-8"`, `"windows-1252"`,
    /// `"Shift_JIS"`, etc.  Unknown labels route through
    /// `encoding_rs` (when the `full-encodings` feature is on).
    ///
    /// Default: `None` (let the sniffer decide).
    pub encoding_override: Option<String>,

    /// Maximum bytes the meta-charset prescan looks at when no
    /// caller-supplied encoding and no BOM are present.  WHATWG
    /// recommends 1024.  Increase only for documents with very
    /// long `<head>` sections that push the `<meta charset>` past
    /// the default window.
    ///
    /// Default: 1024.
    pub encoding_sniff_window: usize,
}

impl Default for HtmlParseOptions {
    fn default() -> Self {
        Self {
            max_element_depth: 256,
            max_text_bytes: 10 * 1024 * 1024,
            scripting_enabled: true,
            discard_bom: true,
            iframe_srcdoc: false,
            recovery_mode: true,
            encoding_override: None,
            encoding_sniff_window: 1024,
        }
    }
}
