//! Quick diagnostic: count what each parser actually emits as events
//! on the same input.  Settles "does quick-xml emit whitespace text
//! events by default?" with data instead of assertions.

use quick_xml::events::Event;
use quick_xml::Reader as QxReader;
use sup_xml::{BytesEvent, ParseOptions, XmlBytesReader};

fn main() {
    let inputs: &[(&str, &str)] = &[
        ("compact",
            "<r><a/><b/></r>"),
        ("indented",
            "<r>\n  <a/>\n  <b/>\n</r>"),
        ("mixed-content",
            "<p>foo <b>bar</b> baz</p>"),
        ("real fixture (1831893)",
            // Just the first ~500 bytes to keep output readable.
            include_str!("../../../tests/assets/xml/1831893.xml")),
    ];

    for (label, src) in inputs {
        println!("\n── {label} ── ({} bytes)", src.len());
        println!("  sup-xml (default):       {:?}", count_super(src.as_bytes(), false));
        println!("  sup-xml (skip_end_tag):  {:?}", count_super(src.as_bytes(), true));
        println!("  quick-xml (default):      {:?}", count_qxml(src.as_bytes(), false));
        println!("  quick-xml (trim_text):    {:?}", count_qxml(src.as_bytes(), true));
    }
}

#[derive(Default, Debug)]
struct C { start: u32, end: u32, text: u32, ws_text: u32, empty_text: u32, other: u32 }

fn count_super(src: &[u8], skip_end_tag: bool) -> C {
    let opts = if skip_end_tag {
        ParseOptions { skip_end_tag_check: true, ..ParseOptions::default() }
    } else { ParseOptions::default() };
    let mut r = unsafe { XmlBytesReader::from_bytes_unchecked(src) }.with_options(opts);
    let mut c = C::default();
    loop {
        match r.next().unwrap() {
            BytesEvent::StartElement(_) => c.start += 1,
            BytesEvent::EndElement(_)   => c.end   += 1,
            BytesEvent::Text(t) => {
                let b = t.as_bytes();
                c.text += 1;
                if b.is_empty() { c.empty_text += 1; }
                else if b.iter().all(|x| x.is_ascii_whitespace()) { c.ws_text += 1; }
            }
            BytesEvent::Eof => break,
            _ => c.other += 1,
        }
    }
    c
}

fn count_qxml(src: &[u8], trim: bool) -> C {
    let mut r = QxReader::from_reader(src);
    if trim { r.config_mut().trim_text(true); }
    let mut buf = Vec::new();
    let mut c = C::default();
    loop {
        match r.read_event_into(&mut buf).unwrap() {
            Event::Start(_)        => c.start += 1,
            Event::End(_)          => c.end   += 1,
            Event::Empty(_)        => { c.start += 1; c.end += 1; }
            Event::Text(t) => {
                let b: &[u8] = t.as_ref();
                c.text += 1;
                if b.is_empty() { c.empty_text += 1; }
                else if b.iter().all(|x| x.is_ascii_whitespace()) { c.ws_text += 1; }
            }
            Event::Eof             => break,
            _ => c.other += 1,
        }
        buf.clear();
    }
    c
}
