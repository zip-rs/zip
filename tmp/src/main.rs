#![allow(unused_imports)]
#![allow(dead_code)]

use std::env;
use std::io::{Cursor, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;

use getrandom::getrandom;
use once_cell::sync::Lazy;
use tokio::{fs, io, task};

use zip::{
    result::{ZipError, ZipResult},
    write::FileOptions,
    CompressionMethod, ZipWriter,
};

fn generate_random_archive(
    num_entries: usize,
    entry_size: usize,
    out_path: &Path,
) -> ZipResult<()> {
    eprintln!("num_entries = {}", num_entries);
    eprintln!("entry_size = {}", entry_size);

    let out_handle = std::fs::File::create(out_path)?;
    let mut zip = ZipWriter::new(out_handle);
    /* No point compressing random entries. */
    let options = FileOptions::default().compression_method(CompressionMethod::Stored);

    let mut bytes = vec![0u8; entry_size];
    for i in 0..num_entries {
        let name = format!("random{}.dat", i);
        zip.start_file(name, options)?;
        getrandom(&mut bytes).unwrap();
        zip.write_all(&bytes)?;
    }

    let out_handle = zip.finish()?;
    out_handle.sync_all()?;

    Ok(())
}

async fn get_len(p: &Path) -> io::Result<u64> {
    Ok(fs::metadata(p).await?.len())
}

static BIG_ARCHIVE_PATH: Lazy<PathBuf> =
    Lazy::new(|| Path::new("../benches/target.zip").to_path_buf());

static SMALL_ARCHIVE_PATH: Lazy<PathBuf> =
    Lazy::new(|| Path::new("../benches/small-target.zip").to_path_buf());

fn flag_var(var_name: &str) -> bool {
    env::var(var_name)
        .ok()
        .filter(|v| v.starts_with('y'))
        .is_some()
}

fn num_var(var_name: &str) -> Option<usize> {
    let var = env::var(var_name).ok()?;
    let n = usize::from_str(&var).ok()?;
    Some(n)
}

fn path_var(var_name: &str) -> Option<PathBuf> {
    let var = env::var(var_name).ok()?;
    Some(var.into())
}

#[tokio::main]
async fn main() -> ZipResult<()> {
    let n = num_var("N").or(num_var("n")).unwrap_or(5);
    eprintln!("n = {}", n);

    let td = task::spawn_blocking(move || tempfile::tempdir())
        .await
        .unwrap()?;

    let test_archive_path = if flag_var("RANDOM") || flag_var("random") {
        let zip_out_path = td.path().join("random.zip");
        let num_entries: usize = num_var("RANDOM_N").or(num_var("random_n")).unwrap_or(1_000);
        let entry_size: usize = num_var("RANDOM_SIZE")
            .or(num_var("random_size"))
            .unwrap_or(10_000);
        {
            let z2 = zip_out_path.clone();
            task::spawn_blocking(move || generate_random_archive(num_entries, entry_size, &z2))
                .await
                .unwrap()?;
        }
        eprintln!(
            "random({}) = {}",
            get_len(&zip_out_path).await?,
            zip_out_path.display()
        );
        zip_out_path
    } else if flag_var("SMALL") || flag_var("small") {
        eprintln!(
            "small({}) = {}",
            get_len(&*SMALL_ARCHIVE_PATH).await?,
            SMALL_ARCHIVE_PATH.display()
        );
        SMALL_ARCHIVE_PATH.to_path_buf()
    } else {
        eprintln!(
            "big({}) = {}",
            get_len(&*BIG_ARCHIVE_PATH).await?,
            BIG_ARCHIVE_PATH.display()
        );
        BIG_ARCHIVE_PATH.to_path_buf()
    };

    let out_path = path_var("OUT")
        .or(path_var("out"))
        .unwrap_or_else(|| PathBuf::from("./tmp-out"));
    eprintln!("out = {}", out_path.display());

    if flag_var("SYNC") || flag_var("sync") {
        eprintln!("synchronous!");
        task::spawn_blocking(move || {
            for _ in 0..n {
                let handle = std::fs::OpenOptions::new()
                    .read(true)
                    .open(&test_archive_path)?;
                let mut src = zip::read::ZipArchive::new(handle)?;
                src.extract(&out_path)?;
            }

            Ok::<_, ZipError>(())
        })
        .await
        .unwrap()?;
    } else {
        eprintln!("async!");
        let out = Arc::new(out_path);
        for _ in 0..n {
            let handle = fs::OpenOptions::new()
                .read(true)
                /* .custom_flags(libc::O_NONBLOCK) */
                .open(&test_archive_path)
                .await?;
            let mut src = zip::tokio::read::ZipArchive::new(Box::pin(handle)).await?;
            Pin::new(&mut src).extract(out.clone()).await?;
        }
    }

    Ok(())
}
