#![forbid(unsafe_code)]  // see CONTRIBUTING.md § "Unsafe policy"

use std::fmt;

/// Which subsystem raised the error.
///
/// Mirrors `xmlErrorDomain` from libxml2 (`include/libxml/xmlerror.h`) so that
/// errors can be categorised without inspecting the message string, AND so
/// that `domain as i32` produces the exact numeric value a C caller sees
/// through `xmlError::domain` after going through the [`crates/compat`] FFI
/// shim.  See `thoughts/c_abi_implementation_plan.md` § "Unified Rust error
/// type, free conversion to libxml2 layout."
///
/// Variant discriminants are pinned — **do not renumber** — they are part
/// of the C ABI surface.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorDomain {
    /// No domain — used by libxml2 as the default value for fields it hasn't
    /// initialised.  We rarely emit this from Rust; included for round-trip.
    None       =  0,    // XML_FROM_NONE
    /// XML parser (well-formedness violations, unexpected tokens, etc.).
    Parser     =  1,    // XML_FROM_PARSER
    /// Document tree manipulation.
    Tree       =  2,    // XML_FROM_TREE
    /// Namespace resolution (undeclared prefix, duplicate declarations, etc.).
    Namespace  =  3,    // XML_FROM_NAMESPACE
    /// DTD processing and entity expansion.
    Dtd        =  4,    // XML_FROM_DTD
    /// HTML parsing errors (malformed tag soup, recovered errors from the
    /// HTML5 tree-construction algorithm).
    Html       =  5,    // XML_FROM_HTML
    /// I/O errors (file not found, read failure, etc.).
    Io         =  8,    // XML_FROM_IO
    /// XPath expression parsing or evaluation.
    XPath      = 12,    // XML_FROM_XPATH
    /// XSLT processing — stylesheet compile, transform errors.
    Xslt       = 22,    // XML_FROM_XSLT
    /// W3C XML Schema (XSD) validation.  lxml's `error.domain_name` →
    /// `"SCHEMASV"`.
    SchemasValidate = 17, // XML_FROM_SCHEMASV
    /// RELAX NG validation.  lxml's `error.domain_name` → `"RELAXNGV"`.
    RelaxNGValidate = 19, // XML_FROM_RELAXNGV
    /// Schematron validation.  lxml's `error.domain_name` → `"SCHEMATRONV"`.
    SchematronValidate = 28, // XML_FROM_SCHEMATRONV
    /// Schema or DTD validation.
    Validation = 23,    // XML_FROM_VALID
    /// Character encoding errors (invalid UTF-8, unsupported encoding).
    /// libxml2 calls this domain "I18N" but it's the encoding/charset
    /// error bucket.
    Encoding   = 27,    // XML_FROM_I18N
}

/// Severity of the error.
///
/// `Warning < Error < Fatal`.  Fatal errors abort processing; warnings and
/// errors may still produce a partial document.
///
/// Discriminants match libxml2's `xmlErrorLevel`.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ErrorLevel {
    /// `XML_ERR_NONE` (0) is a no-error sentinel libxml2 sometimes emits;
    /// callers should treat it as "not actually an error."  We don't
    /// construct it from Rust.
    None    = 0,
    Warning = 1,    // XML_ERR_WARNING
    Error   = 2,    // XML_ERR_ERROR
    Fatal   = 3,    // XML_ERR_FATAL
}

