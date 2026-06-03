//! Concurrency contract for the native Rust API.
//!
//! sup-xml's threading story is deliberately simple: a [`Document`] owns a
//! self-contained arena and is `Send` but not `Sync`. These tests pin that
//! contract down from two angles:
//!
//! * **Type level** — `Document: Send`, and the value/config types that
//!   cross a thread boundary (`ParseOptions`, `XmlError`) are `Send + Sync`.
//!   (`!Sync` for `Document` is locked in by a `compile_fail` doctest in the
//!   crate's "Thread safety" section.)
//! * **Runtime** — independent parses on many threads stay deterministic and
//!   don't corrupt the process-wide statics they share (the license
//!   `OnceLock`, the thread-local regex/PRNG scratch), documents move freely
//!   across thread boundaries in both directions, and an arena allocated on
//!   one thread is sound to drop on another.
//!
//! The runtime tests are probabilistic by nature — a data race may not fire
//! on every run. Their real teeth come from running the suite under a race
//! detector (`RUSTFLAGS="-Zsanitizer=thread" cargo +nightly test` or
//! `cargo +nightly miri test`); the high iteration/contention counts here
//! exist to give such a detector something to catch.
//!
//! The counts in the always-run tests are kept modest so they stay cheap
//! inside `cargo test-all`. The `#[ignore]`d variants at the bottom of this
//! file crank thread and iteration counts way up; `cargo stress-threads`
//! opts into them (in release mode), and `SUPSO_THREAD_STRESS=N` multiplies
//! their iteration count for an even longer soak.

use std::sync::{Arc, Barrier, mpsc};
use std::thread;

use sup_xml::{
    parse_str, serialize_to_string, xpath_count, Document, ParseOptions, XmlError,
};

/// A document that touches several subsystems at once — namespaces,
/// attributes, nested elements, mixed content, a comment, and a character
/// reference — so a race that corrupts any one of them shows up as a
/// mismatched count or serialization.
const SAMPLE: &str = r#"<?xml version="1.0"?>
<catalog xmlns:m="urn:meta" m:rev="3">
  <!-- inventory -->
  <book id="1"><title>Dune</title><price>9.99</price></book>
  <book id="2"><title>Foundation &amp; Empire</title><price>12.50</price></book>
  <book id="3"><title>Neuromancer</title><price>8.00</price></book>
</catalog>"#;

fn doc(xml: &str) -> Document {
    parse_str(xml, &ParseOptions::default()).expect("test document must parse")
}

// ── type-level contract ───────────────────────────────────────────────────────

fn assert_send<T: Send>() {}
fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn type_contract() {
    // A parsed document can be handed to another thread.
    assert_send::<Document>();
    // Inputs flow in and errors flow out across threads.
    assert_send_sync::<ParseOptions>();
    assert_send_sync::<XmlError>();
}

// ── independent parsing is deterministic under concurrency ─────────────────────

#[test]
fn parallel_parsing_matches_serial_golden() {
    // Compute the answer once, single-threaded, then demand every concurrent
    // re-parse reproduces it byte-for-byte.
    let golden = doc(SAMPLE);
    let golden_count = xpath_count(&golden, "//book").unwrap();
    let golden_xml = Arc::new(serialize_to_string(&golden));
    drop(golden);

    const THREADS: usize = 8;
    const ITERS: usize = 50;

    let start = Arc::new(Barrier::new(THREADS));
    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let start = Arc::clone(&start);
            let golden_xml = Arc::clone(&golden_xml);
            thread::spawn(move || {
                start.wait();
                for _ in 0..ITERS {
                    let d = doc(SAMPLE);
                    assert_eq!(xpath_count(&d, "//book").unwrap(), golden_count);
                    assert_eq!(&serialize_to_string(&d), &*golden_xml);
                }
            })
        })
        .collect();

    for h in handles {
        h.join().expect("worker thread panicked");
    }
}

#[test]
fn parallel_parsing_of_distinct_documents() {
    // Each thread parses a *different* document. If interning, encoding, or
    // any shared table leaked state between threads, a thread would see a
    // count that belongs to a sibling's document.
    const THREADS: usize = 16;

    let start = Arc::new(Barrier::new(THREADS));
    let handles: Vec<_> = (0..THREADS)
        .map(|i| {
            let start = Arc::clone(&start);
            thread::spawn(move || {
                // Element count varies with the thread index.
                let items: String = (0..=i).map(|_| "<item/>").collect();
                let xml = format!("<root id=\"{i}\">{items}</root>");
                start.wait();
                for _ in 0..50 {
                    let d = doc(&xml);
                    assert_eq!(xpath_count(&d, "//item").unwrap(), i + 1);
                    assert_eq!(xpath_count(&d, "/root[@id]").unwrap(), 1);
                }
            })
        })
        .collect();

    for h in handles {
        h.join().expect("worker thread panicked");
    }
}

// ── documents move across thread boundaries ────────────────────────────────────

