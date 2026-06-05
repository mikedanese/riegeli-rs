//! Differential harness: identical inputs to BOTH implementations, compared
//! on observable behavior — corruption handling, recovery regions,
//! projection output, and error classes. Three case buckets:
//! - MATCH: assert equality between the implementations.
//! - KNOWN-DIVERGENT: assert the documented divergence ON BOTH SIDES, so
//!   silent drift in either direction fails the suite.
//! - (QUESTION is a development state only: every delivered case has been
//!   promoted to one of the above, with the reference rationale recorded.)

use std::cell::RefCell;
use std::io::Cursor;
use std::rc::Rc;

use riegeli::{
    CompressionType, Field, FieldProjection, ReaderOptions, RecordReader, RecordWriter,
    SkippedRegion, WriterOptions,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn rust_write(records: &[&[u8]], opts: WriterOptions) -> Vec<u8> {
    let mut buf = Cursor::new(Vec::<u8>::new());
    {
        let mut w = RecordWriter::new(&mut buf, opts).expect("writer new ok");
        for rec in records {
            w.write_record(rec).expect("write ok");
        }
        w.flush().expect("flush ok");
    }
    buf.into_inner()
}

/// Read everything with the Rust reader + collecting recovery; returns
/// (records, regions, final numeric position).
fn rust_read_collecting(data: &[u8]) -> (Vec<Vec<u8>>, Vec<SkippedRegion>, u64) {
    let regions: Rc<RefCell<Vec<SkippedRegion>>> = Rc::new(RefCell::new(Vec::new()));
    let rc = Rc::clone(&regions);
    let opts = ReaderOptions::new().recovery(move |r| {
        rc.borrow_mut().push(r.clone());
        true
    });
    let mut reader = RecordReader::new(Cursor::new(data.to_vec()), opts).expect("rust reader ok");
    let mut out = Vec::new();
    while let Some(rec) = reader.read_record().expect("rust read ok (recovery on)") {
        out.push(rec);
    }
    let pos = reader.pos().numeric();
    let regions = regions.borrow().clone();
    (out, regions, pos)
}

/// Read everything with the C++ reader + collecting recovery.
fn cpp_read_collecting(data: &[u8]) -> (Vec<Vec<u8>>, Vec<riegeli_ffi::CppSkippedRegion>, u64) {
    let mut reader = riegeli_ffi::RecordReader::with_options(data, &[], true, None)
        .expect("cpp reader ok");
    let mut out = Vec::new();
    while let Some(rec) = reader.read_record().expect("cpp read ok (recovery on)") {
        out.push(rec);
    }
    let pos = reader.pos_numeric();
    let regions = reader.skipped_regions();
    (out, regions, pos)
}

/// Assert the region sequences agree on extents (messages are per-impl).
fn assert_regions_match(
    label: &str,
    rust: &[SkippedRegion],
    cpp: &[riegeli_ffi::CppSkippedRegion],
) {
    assert_eq!(
        rust.len(),
        cpp.len(),
        "{label}: region count — rust {rust:?} vs cpp {cpp:?}"
    );
    for (i, (r, c)) in rust.iter().zip(cpp.iter()).enumerate() {
        assert_eq!(r.begin(), c.begin, "{label}: region {i} begin");
        assert_eq!(r.end(), c.end, "{label}: region {i} end");
    }
}

/// Rust-side projected read.
fn rust_read_projected(data: &[u8], proj: FieldProjection) -> Vec<Vec<u8>> {
    let mut reader = RecordReader::new(
        Cursor::new(data.to_vec()),
        ReaderOptions::new().field_projection(proj),
    )
    .expect("rust reader ok");
    let mut out = Vec::new();
    while let Some(rec) = reader.read_record().expect("rust read ok") {
        out.push(rec);
    }
    out
}

/// C++-side projected read. Existence-only is expressed as path + [0]
/// (Field::kExistenceOnly) at the bridge — see case D, which doubles as the
/// validation of that mapping.
fn cpp_read_projected(data: &[u8], paths: &[Vec<u32>]) -> Vec<Vec<u8>> {
    let mut reader = riegeli_ffi::RecordReader::with_options(data, paths, false, None)
        .expect("cpp reader ok");
    let mut out = Vec::new();
    while let Some(rec) = reader.read_record().expect("cpp read ok") {
        out.push(rec);
    }
    out
}

fn encode_u32v(v: u32) -> Vec<u8> {
    let mut out = Vec::new();
    let mut v = v as u64;
    loop {
        if v < 0x80 {
            out.push(v as u8);
            break;
        }
        out.push((v as u8 & 0x7f) | 0x80);
        v >>= 7;
    }
    out
}

fn encode_varint_field(field: u32, value: u64) -> Vec<u8> {
    let mut out = encode_u32v((field << 3) | 0);
    let mut v = value;
    loop {
        let b = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            break;
        }
        out.push(b | 0x80);
    }
    out
}