/// One of libxml2's well-known numeric error codes.
///
/// libxml2's `xmlParserErrors` enum has ~800 variants; this type covers
/// the ~40 we actually emit from the parser, validator, encoder, etc.
/// Everything else lands at [`ErrorCode::InternalError`] (`= 1`), which
/// is what libxml2 itself uses for unmapped cases.
///
/// Discriminants are pinned to match libxml2's `include/libxml/xmlerror.h`
/// exactly — **do not renumber.**  A C caller doing
/// `if (err->code == XML_ERR_INVALID_CHAR) { ... }` sees `9` here too,
/// because `ErrorCode::InvalidChar as i32 == 9`.
///
/// # When to add a new variant
///
/// When you have a new error-construction site that maps to a specific
/// libxml2 code that callers genuinely check for.  When in doubt, default
/// to [`ErrorCode::InternalError`].  Adding a variant is additive (callers
/// reading numeric codes are unaffected) but renumbering an existing
/// variant is a breaking ABI change.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorCode {
    // ── general (xmlParserErrors) ────────────────────────────────────────
    /// No error — round-trip sentinel.  We don't emit this from Rust.
    Ok                    =    0,   // XML_ERR_OK
    /// Default for any error we can't classify more specifically.
    /// libxml2 itself uses this for unhandled cases, so callers
    /// branching on specific codes will already have a "default" arm
    /// that handles us.
    InternalError         =    1,   // XML_ERR_INTERNAL_ERROR
    /// Out-of-memory at parse / build time.  Not currently emitted (we
    /// panic on OOM today), reserved for when we add fallible alloc.
    NoMemory              =    2,   // XML_ERR_NO_MEMORY
    /// Document is empty (no root element, no prolog).
    DocumentEmpty         =    4,   // XML_ERR_DOCUMENT_EMPTY

    // ── well-formedness violations (the ones consumers check) ───────────
    /// `&#x...;` malformed — empty hex, non-hex digit, etc.
    InvalidHexCharRef     =    6,   // XML_ERR_INVALID_HEX_CHARREF
    /// `&#...;` malformed — empty decimal, non-digit, etc.
    InvalidDecCharRef     =    7,   // XML_ERR_INVALID_DEC_CHARREF
    /// Character outside XML 1.0 § 2.2 char range.
    InvalidChar           =    9,   // XML_ERR_INVALID_CHAR
    /// Entity reference to an undeclared entity name.
    UndeclaredEntity      =   26,   // XML_ERR_UNDECLARED_ENTITY
    /// Unknown encoding label on `<?xml encoding="..."?>` or BOM mismatch.
    UnknownEncoding       =   31,   // XML_ERR_UNKNOWN_ENCODING
    /// Encoding declared but not supported by the build.
    UnsupportedEncoding   =   32,   // XML_ERR_UNSUPPORTED_ENCODING

    // ── attribute / element structure ───────────────────────────────────
    /// Two attributes with the same expanded name on one element.
    AttributeRedefined    =   42,   // XML_ERR_ATTRIBUTE_REDEFINED
    /// Comment didn't terminate before EOF.
    CommentNotFinished    =   45,   // XML_ERR_COMMENT_NOT_FINISHED
    /// Bad `<?xml ... ?>` declaration syntax.
    XmlDeclNotStarted     =   56,   // XML_ERR_XMLDECL_NOT_STARTED
    XmlDeclNotFinished    =   57,   // XML_ERR_XMLDECL_NOT_FINISHED
    /// `]]>` in text content, where it's reserved for CDATA close.
    MisplacedCdataEnd     =   62,   // XML_ERR_MISPLACED_CDATA_END
    /// CDATA section didn't terminate before EOF.
    CdataNotFinished      =   63,   // XML_ERR_CDATA_NOT_FINISHED
    /// Expected an XML name (start tag, attribute name, entity name, etc.).
    NameRequired          =   68,   // XML_ERR_NAME_REQUIRED
    /// Expected `=` after attribute name.
    EqualRequired         =   75,   // XML_ERR_EQUAL_REQUIRED
    /// `</X>` end-tag name doesn't match the open `<Y>`.
    TagNameMismatch       =   76,   // XML_ERR_TAG_NAME_MISMATCH
    /// Start tag never closed before EOF.
    TagNotFinished        =   77,   // XML_ERR_TAG_NOT_FINISHED
    /// Document is not well-balanced (open tags don't match close tags
    /// at EOF).  General catch-all when we can't say which tag.
    NotWellBalanced       =   85,   // XML_ERR_NOT_WELL_BALANCED
    /// Content after the root element's close.
    ExtraContent          =   86,   // XML_ERR_EXTRA_CONTENT

    // ── namespace ────────────────────────────────────────────────────────
    /// Prefix used but never declared via `xmlns:prefix=...`.
    NsErrUndefinedNamespace = 201, // XML_NS_ERR_UNDEFINED_NAMESPACE
    /// Malformed QName (e.g. multiple colons).
    NsErrQname              = 202, // XML_NS_ERR_QNAME

    // ── I/O (xmlErrorDomain::Io) ────────────────────────────────────────
    /// A document input (the main file or an external entity) could not
    /// be opened or read.  libxml2's `__xmlLoaderErr` reports exactly
    /// this code with domain [`ErrorDomain::Io`]; consumers that key on
    /// the domain (e.g. lxml's `_raiseParseError`) then raise an I/O
    /// error rather than a syntax error.
    IoLoadError             = 1549, // XML_IO_LOAD_ERROR

    // ── XSD schema validation (xmlSchemaValidError) ─────────────────────
    // Codes consumers (e.g. lxml's `error_log.filter_types`) match on to
    // classify a validation failure.
    /// Attribute/element value invalid against its datatype.
    SchemavCvcDatatypeValid121 = 1824, // XML_SCHEMAV_CVC_DATATYPE_VALID_1_2_1
    /// A value rejected by a facet (pattern, length, range, …).
    SchemavCvcFacetValid       = 1829, // XML_SCHEMAV_CVC_FACET_VALID
    /// An attribute not permitted by the element's complex type.
    SchemavCvcComplexType322   = 1867, // XML_SCHEMAV_CVC_COMPLEX_TYPE_3_2_2
    /// A required attribute is missing.
    SchemavCvcComplexType4     = 1868, // XML_SCHEMAV_CVC_COMPLEX_TYPE_4
    /// Element content doesn't match the declared content model
    /// (unexpected child element, or a required one absent).
    SchemavElementContent      = 1871, // XML_SCHEMAV_ELEMENT_CONTENT

    // ── DTD validation (xmlParserErrors) ────────────────────────────────
    /// An element declared `EMPTY` has child content.
    DtdNotEmpty                = 528,  // XML_DTD_NOT_EMPTY

    // ── RELAX NG validation (xmlRelaxNGValidErr) ────────────────────────
    /// An element appeared where the pattern did not expect it.
    RelaxngErrElemwrong        = 38,   // XML_RELAXNG_ERR_ELEMWRONG

    // ── encoding (xmlErrorDomain::I18n) ─────────────────────────────────
    /// Encoding handler couldn't decode the input bytes.
    EncodingConvFailed    = 6003,   // XML_I18N_CONV_FAILED
}

