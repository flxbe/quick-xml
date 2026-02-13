// std::hint::black_box stable since 1.66, but our MSRV = 1.56.
// criterion::black_box is deprecated in since criterion 0.7.
// Running benchmarks assumed on current Rust version, so this should be fine
#![allow(clippy::incompatible_msrv)]
use criterion::{self, criterion_group, criterion_main, Criterion, Throughput};
use quick_xml::events::Event;
use quick_xml::{Reader, Result as XmlResult};
use std::hint::black_box;

static BOB: &str = include_str!("../tests/documents/bob.xml");

fn parse_from_reader(input: &str) -> XmlResult<()> {
    let mut r = Reader::from_str(input);
    loop {
        match black_box(r.read_event()?) {
            Event::Start(e) | Event::Empty(e) => {
                black_box(e.local_name());
                for attr in e.attributes() {
                    black_box(attr?.decode_and_unescape_value(r.decoder())?);
                }
            }
            Event::Text(e) => {
                black_box(e.xml_content()?);
            }
            Event::CData(e) => {
                black_box(e.into_inner());
            }
            Event::End(_) => (),
            Event::Eof => break,
            _ => (),
        }
    }
    Ok(())
}

fn parse_from_slice(input: &str) -> XmlResult<()> {
    let mut r = Reader::from_str(input);
    let mut buf = Vec::new();
    loop {
        match black_box(r.read_event_into(&mut buf)?) {
            Event::Start(e) | Event::Empty(e) => {
                black_box(e.local_name());
                for attr in e.attributes() {
                    black_box(attr?.decode_and_unescape_value(r.decoder())?);
                }
            }
            Event::Text(e) => {
                black_box(e.xml_content()?);
            }
            Event::CData(e) => {
                black_box(e.into_inner());
            }
            Event::End(_) => (),
            Event::Eof => break,
            _ => (),
        }
    }
    Ok(())
}

pub fn bench_parse_from_reader(c: &mut Criterion) {
    let mut group = c.benchmark_group("parse_from_reader");

    group.throughput(Throughput::Bytes(BOB.len() as u64));

    group.bench_with_input("BOB", BOB, |b, input| b.iter(|| parse_from_reader(input)));

    group.finish();
}

pub fn bench_parse_from_slice(c: &mut Criterion) {
    let mut group = c.benchmark_group("parse_from_slice");

    group.throughput(Throughput::Bytes(BOB.len() as u64));

    group.bench_with_input("BOB", BOB, |b, input| b.iter(|| parse_from_slice(input)));

    group.finish();
}

criterion_group!(benches, bench_parse_from_slice, bench_parse_from_reader);
criterion_main!(benches);
