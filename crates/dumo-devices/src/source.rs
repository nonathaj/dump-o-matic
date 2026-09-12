//! A tree of files to read content out of, wherever it physically lives.
//!
//! Two backends exist because two situations are both legitimate. A device this tool
//! opens itself is parsed by [`Fat32Source`], which needs no mount and cannot write. A
//! device the operator has already mounted — or a directory tree copied off one — is read
//! by [`DirSource`]. Everything above this module is written against the trait, so the
//! Xbox 360 reader does not know or care which it is working on.

use crate::{fat32, DeviceError, Result};
use std::path::{Path, PathBuf};

/// One entry in a content directory.
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

/// Positioned reads over a file. Deliberately not `Seek + Read`: every consumer here
/// addresses an image by absolute offset, and a shared cursor would make that a hazard.
pub trait RandomRead {
    fn size(&self) -> u64;
    fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<()>;
}

/// A readable tree of files.
pub trait ContentSource {
    /// How to describe this source to a human, e.g. `/dev/sdd`.
    fn describe(&self) -> String;

    /// Extra provenance worth recording, such as a FAT32 OEM name.
    fn medium_hint(&self) -> Option<String> {
        None
    }

    /// Total size of the medium, when known.
    fn medium_bytes(&self) -> Option<u64> {
        None
    }

    fn list(&self, path: &str) -> Result<Vec<Entry>>;

    fn open(&self, path: &str) -> Result<Box<dyn RandomRead + '_>>;

    /// Read a whole small file, refusing anything larger than `limit`.
    fn read_small(&self, path: &str, limit: u64) -> Result<Vec<u8>> {
        let f = self.open(path)?;
        if f.size() > limit {
            return Err(DeviceError::Corrupt {
                path: format!("{}:{path}", self.describe()),
                detail: format!("expected at most {limit} bytes, found {}", f.size()),
            });
        }
        let mut buf = vec![0u8; f.size() as usize];
        f.read_at(&mut buf, 0)?;
        Ok(buf)
    }

    /// Whether a path exists, used by layout detection.
    fn exists(&self, path: &str) -> bool {
        let (parent, name) = match path.rsplit_once('/') {
            Some((p, n)) => (p, n),
            None => ("", path),
        };
        self.list(parent)
            .map(|es| es.iter().any(|e| e.name.eq_ignore_ascii_case(name)))
            .unwrap_or(false)
    }
}

/// A FAT32 volume this tool parses itself, without mounting it.
pub struct Fat32Source {
    fs: fat32::Fat32,
}

impl Fat32Source {
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            fs: fat32::Fat32::open(path)?,
        })
    }
}

impl ContentSource for Fat32Source {
    fn describe(&self) -> String {
        self.fs.source().to_string()
    }

    fn medium_hint(&self) -> Option<String> {
        let oem = self.fs.oem_name.clone();
        (!oem.is_empty()).then(|| format!("FAT32, OEM name {oem:?}"))
    }

    fn medium_bytes(&self) -> Option<u64> {
        Some(self.fs.volume_bytes())
    }

    fn list(&self, path: &str) -> Result<Vec<Entry>> {
        let cluster = if path.trim_matches('/').is_empty() {
            self.fs.root_cluster()
        } else {
            let e = self.fs.resolve(path)?;
            if !e.is_dir() {
                return Err(DeviceError::NotFound {
                    path: format!("{}:{path} is not a directory", self.describe()),
                });
            }
            e.first_cluster
        };
        Ok(self
            .fs
            .read_dir(cluster)?
            .into_iter()
            .map(|e| Entry {
                is_dir: e.is_dir(),
                name: e.name,
                size: e.size,
            })
            .collect())
    }

    fn open(&self, path: &str) -> Result<Box<dyn RandomRead + '_>> {
        Ok(Box::new(self.fs.open_file(path)?))
    }
}

impl RandomRead for fat32::Fat32File<'_> {
    fn size(&self) -> u64 {
        self.size()
    }
    fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<()> {
        self.read_at(buf, offset)
    }
}

/// A directory tree in the host filesystem: an already-mounted device, or a copy of one.
pub struct DirSource {
    root: PathBuf,
}

impl DirSource {
    pub fn open(root: &Path) -> Result<Self> {
        if !root.is_dir() {
            return Err(DeviceError::NotFound {
                path: root.display().to_string(),
            });
        }
        Ok(Self {
            root: root.to_path_buf(),
        })
    }

    /// Join a source-relative path, refusing anything that would escape the root.
    ///
    /// Paths here come from content on removable media, which is not trusted input: a
    /// `..` component in a package name must not be able to reach outside the tree.
    fn join(&self, path: &str) -> Result<PathBuf> {
        let mut out = self.root.clone();
        for part in path.split('/').filter(|p| !p.is_empty()) {
            if part == ".." || part == "." || part.contains('\0') {
                return Err(DeviceError::Corrupt {
                    path: path.to_string(),
                    detail: format!("path component {part:?} is not allowed"),
                });
            }
            out.push(part);
        }
        Ok(out)
    }
}

impl ContentSource for DirSource {
    fn describe(&self) -> String {
        self.root.display().to_string()
    }

    fn list(&self, path: &str) -> Result<Vec<Entry>> {
        let dir = self.join(path)?;
        let mut out = Vec::new();
        let entries = std::fs::read_dir(&dir).map_err(|e| DeviceError::Io {
            path: dir.display().to_string(),
            source: e,
        })?;
        for e in entries.flatten() {
            let Ok(meta) = e.metadata() else { continue };
            out.push(Entry {
                name: e.file_name().to_string_lossy().to_string(),
                is_dir: meta.is_dir(),
                size: meta.len(),
            });
        }
        Ok(out)
    }

    fn open(&self, path: &str) -> Result<Box<dyn RandomRead + '_>> {
        let p = self.join(path)?;
        let file = std::fs::File::open(&p).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => DeviceError::NotFound {
                path: p.display().to_string(),
            },
            std::io::ErrorKind::PermissionDenied => DeviceError::PermissionDenied {
                path: p.display().to_string(),
            },
            _ => DeviceError::Io {
                path: p.display().to_string(),
                source: e,
            },
        })?;
        let size = file
            .metadata()
            .map_err(|e| DeviceError::Io {
                path: p.display().to_string(),
                source: e,
            })?
            .len();
        Ok(Box::new(HostFile {
            file,
            size,
            path: p.display().to_string(),
        }))
    }
}

struct HostFile {
    file: std::fs::File,
    size: u64,
    path: String,
}

impl RandomRead for HostFile {
    fn size(&self) -> u64 {
        self.size
    }

    fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<()> {
        use std::os::unix::fs::FileExt;
        self.file
            .read_exact_at(buf, offset)
            .map_err(|e| DeviceError::Io {
                path: self.path.clone(),
                source: e,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dir_source_refuses_to_escape_its_root() {
        let base = std::env::temp_dir().join(format!(
            "dumo-src-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(base.join("Content")).unwrap();
        let s = DirSource::open(&base).unwrap();
        assert!(s.join("Content/x").is_ok());
        assert!(s.join("../../etc/passwd").is_err());
        assert!(s.join("Content/../../etc").is_err());
        std::fs::remove_dir_all(&base).ok();
    }
}