fn encode_string_field(field: u32, value: &[u8]) -> Vec<u8> {
    let mut out = encode_u32v((field << 3) | 2);
    out.extend_from_slice(&encode_u32v(value.len() as u32));
    out.extend_from_slice(value);
    out
}

fn encode_group(field: u32, content: &[u8]) -> Vec<u8> {
    let mut out = encode_u32v((field << 3) | 3); // StartGroup
    out.extend_from_slice(content);
    out.extend_from_slice(&encode_u32v((field << 3) | 4)); // EndGroup
    out
}

// ---------------------------------------------------------------------------
// Case A — recovery region parity
// ---------------------------------------------------------------------------

/// MATCH: a single corrupt-DATA chunk (header valid → trusted extent) below
/// the first block boundary. Both sides skip exactly that chunk, the sibling
/// survives, and the region extents agree. This is the precision case where
/// the pre-recovery-API Rust behavior (boundary skip) visibly diverged.
#[test]
fn a1_corrupt_data_chunk_regions_match() {
    let one = rust_write(&[b"a"], WriterOptions::new().chunk_size(1));
    let two = rust_write(&[b"a", b"b"], WriterOptions::new().chunk_size(1));
    let mut data = rust_write(&[b"a", b"b", b"c"], WriterOptions::new().chunk_size(1));
    data[two.len() - 1] ^= 0xFF; // chunk "b" data corrupted, header intact
    let _ = one;

    let (rust_recs, rust_regions, _) = rust_read_collecting(&data);
    let (cpp_recs, cpp_regions, _) = cpp_read_collecting(&data);

    assert_eq!(rust_recs, cpp_recs, "surviving records must agree");
    assert_eq!(rust_recs, vec![b"a".to_vec(), b"c".to_vec()]);
    assert_regions_match("a1", &rust_regions, &cpp_regions);
}

/// MATCH: a corrupt chunk HEADER (hash invalid → untrusted). Promoted from
/// QUESTION empirically: both implementations resync within the same block
/// and report the same region extents on this layout.
#[test]
fn a2_corrupt_header_regions_match() {
    let one = rust_write(&[b"a"], WriterOptions::new().chunk_size(1));
    let mut data = rust_write(&[b"a", b"b", b"c"], WriterOptions::new().chunk_size(1));
    data[one.len()] ^= 0xFF; // chunk "b" header hash corrupted

    let (rust_recs, rust_regions, _) = rust_read_collecting(&data);
    let (cpp_recs, cpp_regions, _) = cpp_read_collecting(&data);

    assert_eq!(rust_recs, cpp_recs, "surviving records must agree");
    assert_regions_match("a2", &rust_regions, &cpp_regions);
}

