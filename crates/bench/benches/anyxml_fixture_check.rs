//! Diagnostic: probe whether anyxml accepts each of our real-world
//! XML fixtures, and if it rejects, which error it returns.  Used to
//! sanity-check the head-to-head perf bench — suspiciously high MB/s
//! ratios for anyxml on some fixtures could mean anyxml is failing
//! fast and inflating the throughput number, rather than genuinely
//! parsing faster.
//!
//! Run with:
//!     cargo bench -p sup-xml-bench --bench anyxml_fixture_check

use anyxml::sax::{
    Attributes as AnyAttrs, EntityResolver as AnyEntityResolver,
    ErrorHandler as AnyErrorHandler, SAXHandler as AnySAXHandler,
    XMLReader as AnyXMLReader,
};
use anyxml::sax::error::SAXParseError;

#[derive(Default)]
struct Probe {
    n_starts:     u32,
    error_count:  u32,
    first_fatal:  Option<String>,
    first_error:  Option<String>,
}

impl AnyEntityResolver for Probe {}
impl AnyErrorHandler for Probe {
    fn fatal_error(&mut self, e: SAXParseError) {
        self.error_count += 1;
        if self.first_fatal.is_none() {
            self.first_fatal = Some(format!("{:?}", e));
        }
    }
    fn error(&mut self, e: SAXParseError) {
        self.error_count += 1;
        if self.first_error.is_none() {
            self.first_error = Some(format!("{:?}", e));
        }
    }
}
impl AnySAXHandler for Probe {
    fn start_element(&mut self, _: Option<&str>, _: Option<&str>, _: &str, _: &AnyAttrs) {
        self.n_starts += 1;
    }
}

const FIXTURES: &[&str] = &[
    "321gone.xml", "1831893.xml", "bargains_he_5.xml", "chinese1.xml",
    "cldr_en.xml", "customer1.xml", "dblp.xml", "ebay.xml",
    "gazali_maqasid_ar.xml", "maven-pom.xml", "nasa.xml", "osm.xml",
    "podcast_episode_2024_03.xml", "pubmed.xml", "sitemap.xml",
    "swiss_prot.xml", "transitions_tutorial.xml", "ubid.xml",
    "utah_legislature_2024.xml", "uwm.xml", "wikipedia_ww2.xml", "yahoo.xml",
];

fn main() {
    println!("\nProbing each real-world fixture through anyxml.\n");
    println!("{:<32}  {:>10}  {:>8}  {:>8}  verdict / error",
             "fixture", "size", "starts", "errs");
    println!("{:-<32}  {:->10}  {:->8}  {:->8}  {:-<60}", "", "", "", "", "");

    for name in FIXTURES {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/assets/xml")
            .join(name);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => { println!("{:<32}  {:>10}  {:>8}  {:>8}  read error: {}", name, "-", "-", "-", e); continue; }
        };
        let s = match std::str::from_utf8(&bytes) {
            Ok(s) => s,
            Err(_) => { println!("{:<32}  {:>10}  {:>8}  {:>8}  not UTF-8 (skipped)", name, bytes.len(), "-", "-"); continue; }
        };

        let mut reader = AnyXMLReader::builder()
            .set_handler(Probe::default())
            .build();
        let res = reader.parse_str(s, None);
        let h = &reader.handler;

        let verdict = match (&res, h.error_count) {
            (Ok(_),  0) => "OK".to_string(),
            (Ok(_),  _) => format!("OK-with-warnings ({} errs)", h.error_count),
            (Err(_), _) => {
                let msg = h.first_fatal.as_deref()
                    .or(h.first_error.as_deref())
                    .map(|s| s.chars().take(80).collect::<String>())
                    .unwrap_or_else(|| format!("Err: {:?}", res.as_ref().err().unwrap()));
                format!("REJECT — {}", msg)
            }
        };

        println!("{:<32}  {:>10}  {:>8}  {:>8}  {}",
                 name, bytes.len(), h.n_starts, h.error_count, verdict);
    }
}
