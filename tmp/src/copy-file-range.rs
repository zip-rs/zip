use std::{
    env,
    io::Cursor,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    pin::Pin,
    str::FromStr,
    sync::Arc,
};

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
    use std::io::Write;

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
    let source = if flag_var("SMALL") || flag_var("small") {
        println!("small!");
        &*SMALL_ARCHIVE_PATH
    } else {
        println!("big!");
        &*BIG_ARCHIVE_PATH
    }
    .to_path_buf();

    use zip::tokio::{buf_reader::BufReader, os::copy_file_range::*};

    let len: u64 = get_len(&source).await?;
    println!("len = {}", len);

    let out_path: PathBuf = path_var("OUT")
        .or(path_var("out"))
        .unwrap_or_else(|| PathBuf::from("./tmp-copy-out"));
    println!("out = {}", out_path.display());

    let num_iters: usize = num_var("N").or(num_var("n")).unwrap_or(15);
    println!("num_iters = {}", num_iters);

    for _ in 0..num_iters {
        if flag_var("ASYNC") || flag_var("async") {
            println!("async!");
            use tokio::io::AsyncWriteExt;

            let non_zero_len = NonZeroUsize::new(len as usize).unwrap();

            let handle = fs::File::open(&source).await?;
            let mut out = fs::File::create(&out_path).await?;

            let mut buf_reader = BufReader::with_capacity(non_zero_len, Box::pin(handle));

            let written = io::copy_buf(&mut buf_reader, &mut out).await?;
            assert_eq!(written, len);

            out.shutdown().await?;
        } else {
            println!("copy_file_range!");
            let source = source.clone();
            let out_path = out_path.clone();
            task::spawn_blocking(move || {
                use std::{fs, io};

                let handle = fs::File::open(source)?;
                let mut src = MutateInnerOffset::new(handle, Role::Readable)?;
                let src = Pin::new(&mut src);

                let out = fs::File::create(out_path)?;
                let mut dst = MutateInnerOffset::new(out, Role::Writable)?;
                let dst = Pin::new(&mut dst);

                let len = len as usize;
                assert_eq!(len, copy_file_range(src, dst, len)?);

                Ok::<_, io::Error>(())
            })
            .await
            .unwrap()?;
        };
    }

    Ok(())
}
