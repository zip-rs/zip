use bencher::{benchmark_group, benchmark_main};

use std::io::{Cursor, Read, Seek, Write};
use std::path::{Path, PathBuf};

use bencher::Bencher;
use getrandom::getrandom;
use once_cell::sync::Lazy;
use tempfile::tempdir;
use zip::{read::IntermediateFile, result::ZipResult, write::FileOptions, ZipArchive, ZipWriter};

fn generate_random_archive(
    num_entries: usize,
    entry_size: usize,
    options: FileOptions,
) -> ZipResult<Vec<u8>> {
    let buf = Cursor::new(Vec::new());
    let mut zip = ZipWriter::new(buf);

    let mut bytes = vec![0u8; entry_size];
    for i in 0..num_entries {
        let name = format!("random{}.dat", i);
        zip.start_file(name, options)?;
        getrandom(&mut bytes).unwrap();
        zip.write_all(&bytes)?;
    }

    let buf = zip.finish()?.into_inner();

    Ok(buf)
}

static BIG_ARCHIVE_PATH: Lazy<PathBuf> =
    Lazy::new(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("benches/target.zip"));

fn get_archive(path: impl AsRef<Path>) -> ZipResult<(u64, ZipArchive<IntermediateFile>)> {
    let f = IntermediateFile::from_path(path)?;
    let len = f.len();
    let archive = ZipArchive::new(f)?;
    Ok((len as u64, archive))
}

static SMALL_ARCHIVE_PATH: Lazy<PathBuf> =
    Lazy::new(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("benches/small-target.zip"));

fn get_big_archive() -> ZipResult<(u64, ZipArchive<IntermediateFile>)> {
    get_archive(&*BIG_ARCHIVE_PATH)
}

fn get_small_archive() -> ZipResult<(u64, ZipArchive<IntermediateFile>)> {
    get_archive(&*SMALL_ARCHIVE_PATH)
}

fn perform_pipelined<P: AsRef<Path>>(
    src: ZipArchive<IntermediateFile>,
    target: P,
) -> ZipResult<()> {
    src.extract_pipelined(target)
}

fn perform_sync<R: Read + Seek, W: Write + Seek, P: AsRef<Path>>(
    mut src: ZipArchive<R>,
    target: P,
) -> ZipResult<()> {
    src.extract(target)
}

const NUM_ENTRIES: usize = 1_000;
const ENTRY_SIZE: usize = 10_000;

fn extract_pipelined_random(bench: &mut Bencher) {
    let options = FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let src = generate_random_archive(NUM_ENTRIES, ENTRY_SIZE, options).unwrap();
    bench.bytes = src.len() as u64;
    let src = ZipArchive::new(IntermediateFile::from_bytes(&src)).unwrap();

    bench.iter(|| {
        let td = tempdir().unwrap();
        perform_pipelined(src.clone(), td).unwrap();
    });
}

fn extract_sync_random(bench: &mut Bencher) {
    let options = FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let src = generate_random_archive(NUM_ENTRIES, ENTRY_SIZE, options).unwrap();
    bench.bytes = src.len() as u64;

    bench.iter(|| {
        let td = tempdir().unwrap();
        perform_sync::<Cursor<Vec<u8>>, Cursor<Vec<u8>>, _>(
            ZipArchive::new(Cursor::new(src.clone())).unwrap(),
            td,
        )
        .unwrap();
    });
}

fn extract_pipelined_compressible_big(bench: &mut Bencher) {
    let (len, src) = get_big_archive().unwrap();
    bench.bytes = len;

    bench.bench_n(2, |_| ());
    bench.iter(|| {
        let td = tempdir().unwrap();
        perform_pipelined(src.clone(), td).unwrap();
    });
}

fn extract_sync_compressible_big(bench: &mut Bencher) {
    let (len, src) = get_big_archive().unwrap();
    bench.bytes = len;

    bench.bench_n(2, |_| ());
    bench.iter(|| {
        let td = tempdir().unwrap();
        perform_sync::<_, IntermediateFile, _>(src.clone(), td).unwrap();
    });
}

fn extract_pipelined_compressible_small(bench: &mut Bencher) {
    let (len, src) = get_small_archive().unwrap();
    bench.bytes = len;

    bench.bench_n(100, |_| ());
    bench.iter(|| {
        let td = tempdir().unwrap();
        perform_pipelined(src.clone(), td).unwrap();
    });
}

fn extract_sync_compressible_small(bench: &mut Bencher) {
    let (len, src) = get_small_archive().unwrap();
    bench.bytes = len;

    bench.bench_n(100, |_| ());
    bench.iter(|| {
        let td = tempdir().unwrap();
        perform_sync::<_, IntermediateFile, _>(src.clone(), td).unwrap();
    });
}

benchmark_group!(
    benches,
    extract_pipelined_random,
    extract_sync_random,
    extract_pipelined_compressible_big,
    extract_sync_compressible_big,
    extract_pipelined_compressible_small,
    extract_sync_compressible_small,
);
benchmark_main!(benches);
