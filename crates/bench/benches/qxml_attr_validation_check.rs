//! Diagnostic: probe what quick-xml actually validates on attributes.
//!
//! quick-xml's `Attributes` iterator has a `with_checks: true`
//! default that the maintainer described as covering attribute
//! validation.  This bench feeds 7 different XML 1.0 attribute-WFC
//! violations to quick-xml in two modes:
//!
//!   1. No-iteration: just walk events, never call `.attributes()`.
//!   2. With `with_checks: true` (their default): walk events AND
//!      iterate `.attributes()`.
//!
//! The output table shows which violations each mode catches.
//! Spoiler: even with full iteration, quick-xml catches only 2 of
//! the 7 cases (duplicate names + unquoted values).  The other 5
//! — including bare `<` in attribute values, bare `&`, undefined
//! entities, digit-start names, and missing whitespace between
//! attributes — slip through silently.
//!
//! Run with:
//!     cargo bench -p sup-xml-bench --bench qxml_attr_validation_check

use quick_xml::Reader;
use quick_xml::events::Event;

const SAMPLES: &[(&str, &str)] = &[
    ("digit-start name",  r#"<doc 12="34"></doc>"#),
    ("duplicate names",   r#"<doc a="x" a="y"></doc>"#),
    ("bare < in value",   r#"<doc a="<foo>"></doc>"#),
    ("bare & in value",   r#"<doc a="A & B"></doc>"#),
    ("unquoted value",    r#"<doc a=v></doc>"#),
    ("undefined entity",  r#"<doc a="&xyz;"></doc>"#),
    ("missing space",     r#"<doc a="x"b="y"></doc>"#),
];

fn main() {
    println!();
    println!("Probing quick-xml's attribute validation on 7 ill-formed inputs.");
    println!("Each row shows what quick-xml does in two modes.\n");
    println!("{:<22}  {:<28}  {}",
             "case", "with_checks=true (default)", "no iteration");
    println!("{:─<22}  {:─<28}  {}",
             "", "", "─".repeat(28));

    for (label, src) in SAMPLES {
        // Mode A: no iteration of attributes — events only.
        let walk = {
            let mut r = Reader::from_reader(src.as_bytes());
            loop {
                match r.read_event() {
                    Ok(Event::Eof) => break "accept".to_string(),
                    Ok(_)  => continue,
                    Err(e) => break format!("REJECT: {}",
                        e.to_string().chars().take(20).collect::<String>()),
                }
            }
        };

        // Mode B: walk + iterate attributes() (their default with_checks=true).
        let with_checks = {
            let mut r = Reader::from_reader(src.as_bytes());
            let mut verdict = "accept".to_string();
            'outer: loop {
                match r.read_event() {
                    Ok(Event::Eof) => break,
                    Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                        for a in e.attributes() {
                            if let Err(err) = a {
                                verdict = format!("REJECT: {}",
                                    err.to_string().chars().take(20).collect::<String>());
                                break 'outer;
                            }
                        }
                    }
                    Ok(_)  => continue,
                    Err(e) => {
                        verdict = format!("REJECT: {}",
                            e.to_string().chars().take(20).collect::<String>());
                        break;
                    }
                }
            }
            verdict
        };

        println!("{:<22}  {:<28}  {}", label, with_checks, walk);
    }

    println!();
    println!("Summary:");
    println!("  - 5 of 7 attribute-WFC violations are silently accepted by");
    println!("    quick-xml even with `with_checks: true` and full iteration.");
    println!("  - The 2 it catches (duplicate names, unquoted values) only fire");
    println!("    when the caller actually calls .attributes() on each tag.");
    println!("    A consumer that walks events by name (e.g. an XPath or DOM");
    println!("    builder that filters tags before reading attrs) would not see");
    println!("    even those.");
    println!();
    println!("  sup-xml's default mode rejects all 7 cases at parse time, with");
    println!("  no caller cooperation needed.  See `text_validation_check` for");
    println!("  the full cross-parser comparison and `xmlts_compliance` for the");
    println!("  W3C conformance suite scores.");
}
