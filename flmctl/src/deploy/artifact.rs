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

use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use flame_rs::apis::FlameError;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::{Compression, GzBuilder};
use ignore::WalkBuilder;
use sha2::{Digest, Sha256};
use tar::{Archive, Builder, EntryType, Header};
use tempfile::TempDir;
use uuid::Uuid;

const PACKAGE_EXTENSIONS: [&str; 2] = [".tar.gz", ".tgz"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplicationInputKind {
    ExecutableFile,
    File,
    TarGz,
    Directory,
}

impl std::fmt::Display for ApplicationInputKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ExecutableFile => write!(f, "executable-file"),
            Self::File => write!(f, "file"),
            Self::TarGz => write!(f, "tar.gz"),
            Self::Directory => write!(f, "directory"),
        }
    }
}

pub struct PreparedApplication {
    pub kind: ApplicationInputKind,
    pub detection_root: PathBuf,
    pub package_path: PathBuf,
    pub sha256: String,
    /// Keep repeated name calculations stable while isolating separately prepared deployments.
    upload_id: Uuid,
    _temp_dir: TempDir,
}

impl PreparedApplication {
    /// Reuse one deployment identity while keeping repeated uploads in distinct single-part objects.
    pub fn package_filename(&self, app_name: &str) -> String {
        format!("{}-{}.tar.gz", app_name, self.upload_id)
    }

    pub fn object_key(&self, app_name: &str) -> String {
        flame_rs::object::package_key(app_name, &self.package_filename(app_name))
    }
}

pub fn prepare_application(path: &Path) -> Result<PreparedApplication, FlameError> {
    let input_path = path.canonicalize().map_err(|e| {
        FlameError::InvalidConfig(format!("failed to resolve {}: {}", path.display(), e))
    })?;

    let kind = classify_application(&input_path)?;
    let temp_dir = TempDir::new()
        .map_err(|e| FlameError::Internal(format!("failed to create temp dir: {}", e)))?;

    let source_root = match kind {
        ApplicationInputKind::ExecutableFile => {
            let root = temp_dir.path().join("binary");
            let bin_dir = root.join("bin");
            fs::create_dir_all(&bin_dir).map_err(|e| {
                FlameError::Internal(format!("failed to create binary package dir: {}", e))
            })?;
            let filename = input_path.file_name().ok_or_else(|| {
                FlameError::InvalidConfig(format!("invalid binary path: {}", input_path.display()))
            })?;
            fs::copy(&input_path, bin_dir.join(filename)).map_err(|e| {
                FlameError::Internal(format!("failed to copy binary into package dir: {}", e))
            })?;
            root
        }
        ApplicationInputKind::File => {
            let root = temp_dir.path().join("file");
            fs::create_dir(&root).map_err(|e| {
                FlameError::Internal(format!("failed to create file package dir: {}", e))
            })?;
            let filename = input_path.file_name().ok_or_else(|| {
                FlameError::InvalidConfig(format!("invalid file path: {}", input_path.display()))
            })?;
            fs::copy(&input_path, root.join(filename)).map_err(|e| {
                FlameError::Internal(format!("failed to copy file into package dir: {}", e))
            })?;
            root
        }
        ApplicationInputKind::TarGz => {
            let root = temp_dir.path().join("extract");
            fs::create_dir_all(&root).map_err(|e| {
                FlameError::Internal(format!("failed to create extraction dir: {}", e))
            })?;
            unpack_tar_gz(&input_path, &root)?;
            select_detection_root(&root)?
        }
        ApplicationInputKind::Directory => input_path.clone(),
    };

    let input_name = input_path
        .file_name()
        .ok_or_else(|| FlameError::InvalidConfig("application path has no name".to_string()))?
        .to_string_lossy();
    let package_name = match kind {
        ApplicationInputKind::TarGz if input_name.ends_with(".tar.gz") => input_name.into_owned(),
        ApplicationInputKind::TarGz => format!("{}.tar.gz", input_name.trim_end_matches(".tgz")),
        ApplicationInputKind::Directory
        | ApplicationInputKind::ExecutableFile
        | ApplicationInputKind::File => {
            format!("{}.tar.gz", input_name)
        }
    };
    let package_path = temp_dir.path().join(package_name);
    package_directory(
        &source_root,
        &package_path,
        kind == ApplicationInputKind::Directory,
    )?;
    let sha256 = sha256_file(&package_path)?;

    // Detect against the exact files the executor will receive. Ignore rules
    // can remove a pyproject or executable from a directory package.
    let detection_root = if kind == ApplicationInputKind::Directory {
        let root = temp_dir.path().join("detection");
        fs::create_dir(&root)
            .map_err(|e| FlameError::Internal(format!("failed to create detection dir: {}", e)))?;
        unpack_tar_gz(&package_path, &root)?;
        root
    } else {
        source_root
    };

    Ok(PreparedApplication {
        kind,
        detection_root,
        package_path,
        sha256,
        upload_id: Uuid::new_v4(),
        _temp_dir: temp_dir,
    })
}