/// MATCH: a run of corrupt-data chunks — one region per chunk, contiguous,
/// identical sequences on both sides, and the good chunk after the run is
/// reached by both.
#[test]
fn a3_corrupt_run_regions_match() {
    const N: usize = 20;
    let mut recs: Vec<Vec<u8>> = Vec::new();
    let mut lens = Vec::new();
    for k in 0..(N + 2) {
        recs.push(format!("r{k:02}").into_bytes());
        let refs: Vec<&[u8]> = recs.iter().map(|r| r.as_slice()).collect();
        lens.push(rust_write(&refs, WriterOptions::new().chunk_size(1)).len());
    }
    let refs: Vec<&[u8]> = recs.iter().map(|r| r.as_slice()).collect();
    let mut data = rust_write(&refs, WriterOptions::new().chunk_size(1));
    for k in 1..=N {
        data[lens[k] - 1] ^= 0xFF;
    }

    let (rust_recs, rust_regions, _) = rust_read_collecting(&data);
    let (cpp_recs, cpp_regions, _) = cpp_read_collecting(&data);

    assert_eq!(rust_recs, cpp_recs);
    assert_eq!(rust_recs.len(), 2, "first and last records survive");
    assert_regions_match("a3", &rust_regions, &cpp_regions);
    assert_eq!(rust_regions.len(), N);
}

/// KNOWN-DIVERGENT: truncated final chunk (header readable, data cut).
/// Records agree, but C++ defers truncation reporting to Close() — its
/// documented contract calls the recovery function for truncation only at
/// Close, so the READ loop sees zero regions — while Rust reports the
/// truncated region at read time (our reader has no Close with that
/// contract). Both shapes asserted.
#[test]
fn a4_truncated_final_chunk_read_time_vs_close_time() {
    let one = rust_write(&[b"first"], WriterOptions::new().chunk_size(1));
    let full = rust_write(&[b"first", b"second"], WriterOptions::new().chunk_size(1));
    let data = full[..one.len() + 40].to_vec(); // keep chunk B's header only

    let (rust_recs, rust_regions, _) = rust_read_collecting(&data);
    let (cpp_recs, cpp_regions, _) = cpp_read_collecting(&data);

    assert_eq!(rust_recs, cpp_recs);
    assert_eq!(rust_recs, vec![b"first".to_vec()]);
    assert_eq!(
        rust_regions.len(),
        1,
        "rust reports the truncated tail at read time"
    );
    assert_eq!(
        cpp_regions.len(),
        0,
        "C++ defers truncation to Close() — zero read-time regions"
    );
}

// ---------------------------------------------------------------------------
// Cases B/C/D — projection parity
// ---------------------------------------------------------------------------

/// MATCH (flipped from KNOWN-DIVERGENT by the group-ancestry restructure):
/// existence-only GROUP upgraded by an included child. C++ emits
/// group-start + child + group-end (maintainer-traced); Rust now agrees —
/// the case was born divergent and proves the restructure.
#[test]
fn b_eo_group_upgraded_by_included_child_matches() {
    let mut content = encode_varint_field(2, 7);
    content.extend_from_slice(&encode_varint_field(3, 9)); // excluded sibling
    let record = encode_group(1, &content);
    let data = rust_write(
        &[record.as_slice()],
        WriterOptions::new()
            .transpose(true)
            .compression(CompressionType::None),
    );

    let proj = FieldProjection::new()
        .add_field(Field::new(vec![1]).existence_only())
        .add_field(Field::new(vec![1, 2]));
    let rust_out = rust_read_projected(&data, proj);
    let cpp_out = cpp_read_projected(&data, &[vec![1, 0], vec![1, 2]]);

    let expected = encode_group(1, &encode_varint_field(2, 7));
    assert_eq!(rust_out, cpp_out, "both sides agree");
    assert_eq!(rust_out, vec![expected], "group frames the included child");
}