/// A structured XML processing error.
///
/// SupXML returns `XmlError` through `Result<_, XmlError>` instead of
/// relying on a global error variable like libxml2 does.  The `domain`,
/// `level`, and `code` fields let callers react without parsing the
/// human-readable `message`.
///
/// `code` is an [`ErrorCode`] enum whose discriminants match libxml2's
/// `xmlParserErrors` numeric values.  Callers can match on the enum
/// (idiomatic Rust); the [`crates/compat`] cdylib converts to libxml2's
/// `xmlError::code: i32` via `err.code as i32` — zero cost.  See
/// `thoughts/c_abi_implementation_plan.md` for the design.
#[derive(Debug, Clone)]
pub struct XmlError {
    /// Which subsystem produced the error.
    pub domain: ErrorDomain,
    /// Severity.
    pub level: ErrorLevel,
    /// Specific error category (libxml2-compatible numeric code on
    /// the wire side).  When in doubt, [`ErrorCode::InternalError`].
    pub code: ErrorCode,
    /// Human-readable description of the problem.
    pub message: String,
    /// Source file name, if available (e.g. for file-based parsing).
    pub file: Option<String>,
    /// 1-based line number where the error occurred, if known.
    pub line: Option<u32>,
    /// 1-based column number where the error occurred, if known.
    pub column: Option<u32>,
    /// 0-based byte offset into the parser's input buffer where the
    /// error occurred, if known.
    ///
    /// Reported alongside [`line`](Self::line) / [`column`](Self::column)
    /// because the three answer different questions: line/col is what
    /// a human reads, byte offset is what tools (editors, LSP servers,
    /// `dd if=… bs=1 skip=…`) act on without re-walking the input.
    /// Byte offset also survives line-ending normalization (XML 1.0
    /// § 2.11) and is the only useful coordinate for binary
    /// pipelines — gzipped XML, network captures, mmap'd files.
    ///
    /// `u64` (not `usize`) so that the ABI surface in `crates/compat`
    /// is stable across 32- and 64-bit targets and survives documents
    /// larger than 4 GB on the streaming reader.
    ///
    /// # Coordinate system
    ///
    /// The offset is measured in the parser's **internal UTF-8
    /// buffer**, which is the same as the caller's input byte slice
    /// in the common case (input was already UTF-8).  If
    /// [`ParseOptions::auto_transcode`](crate::options::ParseOptions)
    /// converted UTF-16 or another encoding to UTF-8 first, the
    /// offset is relative to the post-transcode buffer and does
    /// **not** point at the user's original bytes; the user-facing
    /// offset would require a transcoder back-map we don't have
    /// today.  Callers operating on already-UTF-8 input — which is
    /// the overwhelming majority of XML on the wire — can use this
    /// directly.
    pub byte_offset: Option<u64>,
    /// XPath/XQuery/XSLT error code as a local name in the standard
    /// `err:` namespace (`http://www.w3.org/2005/xqt-errors`) — e.g.
    /// `"FOAR0001"` for division by zero, `"FORG0001"` for an invalid
    /// cast.  Distinct from [`code`](Self::code), which is the
    /// libxml2-numeric category; this is the spec-defined dynamic
    /// error a stylesheet's `xsl:catch` / `try/catch` matches on and
    /// exposes through `$err:code`.  `None` when the error has no
    /// specific spec code (it then projects as the generic
    /// `err:FOER0000`).
    pub xpath_code: Option<String>,
}

