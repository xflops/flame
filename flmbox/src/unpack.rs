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

//! Hardened extraction of OCI layers into a flattened root filesystem.
//!
//! Every entry is confined to the root: paths must be relative and free of
//! `..`, parent directories are resolved component by component without ever
//! following a symlink out of the root, hardlinks must point inside the root
//! and whiteouts are applied instead of written.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::{chown, lchown, symlink, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use flame_rs::apis::FlameError;

const MAX_SYMLINKS: usize = 40;
const WHITEOUT_PREFIX: &str = ".wh.";
const OPAQUE_MARKER: &str = ".wh..opq";

fn unsafe_entry(msg: impl std::fmt::Display) -> FlameError {
    FlameError::InvalidConfig(format!("unsafe layer entry: {msg}"))
}

fn io_err(what: &str, path: &Path) -> impl FnOnce(io::Error) -> FlameError {
    let what = format!("{what} {}", path.display());
    move |e| FlameError::Internal(format!("{what}: {e}"))
}

/// Applies the layer files in order on top of `root`, deleting each layer
/// file once applied. `max_bytes` caps the total size of the extracted files.
pub(crate) fn unpack_layers(
    root: &Path,
    layers: &[PathBuf],
    max_bytes: u64,
) -> Result<(), FlameError> {
    let mut unpacker = Unpacker::new(root.to_path_buf(), max_bytes);
    for layer in layers {
        let reader = open_layer(layer)?;
        unpacker.apply_layer(reader)?;
        fs::remove_file(layer).map_err(io_err("remove", layer))?;
    }
    unpacker.finish()
}

/// Opens a layer, detecting gzip, zstd or an uncompressed tar by magic bytes.
fn open_layer(path: &Path) -> Result<Box<dyn Read>, FlameError> {
    let ctx = io_err("open layer", path);
    let mut file = fs::File::open(path).map_err(ctx)?;
    let mut magic = Vec::with_capacity(4);
    (&mut file)
        .take(4)
        .read_to_end(&mut magic)
        .map_err(io_err("read layer", path))?;
    file.seek(SeekFrom::Start(0))
        .map_err(io_err("seek layer", path))?;

    if magic.starts_with(&[0x1f, 0x8b]) {
        Ok(Box::new(flate2::read::MultiGzDecoder::new(
            io::BufReader::new(file),
        )))
    } else if magic.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        let decoder = zstd::stream::read::Decoder::new(file).map_err(io_err("zstd layer", path))?;
        Ok(Box::new(decoder))
    } else {
        Ok(Box::new(io::BufReader::new(file)))
    }
}

/// Splits a tar path into normal components; `.` is skipped, absolute paths
/// and `..` are refused.
fn normalize(path: &Path) -> Result<Vec<OsString>, FlameError> {
    let mut out = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(c) => out.push(c.to_os_string()),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(unsafe_entry(format!("'..' in {}", path.display())));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(unsafe_entry(format!("absolute path {}", path.display())));
            }
        }
    }
    Ok(out)
}

struct Unpacker {
    root: PathBuf,
    max_bytes: u64,
    written: u64,
    /// Cleared on the first permission error: the process cannot chown.
    chown: bool,
    /// Final modes of directories, applied by `finish` so that read-only
    /// directories do not block extraction.
    dir_modes: BTreeMap<PathBuf, u32>,
    /// Paths created by the current layer; an opaque marker keeps them.
    created: HashSet<PathBuf>,
}

impl Unpacker {
    fn new(root: PathBuf, max_bytes: u64) -> Self {
        Self {
            root,
            max_bytes,
            written: 0,
            chown: true,
            dir_modes: BTreeMap::new(),
            created: HashSet::new(),
        }
    }

    fn apply_layer<R: Read>(&mut self, reader: R) -> Result<(), FlameError> {
        self.created.clear();
        let mut archive = tar::Archive::new(reader);
        let entries = archive
            .entries()
            .map_err(|e| FlameError::Internal(format!("read layer: {e}")))?;
        for entry in entries {
            let mut entry = entry.map_err(|e| FlameError::Internal(format!("read layer: {e}")))?;
            self.apply_entry(&mut entry)?;
        }
        Ok(())
    }