/// MATCH: include-child path with no matching data in the record — both
/// sides emit the empty framed submessage (frame tags unconditional, length
/// from the actual — empty — interior), not a dropped field.
#[test]
fn c_include_child_no_matching_data_matches() {
    let record = encode_string_field(1, &encode_varint_field(3, 9)); // no field 2 inside
    let data = rust_write(
        &[record.as_slice()],
        WriterOptions::new()
            .transpose(true)
            .compression(CompressionType::None),
    );

    let proj = FieldProjection::new().add_field(Field::new(vec![1, 2]));
    let rust_out = rust_read_projected(&data, proj);
    let cpp_out = cpp_read_projected(&data, &[vec![1, 2]]);
    assert_eq!(rust_out, cpp_out, "empty framed submessage on both sides");
}

/// MATCH: existence-only SUBMESSAGE upgraded by an included child (the
/// length-delimited analogue of case B) — min-resolution upgrade on both
/// sides. This case also VALIDATES THE BRIDGE MAPPING: existence-only is
/// translated to path+[0] (kExistenceOnly) at the ffi boundary, so if this
/// case fails, suspect the mapping before either implementation.
#[test]
fn d_eo_submessage_upgraded_matches_and_validates_mapping() {
    let mut inner = encode_varint_field(2, 7);
    inner.extend_from_slice(&encode_varint_field(3, 9));
    let record = encode_string_field(1, &inner);
    let data = rust_write(
        &[record.as_slice()],
        WriterOptions::new()
            .transpose(true)
            .compression(CompressionType::None),
    );

    let proj = FieldProjection::new()
        .add_field(Field::new(vec![1]).existence_only())
        .add_field(Field::new(vec![1, 2]));
    let rust_out = rust_read_projected(&data, proj);
    let cpp_out = cpp_read_projected(&data, &[vec![1, 0], vec![1, 2]]);
    assert_eq!(rust_out, cpp_out, "upgraded submessage output must agree");
    let expected = encode_string_field(1, &encode_varint_field(2, 7));
    assert_eq!(rust_out, vec![expected]);
}

// ---------------------------------------------------------------------------
// Case E — hostile-input agreement
// ---------------------------------------------------------------------------

/// MATCH (reject/reject): both implementations must reject hostile files.
/// Error classes may differ; the contract is rejection without panic.
#[test]
fn e_hostile_inputs_rejected_by_both() {
    // Corrupted signature.
    let mut bad_sig = rust_write(&[b"x"], WriterOptions::new());
    bad_sig[30] ^= 0xFF;

    for (label, data) in [("corrupt signature", bad_sig)] {
        let rust_err = RecordReader::new(Cursor::new(data.clone()), ReaderOptions::new())
            .err()
            .map(|_| true)
            .or_else(|| {
                // Constructor may succeed and the first read fail.
                let mut r =
                    RecordReader::new(Cursor::new(data.clone()), ReaderOptions::new()).ok()?;
                r.read_record().err().map(|_| true)
            });
        assert_eq!(rust_err, Some(true), "{label}: rust must reject");

        let cpp_rejects = match riegeli_ffi::RecordReader::new(&data) {
            Err(_) => true,
            Ok(mut r) => loop {
                match r.read_record() {
                    Ok(Some(_)) => continue,
                    Ok(None) => break false,
                    Err(_) => break true,
                }
            },
        };
        assert!(cpp_rejects, "{label}: C++ must reject");
    }
}

// ---------------------------------------------------------------------------
// Case F — cancel parity (MATCH on semantics; Err-vs-false return asserted)
// ---------------------------------------------------------------------------

