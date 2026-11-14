/*
Copyright 2026 The Flame Authors.
Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at
    http://www.apache.org/licenses/LICENSE-2.0
Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

use super::*;
use std::io::Write;

#[test]
fn classify_directory() {
    let temp = TempDir::new().unwrap();
    assert_eq!(
        classify_application(temp.path()).unwrap(),
        ApplicationInputKind::Directory
    );
}

#[test]
fn prepare_executable_file_packages_under_bin() {
    let temp = TempDir::new().unwrap();
    let bin = temp.path().join("service");
    fs::write(&bin, b"#!/bin/sh\n").unwrap();
    make_executable(&bin);

    let prepared = prepare_application(&bin).unwrap();
    assert_eq!(prepared.kind, ApplicationInputKind::ExecutableFile);
    assert!(prepared.detection_root.join("bin/service").exists());
    assert_eq!(prepared.package_path.file_name().unwrap(), "service.tar.gz");
    assert_eq!(
        prepared.object_key("demo"),
        format!("demo/pkg/demo-{}.tar.gz", prepared.upload_id)
    );
    assert_eq!(prepared.sha256.len(), 64);
}

/// Identical deployment content gets distinct remote objects while each prepared plan keeps a stable identity.
#[test]
fn repeated_preparation_uses_unique_stable_package_objects() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("app.py");
    fs::write(&source, b"print('hello')\n").unwrap();

    let first = prepare_application(&source).unwrap();
    let second = prepare_application(&source).unwrap();
    assert_eq!(first.sha256, second.sha256);
    assert_eq!(
        fs::read(&first.package_path).unwrap(),
        fs::read(&second.package_path).unwrap()
    );
    assert_ne!(first.object_key("demo"), second.object_key("demo"));
    assert_ne!(first.upload_id, second.upload_id);
    let filename = first.package_filename("demo");
    let key = first.object_key("demo");
    assert_eq!(filename, first.package_filename("demo"));
    assert_eq!(key, first.object_key("demo"));
    assert_eq!(key, flame_rs::object::package_key("demo", &filename));
    let identity = filename
        .strip_prefix("demo-")
        .unwrap()
        .strip_suffix(".tar.gz")
        .unwrap();
    assert_eq!(Uuid::parse_str(identity).unwrap(), first.upload_id);
    assert_eq!(first.package_path.file_name().unwrap(), "app.py.tar.gz");
}

#[test]
fn tarball_path_traversal_is_rejected() {
    let temp = TempDir::new().unwrap();
    let package = temp.path().join("bad.tar.gz");
    let file = fs::File::create(&package).unwrap();
    let encoder = GzBuilder::new()
        .mtime(0)
        .write(file, Compression::default());
    let mut encoder = encoder;
    write_raw_tar_entry(&mut encoder, "../bad", b"bad");
    encoder.write_all(&[0_u8; 1024]).unwrap();
    encoder.finish().unwrap();

    let dest = temp.path().join("dest");
    fs::create_dir(&dest).unwrap();
    assert!(unpack_tar_gz(&package, &dest).is_err());
}

#[test]
fn tarball_curdir_entries_are_allowed() {
    assert!(validate_relative_path(Path::new("./app.py")).is_ok());
    assert!(validate_relative_path(Path::new("./bin/service")).is_ok());
}

fn write_raw_tar_entry<W: Write>(writer: &mut W, name: &str, data: &[u8]) {
    let mut header = [0_u8; 512];
    header[..name.len()].copy_from_slice(name.as_bytes());
    write_octal(&mut header[100..108], 0o644);
    write_octal(&mut header[108..116], 0);
    write_octal(&mut header[116..124], 0);
    write_octal(&mut header[124..136], data.len() as u64);
    write_octal(&mut header[136..148], 0);
    header[148..156].fill(b' ');
    header[156] = b'0';
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    let checksum: u32 = header.iter().map(|byte| *byte as u32).sum();
    write_octal(&mut header[148..156], checksum as u64);

    writer.write_all(&header).unwrap();
    writer.write_all(data).unwrap();
    let padding = (512 - (data.len() % 512)) % 512;
    if padding > 0 {
        writer.write_all(&vec![0_u8; padding]).unwrap();
    }
}

fn write_octal(field: &mut [u8], value: u64) {
    field.fill(0);
    let digits = format!("{:0width$o}", value, width = field.len() - 1);
    field[..digits.len()].copy_from_slice(digits.as_bytes());
}

fn make_executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

#[test]
fn directory_package_preserves_content_digest_and_source_name() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    let file = source.join("app.py");
    let mut f = fs::File::create(file).unwrap();
    writeln!(f, "print('hello')").unwrap();

    let prepared = prepare_application(&source).unwrap();
    assert!(prepared.package_path.exists());
    assert_eq!(prepared.package_path.file_name().unwrap(), "source.tar.gz");
    let package = fs::File::open(&prepared.package_path).unwrap();
    let mut archive = Archive::new(GzDecoder::new(package));
    let mut entries = archive.entries().unwrap();
    let mut entry = entries.next().unwrap().unwrap();
    assert_eq!(entry.path().unwrap().as_ref(), Path::new("app.py"));
    let mut contents = String::new();
    entry.read_to_string(&mut contents).unwrap();
    assert_eq!(contents, "print('hello')\n");
    assert!(entries.next().is_none());
    assert_eq!(
        prepared.object_key("demo"),
        format!("demo/pkg/demo-{}.tar.gz", prepared.upload_id)
    );
}

#[test]
fn directory_ignore_files_filter_archive_and_detection_root() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("source");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join(".flmignore"), "*.log\npyproject.toml\n").unwrap();
    fs::write(root.join(".flameignore"), "!keep.log\nsrc/keep.tmp\n").unwrap();
    fs::write(root.join(".gitignore"), "*.py\n").unwrap();
    fs::write(root.join("pyproject.toml"), "[project]\nname='demo'\n").unwrap();
    fs::write(root.join("drop.log"), "drop").unwrap();
    fs::write(root.join("keep.log"), "keep").unwrap();
    fs::write(root.join("src/.flmignore"), "*.tmp\n!keep.tmp\n").unwrap();
    fs::write(root.join("src/drop.tmp"), "drop").unwrap();
    fs::write(root.join("src/keep.tmp"), "keep").unwrap();
    fs::write(root.join("src/main.py"), "print('ok')\n").unwrap();

    let prepared = prepare_application(&root).unwrap();
    let package = fs::File::open(&prepared.package_path).unwrap();
    let mut archive = Archive::new(GzDecoder::new(package));
    let paths: Vec<_> = archive
        .entries()
        .unwrap()
        .map(|entry| entry.unwrap().path().unwrap().into_owned())
        .collect();
    assert!(paths.contains(&PathBuf::from("keep.log")));
    assert!(paths.contains(&PathBuf::from("src/main.py")));
    assert!(paths.contains(&PathBuf::from("src/keep.tmp")));
    assert!(!paths.contains(&PathBuf::from("drop.log")));
    assert!(!paths.contains(&PathBuf::from("src/drop.tmp")));
    assert!(!paths.contains(&PathBuf::from("pyproject.toml")));
    assert!(!prepared.detection_root.join("pyproject.toml").exists());
}

#[test]
fn regular_file_package_uses_full_filename() {
    let temp = TempDir::new().unwrap();
    let script = temp.path().join("main.py");
    fs::write(&script, "print('ok')\n").unwrap();
    make_executable(&script);

    let prepared = prepare_application(&script).unwrap();
    assert_eq!(prepared.kind, ApplicationInputKind::File);
    assert_eq!(prepared.package_path.file_name().unwrap(), "main.py.tar.gz");
    let package = fs::File::open(&prepared.package_path).unwrap();
    let mut archive = Archive::new(GzDecoder::new(package));
    let entry = archive.entries().unwrap().next().unwrap().unwrap();
    assert_eq!(entry.path().unwrap().as_ref(), Path::new("main.py"));
}

#[test]
fn tar_gz_input_keeps_its_filename() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("app.py"), "print('ok')\n").unwrap();
    let input = temp.path().join("bundle.tar.gz");
    package_directory(&source, &input, false).unwrap();

    let prepared = prepare_application(&input).unwrap();
    assert_eq!(prepared.package_path.file_name().unwrap(), "bundle.tar.gz");
}