#[test]
fn document_moves_into_worker_thread() {
    // Parse on this thread, then move the whole document into a worker and
    // use it there. Exercises `Document: Send` in the forward direction.
    let d = doc(SAMPLE);
    let count = thread::spawn(move || {
        let n = xpath_count(&d, "//book").unwrap();
        // Serialize on the worker too, to prove the arena is fully usable
        // from a thread other than the one that built it.
        assert!(serialize_to_string(&d).contains("Neuromancer"));
        n
    })
    .join()
    .unwrap();
    assert_eq!(count, 3);
}

#[test]
fn document_returned_from_worker_thread() {
    // The mirror image: a worker builds the document and ships it back over a
    // channel, and the receiving thread — which never touched the arena
    // during construction — reads from it.
    let (tx, rx) = mpsc::channel::<Document>();
    thread::spawn(move || {
        tx.send(doc(SAMPLE)).unwrap();
    });
    let d = rx.recv().unwrap();
    assert_eq!(xpath_count(&d, "//book").unwrap(), 3);
    assert!(serialize_to_string(&d).contains("Dune"));
}

#[test]
fn arenas_allocated_off_thread_drop_on_main() {
    // Several workers each parse a document and send it home; the main thread
    // collects them and drops them all. A document's arena (`Arc<Bump>`) is
    // thus freed on a thread other than the one that allocated it — the drop
    // must stay sound under that hand-off.
    const THREADS: usize = 12;
    let (tx, rx) = mpsc::channel::<Document>();
    for i in 0..THREADS {
        let tx = tx.clone();
        thread::spawn(move || {
            let xml = format!("<doc n=\"{i}\"><leaf/></doc>");
            tx.send(doc(&xml)).unwrap();
        });
    }
    drop(tx);

    let collected: Vec<Document> = rx.iter().collect();
    assert_eq!(collected.len(), THREADS);
    for d in &collected {
        assert_eq!(xpath_count(d, "//leaf").unwrap(), 1);
    }
    drop(collected); // all foreign arenas released here, on the main thread
}

// ── contention on process-wide lazy state ──────────────────────────────────────

#[test]
fn simultaneous_first_parse_under_contention() {
    // Parsing lazily initializes process-wide state (the license-verdict
    // `OnceLock`, thread-local scratch buffers). Release a wall of threads at
    // exactly the same instant so they all race that first-touch path; every
    // one must come back with a correctly-parsed document.
    const THREADS: usize = 32;

    let start = Arc::new(Barrier::new(THREADS));
    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let start = Arc::clone(&start);
            thread::spawn(move || {
                start.wait();
                let d = doc(SAMPLE);
                xpath_count(&d, "//title").unwrap()
            })
        })
        .collect();

    for h in handles {
        assert_eq!(h.join().expect("worker thread panicked"), 3);
    }
}

// ── heavy stress variants (run via `cargo stress-threads`) ─────────────────────

/// Iteration multiplier read from `SUPSO_THREAD_STRESS` (default 1). Lets a
/// soak run be lengthened without recompiling — `SUPSO_THREAD_STRESS=20
/// cargo stress-threads`.
fn stress_factor() -> usize {
    std::env::var("SUPSO_THREAD_STRESS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(1)
}

#[test]
#[ignore = "heavy soak; run via `cargo stress-threads`"]
fn stress_parallel_parsing_matches_golden() {
    let golden = doc(SAMPLE);
    let golden_count = xpath_count(&golden, "//book").unwrap();
    let golden_xml = Arc::new(serialize_to_string(&golden));
    drop(golden);

    const THREADS: usize = 32;
    let iters = 2_000 * stress_factor();

    let start = Arc::new(Barrier::new(THREADS));
    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let start = Arc::clone(&start);
            let golden_xml = Arc::clone(&golden_xml);
            thread::spawn(move || {
                start.wait();
                for _ in 0..iters {
                    let d = doc(SAMPLE);
                    assert_eq!(xpath_count(&d, "//book").unwrap(), golden_count);
                    assert_eq!(&serialize_to_string(&d), &*golden_xml);
                }
            })
        })
        .collect();

    for h in handles {
        h.join().expect("worker thread panicked");
    }
}

#[test]
#[ignore = "heavy soak; run via `cargo stress-threads`"]
fn stress_cross_thread_handoff() {
    // Repeatedly spin up a wave of workers, each building a document and
    // shipping it to the main thread to read and drop. Pounds the
    // `Send`-move and off-thread-arena-drop paths over and over.
    let rounds = 500 * stress_factor();
    const THREADS: usize = 16;

    for _ in 0..rounds {
        let (tx, rx) = mpsc::channel::<Document>();
        for i in 0..THREADS {
            let tx = tx.clone();
            thread::spawn(move || {
                let xml = format!("<doc n=\"{i}\"><leaf/></doc>");
                tx.send(doc(&xml)).unwrap();
            });
        }
        drop(tx);
        let collected: Vec<Document> = rx.iter().collect();
        assert_eq!(collected.len(), THREADS);
        for d in &collected {
            assert_eq!(xpath_count(d, "//leaf").unwrap(), 1);
        }
    }
}