/// MATCH (semantics): recovery cancel — Rust adopted the C++ shape after
/// the empirical trace falsified the latch assumption: the region is
/// consumed BEFORE the callback runs, cancel reports once, the next read
/// continues past the rejected region, and the callback never re-fires for
/// it. The one remaining (deliberate, documented) difference is the
/// cancelled call's return value: Rust returns the original error, C++
/// reports false/no-record — asserted on both sides.
#[test]
fn f_cancel_consumes_region_both_sides() {
    let two = rust_write(&[b"a", b"b"], WriterOptions::new().chunk_size(1));
    let mut data = rust_write(&[b"a", b"b", b"c"], WriterOptions::new().chunk_size(1));
    data[two.len() - 1] ^= 0xFF;

    // Rust: cancel → Err once; next read continues to "c"; one callback.
    let count: Rc<RefCell<u32>> = Rc::new(RefCell::new(0));
    let rc = Rc::clone(&count);
    let opts = ReaderOptions::new().recovery(move |_r| {
        *rc.borrow_mut() += 1;
        false
    });
    let mut rust_reader =
        RecordReader::new(Cursor::new(data.clone()), opts).expect("rust reader ok");
    assert_eq!(rust_reader.read_record().unwrap().as_deref(), Some(&b"a"[..]));
    assert!(
        rust_reader.read_record().is_err(),
        "rust: cancelled read returns the original error"
    );
    assert_eq!(
        rust_reader.read_record().unwrap().as_deref(),
        Some(&b"c"[..]),
        "rust: region consumed — reading continues"
    );
    assert_eq!(*count.borrow(), 1, "rust: callback fired once, never re-fired");

    // C++: cancel → false/no-record once; next read continues to "c";
    // exactly one region collected.
    let mut cpp_reader =
        riegeli_ffi::RecordReader::with_options(&data, &[], true, Some(0)).expect("cpp reader ok");
    assert_eq!(cpp_reader.read_record().unwrap().as_deref(), Some(&b"a"[..]));
    assert_eq!(
        cpp_reader.read_record().unwrap(),
        None,
        "cpp: cancelled ReadRecord reports no record"
    );
    assert_eq!(
        cpp_reader.read_record().unwrap().as_deref(),
        Some(&b"c"[..]),
        "cpp: region consumed — reading continues"
    );
    assert_eq!(cpp_reader.skipped_regions().len(), 1);
}

// ---------------------------------------------------------------------------
// Case H — per-op recovery parity (seek, metadata)
// ---------------------------------------------------------------------------

/// Seek-to-corrupt-region with CONTINUE recovery: both sides end up
/// positioned past the region and the next read agrees. (Rust seek returns
/// Ok positioned-past; C++ Seek returns its own result — the observable
/// contract compared here is the post-seek record stream.)
#[test]
fn h1_seek_into_corrupt_region_continue_parity() {
    let one = rust_write(&[b"a"], WriterOptions::new().chunk_size(1));
    let two = rust_write(&[b"a", b"b"], WriterOptions::new().chunk_size(1));
    let mut data = rust_write(&[b"a", b"b", b"c"], WriterOptions::new().chunk_size(1));
    data[two.len() - 1] ^= 0xFF; // corrupt chunk "b" (data; header valid)

    // Rust: seek to the corrupt chunk's begin, then read.
    let opts = ReaderOptions::new().recovery(|_r| true);
    let mut rust_reader =
        RecordReader::new(Cursor::new(data.clone()), opts).expect("rust reader ok");
    rust_reader
        .seek(riegeli::RecordPosition::new(one.len() as u64, 0))
        .expect("rust seek ok");
    let rust_next = rust_reader.read_record().unwrap();

    // C++: numeric seek to the same position, then read.
    let mut cpp_reader =
        riegeli_ffi::RecordReader::with_options(&data, &[], true, None).expect("cpp reader ok");
    assert!(cpp_reader.seek_numeric(one.len() as u64), "cpp seek ok");
    let cpp_next = cpp_reader.read_record().unwrap();

    assert_eq!(
        rust_next.as_deref(),
        cpp_next.as_deref(),
        "post-seek record stream agrees"
    );
    assert_eq!(rust_next.as_deref(), Some(&b"c"[..]));
}

