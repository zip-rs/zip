use byteorder::{LittleEndian, WriteBytesExt};
use std::collections::HashSet;
use std::io::prelude::*;
use std::io::{Cursor, Seek};
use std::iter::FromIterator;
use zip::write::FileOptions;
use zip::{CompressionMethod, SUPPORTED_COMPRESSION_METHODS};

#[derive(Debug, Clone, Copy)]
struct TestWriterConfig {
    streaming: bool,
    method: CompressionMethod,
    large_file: bool,
}

fn test_configs() -> Vec<TestWriterConfig> {
    let mut configs = Vec::new();
    for large_file in [false, true] {
        for streaming in [false, true] {
            for &method in SUPPORTED_COMPRESSION_METHODS {
                configs.push(TestWriterConfig {
                    streaming,
                    method,
                    large_file,
                });
            }
        }
    }
    configs
}

// This test asserts that after creating a zip file, then reading its contents back out,
// the extracted data will *always* be exactly the same as the original data.
#[test]
fn end_to_end() {
    for cfg in test_configs() {
        let mut file = Cursor::new(Vec::new());
        write_test_archive(&mut file, cfg).expect("Couldn't write test zip archive");
        check_archive_file(file, ENTRY_NAME, Some(cfg.method), LOREM_IPSUM);
    }
}

// This test asserts that after copying a `ZipFile` to a new `ZipWriter`, then reading its
// contents back out, the extracted data will *always* be exactly the same as the original data.
#[test]
fn copy() {
    for cfg in test_configs() {
        let src_file = &mut Cursor::new(Vec::new());
        write_test_archive(src_file, cfg).expect("Couldn't write test zip archive");

        let mut tgt_file = &mut Cursor::new(Vec::new());

        {
            let mut src_archive = zip::ZipArchive::new(src_file).unwrap();
            let mut zip = zip::ZipWriter::new(&mut tgt_file);

            {
                let file = src_archive
                    .by_name(ENTRY_NAME)
                    .expect("Missing expected file");

                zip.raw_copy_file(file).expect("Couldn't copy file");
            }

            {
                let file = src_archive
                    .by_name(ENTRY_NAME)
                    .expect("Missing expected file");

                zip.raw_copy_file_rename(file, COPY_ENTRY_NAME)
                    .expect("Couldn't copy and rename file");
            }
        }

        let mut tgt_archive = zip::ZipArchive::new(tgt_file).unwrap();
        check_archive_file_contents(&mut tgt_archive, ENTRY_NAME, LOREM_IPSUM);
        check_archive_file_contents(&mut tgt_archive, COPY_ENTRY_NAME, LOREM_IPSUM);
    }
}

// This test asserts that after appending to a `ZipWriter`, then reading its contents back out,
// both the prior data and the appended data will be exactly the same as their originals.
#[test]
fn append() {
    for cfg in test_configs() {
        let mut file = &mut Cursor::new(Vec::new());
        write_test_archive(file, cfg).expect("Couldn't write test zip archive");

        {
            let mut zip = zip::ZipWriter::new_append(&mut file).unwrap();
            zip.start_file(
                COPY_ENTRY_NAME,
                FileOptions::default().compression_method(cfg.method),
            )
            .unwrap();
            zip.write_all(LOREM_IPSUM).unwrap();
            zip.finish().unwrap();
        }

        let mut zip = zip::ZipArchive::new(&mut file).unwrap();
        check_archive_file_contents(&mut zip, ENTRY_NAME, LOREM_IPSUM);
        check_archive_file_contents(&mut zip, COPY_ENTRY_NAME, LOREM_IPSUM);
    }
}

// Write a test zip archive to buffer.
fn write_test_archive(
    file: &mut Cursor<Vec<u8>>,
    cfg: TestWriterConfig,
) -> zip::result::ZipResult<()> {
    println!("Writing file with {cfg:?}");
    let mut zip = if cfg.streaming {
        zip::ZipWriter::new_streaming(file)
    } else {
        zip::ZipWriter::new(file)
    };

    zip.add_directory("test/", Default::default())?;

    let options = FileOptions::default()
        .large_file(cfg.large_file)
        .compression_method(cfg.method)
        .unix_permissions(0o755);

    zip.start_file("test/☃.txt", options)?;
    zip.write_all(b"Hello, World!\n")?;

    zip.start_file_aligned("test_aligned.txt", options, TEST_ALIGNMENT)?;
    zip.write_all(b"i am aligned")?;

    zip.start_file("test_empty", options)?;
    zip.start_file_aligned("test_fake_aligned_empty", options, 1)?;
    zip.start_file_aligned("test_aligned_empty", options, TEST_ALIGNMENT)?;

    zip.start_file_with_extra_data("test_with_extra_data/🐢.txt", options)?;
    zip.write_u16::<LittleEndian>(0xbeef)?;
    zip.write_u16::<LittleEndian>(EXTRA_DATA.len() as u16)?;
    zip.write_all(EXTRA_DATA)?;
    zip.end_extra_data()?;
    zip.write_all(b"Hello, World! Again.\n")?;

    zip.start_file(ENTRY_NAME, options)?;
    zip.write_all(LOREM_IPSUM)?;

    zip.finish()?;
    Ok(())
}

