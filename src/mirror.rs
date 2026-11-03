//! A per-run folder on the host that stands in for the caller's filesystem.
//!
//! A caller path such as `/home/pod/out/shot.png` lives at
//! `<root>/home/pod/out/shot.png` on the host. The preamble applies the same
//! mapping inside the script, so the script keeps using caller paths.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use sha2::{Digest, Sha256};

use crate::protocol::FilePayload;

#[derive(Debug, thiserror::Error)]
pub enum MirrorError {
    #[error("path must be absolute: {0}")]
    NotAbsolute(String),
    #[error("path has no file name: {0}")]
    NoFileName(String),
    #[error("file {path} is not valid base64: {source}")]
    BadBase64 {
        path: String,
        source: base64::DecodeError,
    },
    #[error("{context}: {source}")]
    Io { context: String, source: io::Error },
}

fn io_err(context: impl Into<String>) -> impl FnOnce(io::Error) -> MirrorError {
    let context = context.into();
    move |source| MirrorError::Io { context, source }
}

/// Map an absolute caller path to its place under `root`, resolving `.` and
/// `..` lexically the way `path.posix.normalize` does in the preamble.
pub fn host_path(root: &Path, caller_path: &str) -> Result<PathBuf, MirrorError> {
    if !caller_path.starts_with('/') {
        return Err(MirrorError::NotAbsolute(caller_path.to_owned()));
    }
    let mut parts: Vec<&str> = Vec::new();
    for component in Path::new(caller_path).components() {
        match component {
            Component::Normal(part) => parts.push(part.to_str().unwrap_or_default()),
            Component::ParentDir => {
                parts.pop();
            }
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }
    let mut path = root.to_path_buf();
    path.extend(parts);
    Ok(path)
}

/// The caller path for a file found under `root`.
pub fn caller_path(root: &Path, host: &Path) -> Option<String> {
    let relative = host.strip_prefix(root).ok()?;
    let parts: Vec<&str> = relative
        .components()
        .map(|component| component.as_os_str().to_str())
        .collect::<Option<_>>()?;
    Some(format!("/{}", parts.join("/")))
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// Files the script produced, plus the ones left out because of the size cap.
#[derive(Debug, Default)]
pub struct Changes {
    pub files: Vec<FilePayload>,
    pub skipped: Vec<String>,
}

#[derive(Debug)]
pub struct Mirror {
    root: PathBuf,
    /// Size and digest of each uploaded file, to tell unchanged inputs apart.
    uploaded: HashMap<PathBuf, (u64, [u8; 32])>,
}

impl Mirror {
    /// Create `root` and fill it with the caller's input files and
    /// directories. `root` must not exist yet.
    pub fn create(
        root: PathBuf,
        files: &[FilePayload],
        dirs: &[String],
    ) -> Result<Self, MirrorError> {
        fs::create_dir_all(&root).map_err(io_err(format!("create {}", root.display())))?;
        let mut uploaded = HashMap::new();
        for dir in dirs {
            let path = host_path(&root, dir)?;
            fs::create_dir_all(&path).map_err(io_err(format!("create directory {dir}")))?;
        }
        for file in files {
            let path = host_path(&root, &file.path)?;
            let parent = path
                .parent()
                .filter(|_| path != root)
                .ok_or_else(|| MirrorError::NoFileName(file.path.clone()))?;
            fs::create_dir_all(parent)
                .map_err(io_err(format!("create parent of {}", file.path)))?;
            let bytes =
                BASE64
                    .decode(&file.content_base64)
                    .map_err(|source| MirrorError::BadBase64 {
                        path: file.path.clone(),
                        source,
                    })?;
            fs::write(&path, &bytes).map_err(io_err(format!("write {}", file.path)))?;
            let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
            uploaded.insert(path, (size, digest(&bytes)));
        }
        Ok(Self { root, uploaded })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Every regular file that is new or differs from what was uploaded,
    /// keeping the total under `max_total_bytes`.
    pub fn changes(&self, max_total_bytes: u64) -> Result<Changes, MirrorError> {
        let mut changes = Changes::default();
        let mut total: u64 = 0;
        for entry in walkdir::WalkDir::new(&self.root)
            .follow_links(false)
            .sort_by_file_name()
        {
            let entry = entry.map_err(|err| MirrorError::Io {
                context: format!("scan {}", self.root.display()),
                source: err.into(),
            })?;
            if !entry.file_type().is_file() {
                continue;
            }
            let Some(caller) = caller_path(&self.root, entry.path()) else {
                continue;
            };
            let size = entry
                .metadata()
                .map_err(|err| MirrorError::Io {
                    context: format!("stat {caller}"),
                    source: err.into(),
                })?
                .len();
            // Only an input of the same size can be unchanged; reading it is
            // bounded by the request size limit.
            if let Some((uploaded_size, uploaded_digest)) = self.uploaded.get(entry.path())
                && *uploaded_size == size
            {
                let bytes = fs::read(entry.path()).map_err(io_err(format!("read {caller}")))?;
                if digest(&bytes) == *uploaded_digest {
                    continue;
                }
            }
            // Decide on size before reading so a huge file never enters memory.
            if total.saturating_add(size) > max_total_bytes {
                changes.skipped.push(caller);
                continue;
            }
            let bytes = fs::read(entry.path()).map_err(io_err(format!("read {caller}")))?;
            total += size;
            changes.files.push(FilePayload {
                path: caller,
                content_base64: BASE64.encode(&bytes),
            });
        }
        Ok(changes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(path: &str, content: &[u8]) -> FilePayload {
        FilePayload {
            path: path.to_owned(),
            content_base64: BASE64.encode(content),
        }
    }

    #[test]
    fn maps_caller_paths_lexically_under_root() -> Result<(), MirrorError> {
        let root = Path::new("/srv/run");
        assert_eq!(
            host_path(root, "/home/pod/a.png")?,
            Path::new("/srv/run/home/pod/a.png")
        );
        assert_eq!(
            host_path(root, "/a/../../etc/x")?,
            Path::new("/srv/run/etc/x")
        );
        assert_eq!(host_path(root, "/a//b/./c")?, Path::new("/srv/run/a/b/c"));
        assert!(matches!(
            host_path(root, "rel/x"),
            Err(MirrorError::NotAbsolute(_))
        ));
        Ok(())
    }

    #[test]
    fn caller_path_inverts_host_path() -> Result<(), MirrorError> {
        let root = Path::new("/srv/run");
        let host = host_path(root, "/home/pod/out/x.json")?;
        assert_eq!(
            caller_path(root, &host).as_deref(),
            Some("/home/pod/out/x.json")
        );
        Ok(())
    }

    #[test]
    fn returns_only_new_or_changed_files() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("run");
        let mirror = Mirror::create(
            root.clone(),
            &[
                payload("/in/keep.txt", b"same"),
                payload("/in/edit.txt", b"old"),
            ],
            &["/out".to_owned()],
        )?;
        assert!(root.join("out").is_dir());
        fs::write(root.join("in/edit.txt"), b"new")?;
        fs::write(root.join("out/shot.png"), b"png")?;

        let changes = mirror.changes(u64::MAX)?;
        let paths: Vec<&str> = changes
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect();
        assert_eq!(paths, vec!["/in/edit.txt", "/out/shot.png"]);
        assert_eq!(changes.files[1].content_base64, BASE64.encode(b"png"));
        assert!(changes.skipped.is_empty());

        Ok(())
    }

    #[test]
    fn skips_files_beyond_the_size_cap() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("run");
        let mirror = Mirror::create(root.clone(), &[], &["/out".to_owned()])?;
        fs::write(root.join("out/a.bin"), [0_u8; 6])?;
        fs::write(root.join("out/b.bin"), [0_u8; 6])?;

        let changes = mirror.changes(10)?;
        assert_eq!(changes.files.len(), 1);
        assert_eq!(changes.skipped, vec!["/out/b.bin"]);
        Ok(())
    }

    #[test]
    fn rejects_invalid_base64() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let bad = FilePayload {
            path: "/x.txt".to_owned(),
            content_base64: "!!!".to_owned(),
        };
        let result = Mirror::create(temp.path().join("run"), &[bad], &[]);
        assert!(matches!(result, Err(MirrorError::BadBase64 { .. })));
        Ok(())
    }
}