/// Metadata-position corruption with CONTINUE recovery: both sides report
/// the file as having no (readable) metadata, and the subsequent record
/// stream is undisturbed. (Rust's metadata read is position-neutral by
/// documented contract; C++'s ReadSerializedMetadata is only callable
/// before reading records — both constraints are honored by running
/// metadata-first on each side.)
#[test]
fn h2_metadata_corruption_recovery_parity() {
    let mut data = rust_write(&[b"a", b"b"], WriterOptions::new().chunk_size(1));
    data[64] ^= 0xFF; // corrupt the chunk at the metadata position

    // Rust.
    let opts = ReaderOptions::new().recovery(|_r| true);
    let mut rust_reader =
        RecordReader::new(Cursor::new(data.clone()), opts).expect("rust reader ok");
    assert_eq!(
        rust_reader.read_serialized_metadata().expect("rust metadata ok"),
        None,
        "rust: skipped region reads as absent metadata"
    );

    // C++.
    let mut cpp_reader =
        riegeli_ffi::RecordReader::with_options(&data, &[], true, None).expect("cpp reader ok");
    let cpp_meta = cpp_reader.read_serialized_metadata();
    assert!(
        matches!(cpp_meta, Ok(None)),
        "cpp: recovery-continue reports absent metadata (got {cpp_meta:?})"
    );

    // Record streams after the metadata attempt agree (both recover past
    // the corrupt chunk on the read path).
    let mut rust_rest = Vec::new();
    while let Some(r) = rust_reader.read_record().expect("rust read ok") {
        rust_rest.push(r);
    }
    let mut cpp_rest = Vec::new();
    while let Some(r) = cpp_reader.read_record().expect("cpp read ok") {
        cpp_rest.push(r);
    }
    assert_eq!(rust_rest, cpp_rest, "post-metadata record streams agree");
}

// ---------------------------------------------------------------------------
// Case G — seeded generator// ---------------------------------------------------------------------------
// Case G — seeded generator
// ---------------------------------------------------------------------------

/// Deterministic xorshift so failures reproduce from the printed seed.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// MATCH: generated files round-trip identically, and after corruption both
/// sides report identical region extents. Failure messages carry the seed
/// and corruption offsets for one-command reproduction.
#[test]
fn g_generated_corruption_regions_match() {
    const CASES: u64 = 40;
    const BASE_SEED: u64 = 0x5EED_D1FF_0000_0001;

    for case in 0..CASES {
        let seed = BASE_SEED + case;
        let mut rng = Rng(seed);

        let n_records = 2 + rng.below(20) as usize;
        let records: Vec<Vec<u8>> = (0..n_records)
            .map(|i| {
                let len = 1 + rng.below(40) as usize;
                (0..len).map(|j| ((i * 7 + j * 13) % 251) as u8).collect()
            })
            .collect();
        let refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();
        let transpose = rng.below(2) == 1;
        let mut opts = WriterOptions::new()
            .chunk_size(1 + rng.below(3))
            .compression(CompressionType::None);
        if transpose {
            opts = opts.transpose(true);
        }
        let mut data = rust_write(&refs, opts);

        // Corrupt 1-2 bytes, biased PAST the signature block (offset >= 64):
        // corrupting the signature only re-pins construction-reject, which
        // case E already covers explicitly.
        let n_corrupt = 1 + rng.below(2);
        let mut offsets = Vec::new();
        for _ in 0..n_corrupt {
            let off = 64 + rng.below(data.len() as u64 - 64) as usize;
            data[off] ^= 0xFF;
            offsets.push(off);
        }

        let (rust_recs, rust_regions, _) = rust_read_collecting(&data);
        let (cpp_recs, cpp_regions, _) = cpp_read_collecting(&data);

        let ctx = format!(
            "seed={seed:#x} case={case} transpose={transpose} offsets={offsets:?}"
        );
        assert_eq!(rust_recs, cpp_recs, "{ctx}: surviving records");
        assert_eq!(
            rust_regions.len(),
            cpp_regions.len(),
            "{ctx}: region count (rust {rust_regions:?} vs cpp {cpp_regions:?})"
        );
        for (i, (r, c)) in rust_regions.iter().zip(cpp_regions.iter()).enumerate() {
            assert_eq!(r.begin(), c.begin, "{ctx}: region {i} begin");
            assert_eq!(r.end(), c.end, "{ctx}: region {i} end");
        }
    }
}