pub fn classify_application(path: &Path) -> Result<ApplicationInputKind, FlameError> {
    if path.is_dir() {
        return Ok(ApplicationInputKind::Directory);
    }

    if path.is_file() && has_package_extension(path) {
        return Ok(ApplicationInputKind::TarGz);
    }

    if path.is_file() && path.extension().is_some_and(|ext| ext == "py") {
        return Ok(ApplicationInputKind::File);
    }

    if path.is_file() && is_executable(path)? {
        return Ok(ApplicationInputKind::ExecutableFile);
    }

    if path.is_file() {
        return Ok(ApplicationInputKind::File);
    }

    Err(FlameError::InvalidConfig(format!(
        "{} must be a directory or regular file",
        path.display()
    )))
}

fn has_package_extension(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    PACKAGE_EXTENSIONS.iter().any(|ext| name.ends_with(ext))
}

pub fn is_executable(path: &Path) -> Result<bool, FlameError> {
    let metadata = fs::metadata(path).map_err(|e| {
        FlameError::InvalidConfig(format!("failed to stat {}: {}", path.display(), e))
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Ok(metadata.permissions().mode() & 0o111 != 0)
    }

    #[cfg(not(unix))]
    {
        Ok(metadata.is_file())
    }
}

fn package_directory(
    src_root: &Path,
    dest_path: &Path,
    use_ignore_rules: bool,
) -> Result<(), FlameError> {
    let file = fs::File::create(dest_path).map_err(|e| {
        FlameError::Internal(format!(
            "failed to create package {}: {}",
            dest_path.display(),
            e
        ))
    })?;
    let encoder = GzBuilder::new()
        .mtime(0)
        .write(file, Compression::default());
    let mut builder = Builder::new(encoder);
    let root = src_root.canonicalize().map_err(|e| {
        FlameError::Internal(format!(
            "failed to resolve package root {}: {}",
            src_root.display(),
            e
        ))
    })?;

    let mut entries = Vec::new();
    if use_ignore_rules {
        collect_deploy_entries(&root, &mut entries)?;
    } else {
        collect_entries(&root, Path::new(""), &root, &mut entries)?;
    }
    entries.sort();

    for relative in entries {
        let src = root.join(&relative);
        append_file(&mut builder, &root, &src, &relative)?;
    }

    let encoder = builder
        .into_inner()
        .map_err(|e| FlameError::Internal(format!("failed to finish tar archive: {}", e)))?;
    encoder
        .finish()
        .map_err(|e| FlameError::Internal(format!("failed to finish gzip archive: {}", e)))?;
    Ok(())
}

fn collect_deploy_entries(root: &Path, entries: &mut Vec<PathBuf>) -> Result<(), FlameError> {
    let mut walker = WalkBuilder::new(root);
    walker
        .standard_filters(false)
        .add_custom_ignore_filename(".flmignore")
        .add_custom_ignore_filename(".flameignore");

    for result in walker.build() {
        let entry = result.map_err(|e| {
            FlameError::InvalidConfig(format!("failed to walk application directory: {}", e))
        })?;
        let path = entry.path();
        if path == root {
            continue;
        }
        if entry.file_type().is_some_and(|kind| kind.is_dir()) {
            continue;
        }
        if entry.file_type().is_some_and(|kind| kind.is_symlink()) {
            let target = path.canonicalize().map_err(|e| {
                FlameError::InvalidConfig(format!(
                    "failed to resolve symlink {}: {}",
                    path.display(),
                    e
                ))
            })?;
            if !target.starts_with(root) {
                return Err(FlameError::InvalidConfig(format!(
                    "symlink {} points outside application root",
                    path.display()
                )));
            }
            if target.is_dir() {
                return Err(FlameError::InvalidConfig(format!(
                    "symlinked directories are not supported: {}",
                    path.display()
                )));
            }
        }
        let relative = path.strip_prefix(root).map_err(|e| {
            FlameError::Internal(format!("failed to relativize {}: {}", path.display(), e))
        })?;
        entries.push(relative.to_path_buf());
    }

    Ok(())
}

fn collect_entries(
    root: &Path,
    relative: &Path,
    canonical_root: &Path,
    entries: &mut Vec<PathBuf>,
) -> Result<(), FlameError> {
    let dir = root.join(relative);
    let mut children = Vec::new();
    for entry in fs::read_dir(&dir).map_err(|e| {
        FlameError::Internal(format!("failed to read directory {}: {}", dir.display(), e))
    })? {
        let entry = entry
            .map_err(|e| FlameError::Internal(format!("failed to read directory entry: {}", e)))?;
        children.push(entry.path());
    }
    children.sort();

    for child in children {
        let name = child.file_name().ok_or_else(|| {
            FlameError::Internal(format!("invalid path in package: {}", child.display()))
        })?;
        let child_relative = relative.join(name);
        let metadata = fs::symlink_metadata(&child).map_err(|e| {
            FlameError::Internal(format!("failed to stat {}: {}", child.display(), e))
        })?;

        if metadata.file_type().is_symlink() {
            let target = child.canonicalize().map_err(|e| {
                FlameError::InvalidConfig(format!(
                    "failed to resolve symlink {}: {}",
                    child.display(),
                    e
                ))
            })?;
            if !target.starts_with(canonical_root) {
                return Err(FlameError::InvalidConfig(format!(
                    "symlink {} points outside application root",
                    child.display()
                )));
            }
            if target.is_dir() {
                return Err(FlameError::InvalidConfig(format!(
                    "symlinked directories are not supported: {}",
                    child.display()
                )));
            }
            entries.push(child_relative);
        } else if metadata.is_dir() {
            collect_entries(root, &child_relative, canonical_root, entries)?;
        } else if metadata.is_file() {
            entries.push(child_relative);
        }
    }

    Ok(())
}

fn append_file(
    builder: &mut Builder<GzEncoder<fs::File>>,
    canonical_root: &Path,
    src: &Path,
    relative: &Path,
) -> Result<(), FlameError> {
    let actual_src = if fs::symlink_metadata(src)
        .map_err(|e| FlameError::Internal(format!("failed to stat {}: {}", src.display(), e)))?
        .file_type()
        .is_symlink()
    {
        src.canonicalize().map_err(|e| {
            FlameError::InvalidConfig(format!(
                "failed to resolve symlink {}: {}",
                src.display(),
                e
            ))
        })?
    } else {
        src.to_path_buf()
    };

    if !actual_src
        .canonicalize()
        .unwrap_or(actual_src.clone())
        .starts_with(canonical_root)
    {
        return Err(FlameError::InvalidConfig(format!(
            "{} is outside application root",
            src.display()
        )));
    }

    let mut file = fs::File::open(&actual_src).map_err(|e| {
        FlameError::Internal(format!("failed to open {}: {}", actual_src.display(), e))
    })?;
    let metadata = file.metadata().map_err(|e| {
        FlameError::Internal(format!("failed to stat {}: {}", actual_src.display(), e))
    })?;

    let mut header = Header::new_gnu();
    header.set_entry_type(EntryType::Regular);
    header.set_size(metadata.len());
    header.set_mtime(0);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mode(file_mode(&metadata));
    header.set_cksum();

    builder
        .append_data(&mut header, relative, &mut file)
        .map_err(|e| FlameError::Internal(format!("failed to append package file: {}", e)))?;
    Ok(())
}

fn file_mode(metadata: &fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o777
    }

    #[cfg(not(unix))]
    {
        if metadata.permissions().readonly() {
            0o444
        } else {
            0o644
        }
    }
}

