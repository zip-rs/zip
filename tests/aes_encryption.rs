#![cfg(feature = "aes-crypto")]

use std::io::{self, Read, Write};

use zip::{result::ZipError, write::FileOptions, AesMode, CompressionMethod, ZipArchive};

const SECRET_CONTENT: &str = "Lorem ipsum dolor sit amet";

const PASSWORD: &[u8] = b"helloworld";

#[test]
fn aes256_encrypted_uncompressed_file() {
    let mut v = Vec::new();
    v.extend_from_slice(include_bytes!("data/aes_archive.zip"));
    let mut archive = ZipArchive::new(io::Cursor::new(v)).expect("couldn't open test zip file");

    let mut file = archive
        .by_name_decrypt("secret_data_256_uncompressed", PASSWORD)
        .expect("couldn't find file in archive")
        .expect("invalid password");
    assert_eq!("secret_data_256_uncompressed", file.name());

    let mut content = String::new();
    file.read_to_string(&mut content)
        .expect("couldn't read encrypted file");
    assert_eq!(SECRET_CONTENT, content);
}

#[test]
fn aes256_encrypted_file() {
    let mut v = Vec::new();
    v.extend_from_slice(include_bytes!("data/aes_archive.zip"));
    let mut archive = ZipArchive::new(io::Cursor::new(v)).expect("couldn't open test zip file");

    let mut file = archive
        .by_name_decrypt("secret_data_256", PASSWORD)
        .expect("couldn't find file in archive")
        .expect("invalid password");
    assert_eq!("secret_data_256", file.name());

    let mut content = String::new();
    file.read_to_string(&mut content)
        .expect("couldn't read encrypted and compressed file");
    assert_eq!(SECRET_CONTENT, content);
}

#[test]
fn aes192_encrypted_file() {
    let mut v = Vec::new();
    v.extend_from_slice(include_bytes!("data/aes_archive.zip"));
    let mut archive = ZipArchive::new(io::Cursor::new(v)).expect("couldn't open test zip file");

    let mut file = archive
        .by_name_decrypt("secret_data_192", PASSWORD)
        .expect("couldn't find file in archive")
        .expect("invalid password");
    assert_eq!("secret_data_192", file.name());

    let mut content = String::new();
    file.read_to_string(&mut content)
        .expect("couldn't read encrypted file");
    assert_eq!(SECRET_CONTENT, content);
}

#[test]
fn aes128_encrypted_file() {
    let mut v = Vec::new();
    v.extend_from_slice(include_bytes!("data/aes_archive.zip"));
    let mut archive = ZipArchive::new(io::Cursor::new(v)).expect("couldn't open test zip file");

    let mut file = archive
        .by_name_decrypt("secret_data_128", PASSWORD)
        .expect("couldn't find file in archive")
        .expect("invalid password");
    assert_eq!("secret_data_128", file.name());

    let mut content = String::new();
    file.read_to_string(&mut content)
        .expect("couldn't read encrypted file");
    assert_eq!(SECRET_CONTENT, content);
}

#[test]
fn aes128_stored_roundtrip() {
    let cursor = {
        let mut zip = zip::ZipWriter::new(io::Cursor::new(Vec::new()));

        zip.start_file(
            "test.txt",
            FileOptions::default().with_aes_encryption(AesMode::Aes128, "some password"),
        )
        .unwrap();
        zip.write_all(SECRET_CONTENT.as_bytes()).unwrap();

        zip.finish().unwrap()
    };

    let mut archive = ZipArchive::new(cursor).expect("couldn't open test zip file");
    test_extract_encrypted_file(&mut archive, "test.txt", "some password", "other password");
}

#[test]
fn aes256_deflated_roundtrip() {
    let cursor = {
        let mut zip = zip::ZipWriter::new(io::Cursor::new(Vec::new()));

        zip.start_file(
            "test.txt",
            FileOptions::default()
                .compression_method(CompressionMethod::Deflated)
                .with_aes_encryption(AesMode::Aes256, "some password"),
        )
        .unwrap();
        zip.write_all(SECRET_CONTENT.as_bytes()).unwrap();

        zip.finish().unwrap()
    };

    let mut archive = ZipArchive::new(cursor).expect("couldn't open test zip file");
    test_extract_encrypted_file(&mut archive, "test.txt", "some password", "other password");
}

fn test_extract_encrypted_file<R: io::Read + io::Seek>(
    archive: &mut ZipArchive<R>,
    file_name: &str,
    correct_password: &str,
    incorrect_password: &str,
) {
    {
        let file = archive.by_name(file_name).map(|_| ());
        match file {
            Err(ZipError::UnsupportedArchive("Password required to decrypt file")) => {}
            Err(err) => {
                panic!("Failed to read file for unknown reason: {err:?}");
            }
            Ok(_) => {
                panic!("Was able to successfully read encrypted file without password");
            }
        }
    }

    {
        archive
            .by_name_decrypt(file_name, incorrect_password.as_bytes())
            .expect("couldn't find file in archive")
            .map(|_| ())
            .expect_err("accepted invalid password");
    }

    {
        let mut content = String::new();
        archive
            .by_name_decrypt(file_name, correct_password.as_bytes())
            .expect("couldn't find file in archive")
            .expect("invalid password")
            .read_to_string(&mut content)
            .expect("couldn't read encrypted file");
        assert_eq!(SECRET_CONTENT, content);
    }
}
