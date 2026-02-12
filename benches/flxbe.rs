// std::hint::black_box stable since 1.66, but our MSRV = 1.56.
// criterion::black_box is deprecated in since criterion 0.7.
// Running benchmarks assumed on current Rust version, so this should be fine
#![allow(clippy::incompatible_msrv)]
use criterion::{self, criterion_group, criterion_main, Criterion, Throughput};
use quick_xml::events::Event;
use quick_xml::reader::NsReader;
use quick_xml::Result as XmlResult;
use std::fs;
use std::hint::black_box;

static BOB_PATH: &str = "./tests/documents/bob.xml";

// TODO: use fully normalized attribute values
fn parse_document_from_bytes_with_namespaces(file_path: &str) -> XmlResult<()> {
    let mut r = NsReader::from_file(file_path)?;
    let mut buf = Vec::new();
    loop {
        match black_box(r.read_resolved_event_into(&mut buf)?) {
            (resolved_ns, Event::Start(e) | Event::Empty(e)) => {
                black_box(resolved_ns);
                for attr in e.attributes() {
                    black_box(attr?.decode_and_unescape_value(r.decoder())?);
                }
            }
            (resolved_ns, Event::Text(e)) => {
                black_box(e.xml_content()?);
                black_box(resolved_ns);
            }
            (resolved_ns, Event::CData(e)) => {
                black_box(e.into_inner());
                black_box(resolved_ns);
            }
            (_, Event::End(_)) => (),
            (_, Event::Eof) => break,
            _ => (),
        }
        // buf.clear();
    }
    Ok(())
}

/// Decode into a buffer, then parse - including namespaces
pub fn bench_decode_and_parse_document_with_namespaces(c: &mut Criterion) {
    let mut group = c.benchmark_group("parse_bob_from_file");

    let metadata = fs::metadata(BOB_PATH).unwrap();
    group.throughput(Throughput::Bytes(metadata.len()));

    group.bench_with_input("BOB", BOB_PATH, |b, input| {
        b.iter(|| parse_document_from_bytes_with_namespaces(input))
    });

    group.finish();
}

criterion_group!(benches, bench_decode_and_parse_document_with_namespaces,);
criterion_main!(benches);
