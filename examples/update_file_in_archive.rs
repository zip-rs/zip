// See this dicussion for further background on why it is done like this:
// https://github.com/zip-rs/zip/discussions/430

use std::io::prelude::*;

use zip::result::{ZipError, ZipResult};

fn main() {
    std::process::exit(real_main());
}

fn real_main() -> i32 {
    let args: Vec<_> = std::env::args().collect();
    if args.len() < 3 {
        println!(
            "Usage: {} <filename> <file_within_archive_to_update>",
            args[0]
        );
        return 1;
    }
    let filename = &*args[1];
    let file_to_update = &*args[2];
    match update_file(filename, file_to_update, false) {
        Ok(_) => println!("{file_to_update} updated in {filename}"),
        Err(e) => {
            eprintln!("Error: {e:?}");
            return 1;
        }
    }
    0
}

fn update_file(archive_filename: &str, file_to_update: &str, in_place: bool) -> ZipResult<()> {
    let fname = std::path::Path::new(archive_filename);
    let zipfile = std::fs::File::open(fname)?;

    let mut archive = zip::ZipArchive::new(zipfile)?;

    // Open a new, empty archive for writing to
    let new_filename = replacement_filename(archive_filename.as_ref())?;
    let new_file = std::fs::File::create(&new_filename)?;
    let mut new_archive = zip::ZipWriter::new(new_file);

    // Loop through the original archive:
    //  - Write the target file from some bytes
    //  - Copy everything else across as raw, which saves the bother of decoding it
    // The end effect is to have a new archive, which is a clone of the original,
    // save for the target file which has been re-written
    let target: &std::path::Path = file_to_update.as_ref();
    let new = b"Lorem ipsum";
    for i in 0..archive.len() {
        let file = archive.by_index_raw(i).unwrap();
        match file.enclosed_name() {
            Some(p) if p == target => {
                new_archive.start_file(file_to_update, zip::write::FileOptions::default())?;
                new_archive.write_all(new)?;
                new_archive.flush()?;
            }
            _ => new_archive.raw_copy_file(file)?,
        }
    }
    new_archive.finish()?;

    drop(archive);
    drop(new_archive);

    // If we're doing this in place then overwrite the original with the new
    if in_place {
        std::fs::rename(new_filename, archive_filename)?;
    }

    Ok(())
}

fn replacement_filename(source: &std::path::Path) -> ZipResult<std::path::PathBuf> {
    let mut new = std::path::PathBuf::from(source);
    let mut stem = source.file_stem().ok_or(ZipError::FileNotFound)?.to_owned();
    stem.push("_updated");
    new.set_file_name(stem);
    let ext = source.extension().ok_or(ZipError::FileNotFound)?;
    new.set_extension(ext);
    Ok(new)
}