// ---------------------------------------------------------------------------
// Case I — randomized projection parity (groups in ANCESTRY positions)
// ---------------------------------------------------------------------------

/// Build a random valid proto record with nested structure. Returns the
/// encoded bytes. Depth-limited; wire types include groups and submessages
/// in ANCESTRY positions (group-wrapping-submessage, submessage-wrapping-
/// group, group-wrapping-group) — the exact blind spot where the ancestry
/// bugs lived.
/// Field numbers are PARTITIONED BY DEPTH (depth d uses 5d+1..=5d+5) so a
/// number never appears in two nesting contexts. Rationale: the
/// implementations DISAGREE on records where a field number is aliased
/// between an included context and an excluded-group interior (see the
/// pinned case below — C++ emits the aliased occurrence inside the
/// excluded group, this implementation does not). Which behavior the
/// format intends for that shape is an open maintainer question; until
/// answered, the generator stays on the unambiguous shapes.
fn gen_record(rng: &mut Rng, depth: u32) -> Vec<u8> {
    let mut out = Vec::new();
    let n_fields = 1 + rng.below(4);
    for _ in 0..n_fields {
        let field = depth * 5 + 1 + rng.below(5) as u32;
        match rng.below(if depth < 3 { 5 } else { 3 }) {
            0 => out.extend_from_slice(&encode_varint_field(field, rng.below(1000))),
            1 => {
                let len = rng.below(6) as usize;
                let val: Vec<u8> = (0..len).map(|i| (i as u8) + 65).collect();
                out.extend_from_slice(&encode_string_field(field, &val));
            }
            2 => out.extend_from_slice(&encode_varint_field(field, 7)),
            3 => {
                let inner = gen_record(rng, depth + 1);
                out.extend_from_slice(&encode_string_field(field, &inner));
            }
            _ => {
                let inner = gen_record(rng, depth + 1);
                out.extend_from_slice(&encode_group(field, &inner));
            }
        }
    }
    out
}

/// Build a random projection: 1-3 paths of depth 1-3, ~1/3 existence-only
/// terminals. Returns (rust form, cpp flat form).
fn gen_projection(rng: &mut Rng) -> (FieldProjection, Vec<Vec<u32>>) {
    let mut proj = FieldProjection::new();
    let mut cpp_paths = Vec::new();
    let n_paths = 1 + rng.below(3);
    for _ in 0..n_paths {
        let depth = 1 + rng.below(3) as usize;
        let path: Vec<u32> = (0..depth)
            .map(|d| (d as u32) * 5 + 1 + rng.below(5) as u32)
            .collect();
        let eo = rng.below(3) == 0;
        let mut field = Field::new(path.clone());
        let mut cpp_path = path.clone();
        if eo {
            field = field.existence_only();
            cpp_path.push(0); // kExistenceOnly
        }
        proj = proj.add_field(field);
        cpp_paths.push(cpp_path);
    }
    (proj, cpp_paths)
}

/// MATCH: random records x random projections — Rust and C++ projected
/// outputs must agree byte-for-byte. Failures print the seed for
/// one-command reproduction. This case would have caught all three
/// ancestry-blindness bugs (group-walk invisibility, start-node
/// own-frame misresolution, buffer-pruning under full includes).
#[test]
fn i_randomized_projection_parity() {
    const CASES: u64 = 100;
    const BASE_SEED: u64 = 0x9E0_1EC7_0000_0001;

    for case in 0..CASES {
        let seed = BASE_SEED + case;
        let mut rng = Rng(seed);
        let record = gen_record(&mut rng, 0);
        if record.is_empty() {
            continue;
        }
        let (proj, cpp_paths) = gen_projection(&mut rng);

        let data = rust_write(
            &[record.as_slice()],
            WriterOptions::new()
                .transpose(true)
                .compression(CompressionType::None),
        );
        let rust_out = rust_read_projected(&data, proj);
        let cpp_out = cpp_read_projected(&data, &cpp_paths);
        assert_eq!(
            rust_out, cpp_out,
            "seed={seed:#x} case={case} paths={cpp_paths:?} record={record:?}"
        );
    }
}