    fn finish(self) -> Result<(), FlameError> {
        let mut dirs: Vec<_> = self.dir_modes.iter().collect();
        dirs.sort_by_key(|(path, _)| std::cmp::Reverse(path.components().count()));
        for (rel, mode) in dirs {
            let path = self.root.join(rel);
            if fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir()) {
                fs::set_permissions(&path, fs::Permissions::from_mode(*mode))
                    .map_err(io_err("chmod", &path))?;
            }
        }
        Ok(())
    }

    fn abs(&self, rel: &[OsString]) -> PathBuf {
        let mut path = self.root.clone();
        path.extend(rel);
        path
    }

    /// Resolves a directory chroot-style: symlinks inside the root are
    /// followed (absolute targets are relative to the root) but the result
    /// can never leave it; a `..` that would climb above the root is refused.
    /// Returns the components of the resolved real directory, or `None` when
    /// it does not exist and `create` is false.
    fn resolve_dir(
        &self,
        components: &[OsString],
        create: bool,
    ) -> Result<Option<Vec<OsString>>, FlameError> {
        let mut current: Vec<OsString> = Vec::new();
        let mut pending: VecDeque<OsString> = components.iter().cloned().collect();
        let mut links = 0;

        while let Some(component) = pending.pop_front() {
            if component == "." {
                continue;
            }
            if component == ".." {
                if current.pop().is_none() {
                    return Err(unsafe_entry("path escapes the root"));
                }
                continue;
            }
            let path = self.abs(&current).join(&component);
            match fs::symlink_metadata(&path) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    links += 1;
                    if links > MAX_SYMLINKS {
                        return Err(unsafe_entry("too many levels of symlinks"));
                    }
                    let target = fs::read_link(&path).map_err(io_err("readlink", &path))?;
                    if target.is_absolute() {
                        current.clear();
                    }
                    for part in target.components().rev() {
                        match part {
                            Component::Normal(c) => pending.push_front(c.to_os_string()),
                            Component::ParentDir => pending.push_front("..".into()),
                            _ => {}
                        }
                    }
                }
                Ok(meta) if meta.is_dir() => current.push(component),
                Ok(_) => {
                    return Err(unsafe_entry(format!(
                        "{} is not a directory",
                        path.display()
                    )))
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    if !create {
                        return Ok(None);
                    }
                    fs::create_dir(&path).map_err(io_err("mkdir", &path))?;
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
                        .map_err(io_err("chmod", &path))?;
                    current.push(component);
                }
                Err(e) => return Err(io_err("stat", &path)(e)),
            }
        }
        Ok(Some(current))
    }

    /// Removes whatever exists at `rel` (without following symlinks).
    fn remove(&mut self, rel: &Path) -> Result<(), FlameError> {
        let path = self.root.join(rel);
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() => {
                fs::remove_dir_all(&path).map_err(io_err("remove", &path))?
            }
            Ok(_) => fs::remove_file(&path).map_err(io_err("remove", &path))?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_err("stat", &path)(e)),
        }
        self.dir_modes.retain(|p, _| !p.starts_with(rel));
        Ok(())
    }

    fn make_opaque(&mut self, dir: &[OsString]) -> Result<(), FlameError> {
        let path = self.abs(dir);
        let children = fs::read_dir(&path).map_err(io_err("read_dir", &path))?;
        for child in children {
            let child = child.map_err(io_err("read_dir", &path))?;
            let rel: PathBuf = dir.iter().cloned().chain([child.file_name()]).collect();
            if !self.created.contains(&rel) {
                self.remove(&rel)?;
            }
        }
        Ok(())
    }

    fn apply_entry<R: Read>(&mut self, entry: &mut tar::Entry<'_, R>) -> Result<(), FlameError> {
        let kind = entry.header().entry_type();
        if !(kind.is_dir() || kind.is_file() || kind.is_symlink() || kind.is_hard_link()) {
            tracing::debug!("skipping layer entry of type {:?}", kind);
            return Ok(());
        }

        let path = entry
            .path()
            .map_err(|e| FlameError::Internal(format!("read layer path: {e}")))?
            .into_owned();
        let components = normalize(&path)?;
        let Some((name, parent)) = components.split_last() else {
            return Ok(());
        };

        if let Some(target) = name.to_str().and_then(|n| n.strip_prefix(WHITEOUT_PREFIX)) {
            return self.apply_whiteout(target, parent);
        }

        let parent = self
            .resolve_dir(parent, true)?
            .ok_or_else(|| FlameError::Internal("parent directory vanished".to_string()))?;
        let rel: PathBuf = parent.iter().chain([name]).collect();
        let target = self.root.join(&rel);

        if kind.is_dir() {
            match fs::symlink_metadata(&target) {
                Ok(meta) if meta.is_dir() => {}
                Ok(_) => {
                    self.remove(&rel)?;
                    fs::create_dir(&target).map_err(io_err("mkdir", &target))?;
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    fs::create_dir(&target).map_err(io_err("mkdir", &target))?;
                }
                Err(e) => return Err(io_err("stat", &target)(e)),
            }
            let mode = entry.header().mode().unwrap_or(0o755) & 0o7777;
            fs::set_permissions(&target, fs::Permissions::from_mode(mode | 0o700))
                .map_err(io_err("chmod", &target))?;
            self.chown_entry(&target, entry.header(), false)?;
            self.dir_modes.insert(rel.clone(), mode);
        } else if kind.is_file() {
            self.remove(&rel)?;
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&target)
                .map_err(io_err("create", &target))?;
            let remaining = self.max_bytes.saturating_sub(self.written);
            let copied = io::copy(&mut entry.by_ref().take(remaining + 1), &mut file)
                .map_err(io_err("write", &target))?;
            if copied > remaining {
                return Err(FlameError::InvalidConfig(format!(
                    "image content exceeds the {} byte limit",
                    self.max_bytes
                )));
            }
            self.written += copied;
            self.chown_entry(&target, entry.header(), false)?;
            let mode = entry.header().mode().unwrap_or(0o644) & 0o7777;
            fs::set_permissions(&target, fs::Permissions::from_mode(mode))
                .map_err(io_err("chmod", &target))?;
        } else if kind.is_symlink() {
            let link = entry
                .link_name()
                .map_err(|e| FlameError::Internal(format!("read link name: {e}")))?
                .ok_or_else(|| unsafe_entry(format!("symlink {} has no target", path.display())))?
                .into_owned();
            self.remove(&rel)?;
            symlink(&link, &target).map_err(io_err("symlink", &target))?;
            self.chown_entry(&target, entry.header(), true)?;
        } else {
            let link = entry
                .link_name()
                .map_err(|e| FlameError::Internal(format!("read link name: {e}")))?
                .ok_or_else(|| unsafe_entry(format!("hardlink {} has no target", path.display())))?
                .into_owned();
            let link = normalize(&link)?;
            let Some((link_name, link_parent)) = link.split_last() else {
                return Err(unsafe_entry("empty hardlink target"));
            };
            let link_parent = self.resolve_dir(link_parent, false)?.ok_or_else(|| {
                unsafe_entry(format!("hardlink target of {} missing", path.display()))
            })?;
            let source = self.abs(&link_parent).join(link_name);
            match fs::symlink_metadata(&source) {
                Ok(meta) if meta.is_file() => {}
                Ok(_) => {
                    return Err(unsafe_entry(format!(
                        "hardlink target {} is not a regular file",
                        source.display()
                    )))
                }
                Err(e) => return Err(io_err("stat hardlink target", &source)(e)),
            }
            self.remove(&rel)?;
            fs::hard_link(&source, &target).map_err(io_err("hardlink", &target))?;
        }

        self.created.insert(rel);
        Ok(())
    }

    fn apply_whiteout(&mut self, target: &str, parent: &[OsString]) -> Result<(), FlameError> {
        if target.is_empty() || target == "." || target == ".." {
            return Err(unsafe_entry(format!("invalid whiteout '.wh.{target}'")));
        }
        let Some(dir) = self.resolve_dir(parent, false)? else {
            return Ok(());
        };
        if target == OPAQUE_MARKER {
            self.make_opaque(&dir)
        } else {
            let rel: PathBuf = dir
                .iter()
                .cloned()
                .chain([OsString::from(target)])
                .collect();
            self.remove(&rel)
        }
    }

    /// Sets ownership when permitted; a process that is not root skips it.
    fn chown_entry(
        &mut self,
        path: &Path,
        header: &tar::Header,
        is_symlink: bool,
    ) -> Result<(), FlameError> {
        if !self.chown {
            return Ok(());
        }
        let uid = header.uid().ok().and_then(|v| u32::try_from(v).ok());
        let gid = header.gid().ok().and_then(|v| u32::try_from(v).ok());
        let (Some(uid), Some(gid)) = (uid, gid) else {
            return Ok(());
        };
        let result = if is_symlink {
            lchown(path, Some(uid), Some(gid))
        } else {
            chown(path, Some(uid), Some(gid))
        };
        match result {
            Ok(()) => Ok(()),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::PermissionDenied | io::ErrorKind::InvalidInput
                ) =>
            {
                self.chown = false;
                Ok(())
            }
            Err(e) => Err(io_err("chown", path)(e)),
        }
    }
}