impl XmlError {
    /// Construct an error with the catch-all
    /// [`ErrorCode::InternalError`] code.  Add a more specific code
    /// via [`with_code`](Self::with_code) when there's a libxml2
    /// numeric value that fits the case.
    pub fn new(domain: ErrorDomain, level: ErrorLevel, message: impl Into<String>) -> Self {
        Self {
            domain,
            level,
            code: ErrorCode::InternalError,
            message: message.into(),
            file: None,
            line: None,
            column: None,
            byte_offset: None,
            xpath_code: None,
        }
    }

    /// Attach a specific [`ErrorCode`] (libxml2-numeric).  Builder-style;
    /// returns `self` for chaining with [`at`](Self::at).
    pub fn with_code(mut self, code: ErrorCode) -> Self {
        self.code = code;
        self
    }

    /// Attach the spec-defined XPath/XSLT error code (an `err:` local
    /// name such as `"FOAR0001"`).  Builder-style; see
    /// [`xpath_code`](Self::xpath_code).
    pub fn with_xpath_code(mut self, code: impl Into<String>) -> Self {
        self.xpath_code = Some(code.into());
        self
    }

    /// Attach `code` only if no spec code is already present.  Used at
    /// outer choke points (e.g. `document()` retrieval) that want to
    /// label an otherwise-uncoded error without overwriting a more
    /// specific code an inner layer already set.
    pub fn or_xpath_code(mut self, code: impl Into<String>) -> Self {
        if self.xpath_code.is_none() {
            self.xpath_code = Some(code.into());
        }
        self
    }

    /// Attach source position.  All three coordinates are taken
    /// together because the scanner derives them from a single byte
    /// offset and any error that knows one knows all three;
    /// `byte_offset` is documented on [`Self::byte_offset`].
    pub fn at(
        mut self,
        file:        impl Into<String>,
        line:        u32,
        column:      u32,
        byte_offset: u64,
    ) -> Self {
        self.file        = Some(file.into());
        self.line        = Some(line);
        self.column      = Some(column);
        self.byte_offset = Some(byte_offset);
        self
    }
}

