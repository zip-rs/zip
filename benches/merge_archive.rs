/* use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput}; */

/* use std::io::Cursor; */
/* use std::path::{Path, PathBuf}; */
/* use std::pin::Pin; */
/* use std::sync::Arc; */
/* use std::time::Duration; */

/* use getrandom::getrandom; */
/* use once_cell::sync::Lazy; */
/* use tempfile::tempdir; */
/* use tokio::{fs, io, runtime::Runtime}; */
/* use uuid::Uuid; */

/* use zip::{result::ZipResult, write::FileOptions, ZipWriter}; */

/* fn generate_random_archive( */
/*     num_entries: usize, */
/*     entry_size: usize, */
/*     options: FileOptions, */
/* ) -> ZipResult<Cursor<Box<[u8]>>> { */
/*     use std::io::Write; */

/*     let buf = Cursor::new(Vec::new()); */
/*     let mut zip = ZipWriter::new(buf); */

/*     let mut bytes = vec![0u8; entry_size]; */
/*     for i in 0..num_entries { */
/*         let name = format!("random{}.dat", i); */
/*         zip.start_file(name, options)?; */
/*         getrandom(&mut bytes).unwrap(); */
/*         zip.write_all(&bytes)?; */
/*     } */

/*     let buf = zip.finish()?.into_inner(); */

/*     Ok(Cursor::new(buf.into_boxed_slice())) */
/* } */

/* static BIG_ARCHIVE_PATH: Lazy<PathBuf> = */
/*     Lazy::new(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("benches/target.zip")); */

/* static SMALL_ARCHIVE_PATH: Lazy<PathBuf> = */
/*     Lazy::new(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("benches/small-target.zip")); */

/* const NUM_ENTRIES: usize = 1_000; */
/* const ENTRY_SIZE: usize = 10_000; */

/* fn perform_merge<R: io::AsyncRead + io::AsyncSeek, W: io::AsyncWrite + io::AsyncSeek>( */
/*     mut src: ZipArchive<R>, */
/*     mut target: ZipWriter<W>, */
/* ) -> ZipResult<ZipWriter<W>> { */
/*     (target).merge_archive(Pin::new(&mut src))?; */
/*     Ok(target) */
/* } */

/* fn perform_raw_copy_file<R: Read + Seek, W: Write + Seek>( */
/*     mut src: ZipArchive<R>, */
/*     mut target: ZipWriter<W>, */
/* ) -> ZipResult<ZipWriter<W>> { */
/*     for i in 0..src.len() { */
/*         let entry = src.by_index(i)?; */
/*         target.raw_copy_file(entry)?; */
/*     } */
/*     Ok(target) */
/* } */

/* const NUM_ENTRIES: usize = 100; */
/* const ENTRY_SIZE: usize = 1024; */

/* fn merge_archive_stored(bench: &mut Bencher) { */
/*     let options = FileOptions::default().compression_method(zip::CompressionMethod::Stored); */
/*     let (len, src) = generate_random_archive(NUM_ENTRIES, ENTRY_SIZE, options).unwrap(); */

/*     bench.bytes = len as u64; */

/*     bench.iter(|| { */
/*         let buf = Cursor::new(Vec::new()); */
/*         let zip = ZipWriter::new(buf); */
/*         let mut zip = perform_merge(src.clone(), zip).unwrap(); */
/*         let buf = zip.finish().unwrap().into_inner(); */
/*         assert_eq!(buf.len(), len); */
/*     }); */
/* } */

/* fn merge_archive_compressed(bench: &mut Bencher) { */
/*     let options = FileOptions::default().compression_method(zip::CompressionMethod::Deflated); */
/*     let (len, src) = generate_random_archive(NUM_ENTRIES, ENTRY_SIZE, options).unwrap(); */

/*     bench.bytes = len as u64; */

/*     bench.iter(|| { */
/*         let buf = Cursor::new(Vec::new()); */
/*         let zip = ZipWriter::new(buf); */
/*         let mut zip = perform_merge(src.clone(), zip).unwrap(); */
/*         let buf = zip.finish().unwrap().into_inner(); */
/*         assert_eq!(buf.len(), len); */
/*     }); */
/* } */

/* fn merge_archive_raw_copy_file_stored(bench: &mut Bencher) { */
/*     let options = FileOptions::default().compression_method(zip::CompressionMethod::Stored); */
/*     let (len, src) = generate_random_archive(NUM_ENTRIES, ENTRY_SIZE, options).unwrap(); */

/*     bench.bytes = len as u64; */

/*     bench.iter(|| { */
/*         let buf = Cursor::new(Vec::new()); */
/*         let zip = ZipWriter::new(buf); */
/*         let mut zip = perform_raw_copy_file(src.clone(), zip).unwrap(); */
/*         let buf = zip.finish().unwrap().into_inner(); */
/*         assert_eq!(buf.len(), len); */
/*     }); */
/* } */

/* fn merge_archive_raw_copy_file_compressed(bench: &mut Bencher) { */
/*     let options = FileOptions::default().compression_method(zip::CompressionMethod::Deflated); */
/*     let (len, src) = generate_random_archive(NUM_ENTRIES, ENTRY_SIZE, options).unwrap(); */

/*     bench.bytes = len as u64; */

/*     bench.iter(|| { */
/*         let buf = Cursor::new(Vec::new()); */
/*         let zip = ZipWriter::new(buf); */
/*         let mut zip = perform_raw_copy_file(src.clone(), zip).unwrap(); */
/*         let buf = zip.finish().unwrap().into_inner(); */
/*         assert_eq!(buf.len(), len); */
/*     }); */
/* } */

/* benchmark_group!( */
/*     benches, */
/*     merge_archive_stored, */
/*     merge_archive_compressed, */
/*     merge_archive_raw_copy_file_stored, */
/*     merge_archive_raw_copy_file_compressed, */
/* ); */
/* benchmark_main!(benches); */
