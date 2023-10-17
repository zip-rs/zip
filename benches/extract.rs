use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use getrandom::getrandom;
use once_cell::sync::Lazy;
use tempfile::tempdir;
use tokio::{fs, io, runtime::Runtime};
use uuid::Uuid;

use zip::{result::ZipResult, write::FileOptions, ZipWriter};

fn generate_random_archive(
    num_entries: usize,
    entry_size: usize,
    options: FileOptions,
) -> ZipResult<Cursor<Box<[u8]>>> {
    use std::io::Write;

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

    Ok(Cursor::new(buf.into_boxed_slice()))
}

static BIG_ARCHIVE_PATH: Lazy<PathBuf> =
    Lazy::new(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("benches/target.zip"));

static SMALL_ARCHIVE_PATH: Lazy<PathBuf> =
    Lazy::new(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("benches/small-target.zip"));

const NUM_ENTRIES: usize = 1_000;
const ENTRY_SIZE: usize = 10_000;

pub fn bench_io(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    let mut group = c.benchmark_group("io");

    let options = FileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    let gen_td = tempdir().unwrap();
    let random_path = gen_td.path().join("random.zip");
    std::io::copy(
        &mut generate_random_archive(NUM_ENTRIES, ENTRY_SIZE, options).unwrap(),
        &mut std::fs::File::create(&random_path).unwrap(),
    )
    .unwrap();

    for (path, desc, n) in [
        (&*BIG_ARCHIVE_PATH, "big archive", Some(30)),
        (&*SMALL_ARCHIVE_PATH, "small archive", None),
        (&random_path, "random archive", None),
    ] {
        let len = std::fs::metadata(&path).unwrap().len() as usize;
        let id = format!("{}({} bytes)", desc, len);

        group.throughput(Throughput::Bytes(len as u64));

        if let Some(n) = n {
            group.sample_size(n);
        }

        group.bench_function(BenchmarkId::new(&id, "<copy_file_range>"), |b| {
            use zip::tokio::os::copy_file_range::*;

            let td = tempdir().unwrap();
            b.to_async(&rt).iter(|| async {
                let sync_handle = std::fs::File::open(path).unwrap();
                let mut src = MutateInnerOffset::new(sync_handle, Role::Readable).unwrap();
                let mut src = Pin::new(&mut src);

                let cur_name = format!("{}.zip", Uuid::new_v4());
                let tf = td.path().join(cur_name);
                let out = std::fs::File::create(tf).unwrap();
                let mut dst = MutateInnerOffset::new(out, Role::Writable).unwrap();
                let mut dst = Pin::new(&mut dst);

                let written = copy_file_range(src, dst, len).await.unwrap();
                assert_eq!(written, len);
            });
        });

        group.bench_function(BenchmarkId::new(&id, "<async copy_buf>"), |b| {
            let td = tempdir().unwrap();
            b.to_async(&rt).iter(|| async {
                let handle = fs::File::open(path).await.unwrap();
                let mut buf_handle = io::BufReader::with_capacity(len, handle);

                let cur_name = format!("{}.zip", Uuid::new_v4());
                let tf = td.path().join(cur_name);

                let mut out = fs::File::create(tf).await.unwrap();
                assert_eq!(
                    len as u64,
                    io::copy_buf(&mut buf_handle, &mut out).await.unwrap()
                );
            });
        });

        group.bench_function(BenchmarkId::new(&id, "<async copy (no buf)>"), |b| {
            let td = tempdir().unwrap();
            b.to_async(&rt).iter(|| async {
                let mut handle = fs::File::open(path).await.unwrap();

                let cur_name = format!("{}.zip", Uuid::new_v4());
                let tf = td.path().join(cur_name);

                let mut out = fs::File::create(tf).await.unwrap();
                assert_eq!(len as u64, io::copy(&mut handle, &mut out).await.unwrap());
            });
        });

        group.bench_function(BenchmarkId::new(&id, "<sync copy (no buf)>"), |b| {
            let td = tempdir().unwrap();
            b.iter(|| {
                let mut sync_handle = std::fs::File::open(path).unwrap();

                let cur_name = format!("{}.zip", Uuid::new_v4());
                let tf = td.path().join(cur_name);

                let mut out = std::fs::File::create(tf).unwrap();
                assert_eq!(
                    len as u64,
                    /* NB: this doesn't use copy_buf like the async case, because std::io has no
                     * corresponding function, and using an std::io::BufReader wrapper actually
                     * hurts perf by a lot!! */
                    std::io::copy(&mut sync_handle, &mut out).unwrap()
                );
            });
        });
    }
}

pub fn bench_extract(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut group = c.benchmark_group("extract");

    let options = FileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    let gen_td = tempdir().unwrap();
    let random_path = gen_td.path().join("random.zip");
    std::io::copy(
        &mut generate_random_archive(NUM_ENTRIES, ENTRY_SIZE, options).unwrap(),
        &mut std::fs::File::create(&random_path).unwrap(),
    )
    .unwrap();

    for (path, desc, n, t) in [
        (
            &*BIG_ARCHIVE_PATH,
            "big archive",
            Some(10),
            Some(Duration::from_secs(10)),
        ),
        (&*SMALL_ARCHIVE_PATH, "small archive", None, None),
        (&random_path, "random archive", None, None),
    ] {
        let len = std::fs::metadata(&path).unwrap().len() as usize;
        let id = format!("{}({} bytes)", desc, len);

        group.throughput(Throughput::Bytes(len as u64));

        if let Some(n) = n {
            group.sample_size(n);
        }
        if let Some(t) = t {
            group.measurement_time(t);
        }

        group.bench_function(BenchmarkId::new(&id, "<async extract()>"), |b| {
            let td = tempdir().unwrap();
            b.to_async(&rt).iter(|| async {
                let out_dir = Arc::new(td.path().to_path_buf());
                let handle = fs::File::open(path).await.unwrap();
                let mut zip = zip::tokio::read::ZipArchive::new(Box::pin(handle))
                    .await
                    .unwrap();
                Pin::new(&mut zip).extract(out_dir).await.unwrap();
            })
        });

        group.bench_function(BenchmarkId::new(&id, "<async extract_simple()>"), |b| {
            let td = tempdir().unwrap();
            b.to_async(&rt).iter(|| async {
                let out_dir = Arc::new(td.path().to_path_buf());
                let handle = fs::File::open(path).await.unwrap();
                let mut zip = zip::tokio::read::ZipArchive::new(Box::pin(handle))
                    .await
                    .unwrap();
                Pin::new(&mut zip).extract_simple(out_dir).await.unwrap();
            })
        });

        group.bench_function(BenchmarkId::new(&id, "<sync extract()>"), |b| {
            let td = tempdir().unwrap();
            b.iter(|| {
                let sync_handle = std::fs::File::open(path).unwrap();
                let mut zip = zip::read::ZipArchive::new(sync_handle).unwrap();
                zip.extract(td.path()).unwrap();
            })
        });
    }
}

criterion_group!(benches, bench_io, bench_extract);
criterion_main!(benches);