fn unpack_tar_gz(package: &Path, dest: &Path) -> Result<(), FlameError> {
    let file = fs::File::open(package).map_err(|e| {
        FlameError::Internal(format!(
            "failed to open package {}: {}",
            package.display(),
            e
        ))
    })?;
    let decoder = GzDecoder::new(file);
    let mut archive = Archive::new(decoder);

    for entry in archive.entries().map_err(|e| {
        FlameError::InvalidConfig(format!(
            "failed to read tar archive {}: {}",
            package.display(),
            e
        ))
    })? {
        let mut entry =
            entry.map_err(|e| FlameError::InvalidConfig(format!("invalid tar entry: {}", e)))?;
        let path = entry
            .path()
            .map_err(|e| FlameError::InvalidConfig(format!("invalid tar path: {}", e)))?;
        let path = path.to_path_buf();
        validate_relative_path(&path)?;

        let entry_type = entry.header().entry_type();
        if !(entry_type.is_dir() || entry_type.is_file()) {
            return Err(FlameError::InvalidConfig(format!(
                "unsupported tar entry type for {}",
                path.display()
            )));
        }

        entry.unpack_in(dest).map_err(|e| {
            FlameError::InvalidConfig(format!("failed to unpack {}: {}", path.display(), e))
        })?;
    }

    Ok(())
}

fn validate_relative_path(path: &Path) -> Result<(), FlameError> {
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            _ => {
                return Err(FlameError::InvalidConfig(format!(
                    "unsafe archive path: {}",
                    path.display()
                )))
            }
        }
    }
    Ok(())
}

fn select_detection_root(root: &Path) -> Result<PathBuf, FlameError> {
    let entries: Vec<PathBuf> = fs::read_dir(root)
        .map_err(|e| FlameError::Internal(format!("failed to read extraction root: {}", e)))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect();

    if entries.len() == 1 && entries[0].is_dir() {
        Ok(entries[0].clone())
    } else {
        Ok(root.to_path_buf())
    }
}

fn sha256_file(path: &Path) -> Result<String, FlameError> {
    let mut file = fs::File::open(path).map_err(|e| {
        FlameError::Internal(format!("failed to open package {}: {}", path.display(), e))
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 1024 * 1024];

    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|e| FlameError::Internal(format!("failed to read package: {}", e)))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

#[cfg(test)]
mod tests;