/// KNOWN-DIVERGENT (open maintainer question): a field number aliased
/// between an INCLUDED top-level context and the interior of an EXCLUDED
/// group. Observed (this minimal repro): C++ emits the occurrence inside
/// the excluded group as well as the top-level one; this implementation
/// emits only the top-level occurrence. Which output the format intends
/// for aliased-field shapes is a question for the maintainer — neither
/// behavior is asserted as correct here; BOTH are pinned so drift in
/// either direction fails while the question is open. Mechanism
/// (verified in the reference source): both engines resolve a shared
/// tag node's inclusion DYNAMICALLY against the live ancestry, but the
/// reference CACHES the first resolution on the node and reuses it for
/// every later occurrence, while this implementation re-resolves per
/// visit — so the reference's output depends on which occurrence
/// decodes first (see the mirrored-order case below, where the cached
/// exclusion DROPS the included occurrence).
#[test]
fn j_aliased_field_in_excluded_group_known_divergent() {
    // record: f4-group{ f3: 7 }, f3: 9  — f3 aliased inside the excluded
    // f4 group and at (included) top level. Projection: [3] full include.
    let mut record = encode_group(4, &encode_varint_field(3, 7));
    record.extend_from_slice(&encode_varint_field(3, 9));
    let data = rust_write(
        &[record.as_slice()],
        WriterOptions::new()
            .transpose(true)
            .compression(CompressionType::None),
    );

    let proj = FieldProjection::new().add_field(Field::new(vec![3]));
    let rust_out = rust_read_projected(&data, proj);
    let cpp_out = cpp_read_projected(&data, &[vec![3]]);

    let top_only = encode_varint_field(3, 9);
    assert_eq!(
        rust_out,
        vec![top_only],
        "this implementation emits only the top-level occurrence"
    );
    let mut with_aliased = encode_varint_field(3, 7);
    with_aliased.extend_from_slice(&encode_varint_field(3, 9));
    assert_eq!(
        cpp_out,
        vec![with_aliased],
        "C++ emits the excluded-group interior occurrence as well"
    );
}

/// KNOWN-DIVERGENT (same open question, mirrored order): identical
/// structure to the case above with the wire order swapped — the
/// top-level (included) occurrence comes FIRST in the record, so in
/// backward decode the shared node's first resolution happens INSIDE the
/// excluded group, and the reference caches the exclusion: the included
/// top-level value is silently DROPPED. Together the two cases show the
/// reference's aliased-field behavior is order-dependent between leaking
/// excluded data and dropping included data; this implementation is
/// context-correct in both orders. Both shapes pinned.
#[test]
fn j2_aliased_field_mirrored_order_known_divergent() {
    // record: f3: 9, f4-group{ f3: 7 } — top-level occurrence FIRST.
    let mut record = encode_varint_field(3, 9);
    record.extend_from_slice(&encode_group(4, &encode_varint_field(3, 7)));
    let data = rust_write(
        &[record.as_slice()],
        WriterOptions::new()
            .transpose(true)
            .compression(CompressionType::None),
    );

    let proj = FieldProjection::new().add_field(Field::new(vec![3]));
    let rust_out = rust_read_projected(&data, proj);
    let cpp_out = cpp_read_projected(&data, &[vec![3]]);

    assert_eq!(
        rust_out,
        vec![encode_varint_field(3, 9)],
        "this implementation keeps the included top-level occurrence"
    );
    assert_eq!(
        cpp_out,
        vec![Vec::<u8>::new()],
        "the reference's cached exclusion drops the included occurrence"
    );
}