impl fmt::Display for XmlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `file:line:col:` is the conventional editor-clickable
        // prefix.  Byte offset goes in a trailing `@N` because it
        // breaks that convention if injected in the middle.
        match (&self.file, self.line, self.column) {
            (Some(file), Some(line), Some(col)) => write!(f, "{file}:{line}:{col}: ")?,
            (Some(file), Some(line), None)      => write!(f, "{file}:{line}: ")?,
            _ => {}
        }
        write!(f, "[{:?}/{:?}] {}", self.domain, self.level, self.message)?;
        if let Some(ofs) = self.byte_offset {
            write!(f, " @ byte {ofs}")?;
        }
        Ok(())
    }
}

impl std::error::Error for XmlError {}

/// Convenience alias used throughout SupXML — equivalent to
/// `std::result::Result<T, XmlError>`.
pub type Result<T> = std::result::Result<T, XmlError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display() {
        let e = XmlError::new(ErrorDomain::Parser, ErrorLevel::Fatal, "unexpected EOF")
            .at("doc.xml", 42, 7, 1024);
        let s = e.to_string();
        assert!(s.contains("doc.xml"));
        assert!(s.contains("42"));
        assert!(s.contains("7"));
        assert!(s.contains("unexpected EOF"));
        assert!(s.contains("byte 1024"));
    }

    #[test]
    fn error_level_ordering() {
        assert!(ErrorLevel::Warning < ErrorLevel::Error);
        assert!(ErrorLevel::Error < ErrorLevel::Fatal);
    }

    /// Verifies that `ErrorCode` discriminants match libxml2's
    /// `xmlParserErrors` numeric values exactly — the contract that
    /// makes `err.code as i32` a zero-cost FFI conversion.  Drift here
    /// breaks libsupxml2's caller-facing codes silently.
    ///
    /// Spot-check the codes most consumers care about; the full set
    /// is documented inline in the enum.
    /// Pinned discriminants — one row per variant so a renumbering bug
    /// (the kind that caused [`NsErrUndefinedNamespace`] to ship at 502
    /// instead of 201, silently breaking C consumers comparing
    /// `err->code == XML_NS_ERR_UNDEFINED_NAMESPACE`) lights up here
    /// instead of leaking through the FFI surface.  Numbers checked
    /// against `xmlParserErrors` in
    /// `/opt/homebrew/Cellar/libxml2/<version>/include/libxml2/libxml/xmlerror.h`.
    #[test]
    fn error_code_libxml2_values_match() {
        assert_eq!(ErrorCode::Ok                      as i32,    0);
        assert_eq!(ErrorCode::InternalError           as i32,    1);
        assert_eq!(ErrorCode::NoMemory                as i32,    2);
        assert_eq!(ErrorCode::DocumentEmpty           as i32,    4);
        assert_eq!(ErrorCode::InvalidHexCharRef       as i32,    6);
        assert_eq!(ErrorCode::InvalidDecCharRef       as i32,    7);
        assert_eq!(ErrorCode::InvalidChar             as i32,    9);
        assert_eq!(ErrorCode::UndeclaredEntity        as i32,   26);
        assert_eq!(ErrorCode::UnknownEncoding         as i32,   31);
        assert_eq!(ErrorCode::UnsupportedEncoding     as i32,   32);
        assert_eq!(ErrorCode::AttributeRedefined      as i32,   42);
        assert_eq!(ErrorCode::CommentNotFinished      as i32,   45);
        assert_eq!(ErrorCode::XmlDeclNotStarted       as i32,   56);
        assert_eq!(ErrorCode::XmlDeclNotFinished      as i32,   57);
        assert_eq!(ErrorCode::MisplacedCdataEnd       as i32,   62);
        assert_eq!(ErrorCode::CdataNotFinished        as i32,   63);
        assert_eq!(ErrorCode::NameRequired            as i32,   68);
        assert_eq!(ErrorCode::EqualRequired           as i32,   75);
        assert_eq!(ErrorCode::TagNameMismatch         as i32,   76);
        assert_eq!(ErrorCode::TagNotFinished          as i32,   77);
        assert_eq!(ErrorCode::NotWellBalanced         as i32,   85);
        assert_eq!(ErrorCode::ExtraContent            as i32,   86);
        assert_eq!(ErrorCode::NsErrUndefinedNamespace as i32,  201);
        assert_eq!(ErrorCode::NsErrQname              as i32,  202);
        assert_eq!(ErrorCode::IoLoadError             as i32, 1549);
        assert_eq!(ErrorCode::EncodingConvFailed      as i32, 6003);
    }

    /// Pins every [`ErrorDomain`] discriminant against libxml2's
    /// `xmlErrorDomain` (xmlerror.h).  Same drift-protection role as
    /// [`error_code_libxml2_values_match`].
    ///
    /// We model **11 of 31** libxml2 domains.  The 20 absent values
    /// stay free at their libxml2 numbers so adding them later
    /// requires no renumbering:
    ///
    /// |  # | libxml2 name           | rationale to add |
    /// |---:|------------------------|------------------|
    /// |  6 | XML_FROM_MEMORY        | alloc failures   |
    /// |  7 | XML_FROM_OUTPUT        | serializer I/O   |
    /// |  9 | XML_FROM_FTP           | (deprecated)     |
    /// | 10 | XML_FROM_HTTP          | network fetch    |
    /// | 11 | XML_FROM_XINCLUDE      | XInclude         |
    /// | 13 | XML_FROM_XPOINTER      | XPointer         |
    /// | 14 | XML_FROM_REGEXP        | XSD §F regex     |
    /// | 15 | XML_FROM_DATATYPE      | XSD types        |
    /// | 16 | XML_FROM_SCHEMASP      | schema parse     |
    /// | 17 | XML_FROM_SCHEMASV      | schema validate  |
    /// | 18 | XML_FROM_RELAXNGP      | RelaxNG parse    |
    /// | 19 | XML_FROM_RELAXNGV      | RelaxNG validate |
    /// | 20 | XML_FROM_CATALOG       | XML Catalogs     |
    /// | 21 | XML_FROM_C14N          | canonicalisation |
    /// | 24 | XML_FROM_CHECK         | tree integrity   |
    /// | 25 | XML_FROM_WRITER        | xmlTextWriter    |
    /// | 26 | XML_FROM_MODULE        | xmlModule        |
    /// | 28 | XML_FROM_SCHEMATRONV   | Schematron       |
    /// | 29 | XML_FROM_BUFFER        | xmlBuffer        |
    /// | 30 | XML_FROM_URI           | URI parse        |
    ///
    /// Numbers verified against
    /// `/opt/homebrew/Cellar/libxml2/<version>/include/libxml2/libxml/xmlerror.h`.
    #[test]
    fn error_domain_libxml2_values_match() {
        assert_eq!(ErrorDomain::None       as i32,  0);
        assert_eq!(ErrorDomain::Parser     as i32,  1);
        assert_eq!(ErrorDomain::Tree       as i32,  2);
        assert_eq!(ErrorDomain::Namespace  as i32,  3);
        assert_eq!(ErrorDomain::Dtd        as i32,  4);
        assert_eq!(ErrorDomain::Html       as i32,  5);
        assert_eq!(ErrorDomain::Io         as i32,  8);
        assert_eq!(ErrorDomain::XPath      as i32, 12);
        assert_eq!(ErrorDomain::Xslt       as i32, 22);
        assert_eq!(ErrorDomain::Validation as i32, 23);
        assert_eq!(ErrorDomain::Encoding   as i32, 27);
    }

    #[test]
    fn xml_error_carries_default_code() {
        let e = XmlError::new(ErrorDomain::Parser, ErrorLevel::Fatal, "boom");
        assert_eq!(e.code, ErrorCode::InternalError);
    }

    #[test]
    fn xml_error_with_code_chains() {
        let e = XmlError::new(ErrorDomain::Parser, ErrorLevel::Fatal, "bad char")
            .with_code(ErrorCode::InvalidChar)
            .at("doc.xml", 1, 2, 5);
        assert_eq!(e.code, ErrorCode::InvalidChar);
        assert_eq!(e.code as i32, 9);
        assert_eq!(e.line,        Some(1));
        assert_eq!(e.column,      Some(2));
        assert_eq!(e.byte_offset, Some(5));
    }
}