// Load an archive from buffer and check for test data.
fn check_test_archive<R: Read + Seek>(zip_file: R) -> zip::result::ZipResult<zip::ZipArchive<R>> {
    let mut archive = zip::ZipArchive::new(zip_file).unwrap();

    // Check archive contains expected file names.
    {
        let expected_file_names = [
            "test/",
            "test/☃.txt",
            "test_aligned.txt",
            "test_empty",
            "test_fake_aligned_empty",
            "test_aligned_empty",
            "test_with_extra_data/🐢.txt",
            ENTRY_NAME,
        ];
        let expected_file_names = HashSet::from_iter(expected_file_names.iter().copied());
        let file_names = archive.file_names().collect::<HashSet<_>>();
        assert_eq!(file_names, expected_file_names);
    }

    // Check an archive file for extra data field contents.
    {
        let file_with_extra_data = archive.by_name("test_with_extra_data/🐢.txt")?;
        let mut extra_data = Vec::new();
        extra_data.write_u16::<LittleEndian>(0xbeef)?;
        extra_data.write_u16::<LittleEndian>(EXTRA_DATA.len() as u16)?;
        extra_data.write_all(EXTRA_DATA)?;
        assert_eq!(file_with_extra_data.extra_data(), extra_data.as_slice());
    }

    // Check alignment.
    for name in ["test_aligned.txt", "test_aligned_empty"] {
        let file_aligned = archive.by_name(name)?;
        let data_start = file_aligned.data_start();
        assert_eq!(
            data_start % TEST_ALIGNMENT as u64,
            0,
            "should be aligned to {TEST_ALIGNMENT}: {data_start}"
        );

        // central directory extra data should be empty
        // alignment is performed using local extra data, which is not exposed by ZipArchive
        assert!(file_aligned.extra_data().is_empty());
        // check that alignment was actually performed;
        // this can fail if the file is somehow naturally aligned;
        // in that case, modify `write_test_archive` to avoid it
        assert!(file_aligned.data_start() - file_aligned.header_start() > 100);
    }

    for name in [
        "test_empty",
        "test_aligned_empty",
        "test_fake_aligned_empty",
    ] {
        let mut file = archive.by_name(name)?;
        let mut content = Vec::new();
        assert_eq!(file.read_to_end(&mut content)?, 0);
    }

    Ok(archive)
}

// Read a file in the archive as a string.
fn read_archive_file<R: Read + Seek>(
    archive: &mut zip::ZipArchive<R>,
    name: &str,
) -> zip::result::ZipResult<String> {
    let mut file = archive.by_name(name)?;

    let mut contents = String::new();
    file.read_to_string(&mut contents).unwrap();

    Ok(contents)
}

// Check a file in the archive contains expected data and properties.
fn check_archive_file(
    mut zip_file: Cursor<Vec<u8>>,
    name: &str,
    expected_method: Option<CompressionMethod>,
    expected_data: &[u8],
) {
    println!("Checking file contents");
    let mut archive = check_test_archive(&mut zip_file).unwrap();

    if let Some(expected_method) = expected_method {
        // Check the file's compression method.
        let file = archive.by_name(name).unwrap();
        let real_method = file.compression();

        assert_eq!(
            expected_method, real_method,
            "File does not have expected compression method"
        );
    }

    check_archive_file_contents(&mut archive, name, expected_data);
}

// Check a file in the archive contains the given data.
fn check_archive_file_contents<R: Read + Seek>(
    archive: &mut zip::ZipArchive<R>,
    name: &str,
    expected: &[u8],
) {
    let file_contents: String = read_archive_file(archive, name).unwrap();
    assert_eq!(file_contents.as_bytes(), expected);
}

const LOREM_IPSUM : &[u8] = b"Lorem ipsum dolor sit amet, consectetur adipiscing elit. In tellus elit, tristique vitae mattis egestas, ultricies vitae risus. Quisque sit amet quam ut urna aliquet
molestie. Proin blandit ornare dui, a tempor nisl accumsan in. Praesent a consequat felis. Morbi metus diam, auctor in auctor vel, feugiat id odio. Curabitur ex ex,
dictum quis auctor quis, suscipit id lorem. Aliquam vestibulum dolor nec enim vehicula, porta tristique augue tincidunt. Vivamus ut gravida est. Sed pellentesque, dolor
vitae tristique consectetur, neque lectus pulvinar dui, sed feugiat purus diam id lectus. Class aptent taciti sociosqu ad litora torquent per conubia nostra, per
inceptos himenaeos. Maecenas feugiat velit in ex ultrices scelerisque id id neque.
";

const EXTRA_DATA: &[u8] = b"Extra Data";

const ENTRY_NAME: &str = "test/lorem_ipsum.txt";

const COPY_ENTRY_NAME: &str = "test/lorem_ipsum_renamed.txt";

const TEST_ALIGNMENT: u16 = 512;
