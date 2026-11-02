//! A pack is a folder or a `.zip` whose root holds a folder named `shaders`; everything else at
//! the root (licences, readmes, decoy folders) is ignored (spec 1.1). Every path a pack uses is
//! relative to that `shaders` folder, written with `/`.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::{self, Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use zip::ZipArchive;

const ROOT: &str = "shaders";

/// Limits apply to the entire archive, not just `shaders/`: ZIPs can contain decoys, duplicate
/// entries and highly compressible files. Keep one ordinary high-resolution texture affordable
/// while refusing archives whose declared expansion could exhaust a player's memory.
#[derive(Clone, Copy)]
struct ZipLimits {
    archive: u64,
    entries: usize,
    compressed_entry: u64,
    compressed_total: u64,
    expanded_entry: u64,
    expanded_total: u64,
}

const ZIP_LIMITS: ZipLimits = ZipLimits {
    archive: 256 * 1024 * 1024,
    entries: 8192,
    compressed_entry: 128 * 1024 * 1024,
    compressed_total: 256 * 1024 * 1024,
    expanded_entry: 64 * 1024 * 1024,
    expanded_total: 512 * 1024 * 1024,
};

pub struct Pack {
    name: String,
    files: Files,
    /// The values the player chose for the pack's options, applied to its sources as they are
    /// read (`options::apply`).
    chosen: BTreeMap<String, String>,
}

enum Files {
    /// The pack's `shaders` folder.
    Folder(PathBuf),
    /// The archive and its entries under `shaders/`, by their path inside that folder.
    Zip {
        archive: ZipArchive<Cursor<Arc<[u8]>>>,
        entries: BTreeMap<String, usize>,
        expanded_entry_limit: u64,
    },
}

#[derive(Debug)]
pub enum PackError {
    Io(io::Error),
    Zip(String),
    Limit(String),
    /// A readable folder or archive with no `shaders` folder at its root.
    NoShadersFolder,
}

impl fmt::Display for PackError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::Zip(e) => write!(f, "not a readable zip: {e}"),
            Self::Limit(e) => write!(f, "shader zip exceeds safety limit: {e}"),
            Self::NoShadersFolder => f.write_str("no shaders folder at the root of the pack"),
        }
    }
}

impl std::error::Error for PackError {}

impl From<io::Error> for PackError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl Pack {
    /// Opens a pack folder or a `.zip` archive.
    pub fn open(path: &Path) -> Result<Self, PackError> {
        let name = path
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let files = if path.is_dir() {
            let root = path.join(ROOT);
            if !root.is_dir() {
                return Err(PackError::NoShadersFolder);
            }
            Files::Folder(root)
        } else {
            // Checking the metadata alone is racy: also cap the actual bytes read in case the
            // file changes while loading it.
            let file = fs::File::open(path)?;
            if file.metadata()?.len() > ZIP_LIMITS.archive {
                return Err(PackError::Limit(format!(
                    "archive exceeds {} bytes",
                    ZIP_LIMITS.archive
                )));
            }
            let mut bytes = Vec::new();
            file.take(ZIP_LIMITS.archive + 1).read_to_end(&mut bytes)?;
            Self::zip_files(bytes)?
        };
        Ok(Self {
            name,
            files,
            chosen: BTreeMap::new(),
        })
    }

    /// An archive held in memory; `name` is what [`Pack::name`] returns.
    pub fn from_zip_bytes(name: &str, bytes: Vec<u8>) -> Result<Self, PackError> {
        let files = Self::zip_files(bytes)?;
        Ok(Self {
            name: name.to_owned(),
            files,
            chosen: BTreeMap::new(),
        })
    }

    fn zip_files(bytes: Vec<u8>) -> Result<Files, PackError> {
        Self::zip_files_with_limits(bytes, ZIP_LIMITS)
    }

    fn zip_files_with_limits(bytes: Vec<u8>, limits: ZipLimits) -> Result<Files, PackError> {
        if bytes.len() as u64 > limits.archive {
            return Err(PackError::Limit(format!(
                "archive exceeds {} bytes",
                limits.archive
            )));
        }
        let mut archive = ZipArchive::new(Cursor::new(Arc::from(bytes)))
            .map_err(|e| PackError::Zip(e.to_string()))?;
        if archive.len() > limits.entries {
            return Err(PackError::Limit(format!(
                "{} entries exceeds the {}-entry limit",
                archive.len(),
                limits.entries
            )));
        }
        let mut entries = BTreeMap::new();
        let (mut compressed_total, mut expanded_total) = (0u64, 0u64);
        for i in 0..archive.len() {
            let file = archive
                .by_index_raw(i)
                .map_err(|e| PackError::Zip(e.to_string()))?;
            let (compressed, expanded) = (file.compressed_size(), file.size());
            if compressed > limits.compressed_entry || expanded > limits.expanded_entry {
                return Err(PackError::Limit(format!(
                    "entry {i} is too large ({compressed} compressed, {expanded} expanded bytes)"
                )));
            }
            compressed_total = compressed_total.checked_add(compressed).ok_or_else(|| {
                PackError::Limit("compressed entry sizes overflow".to_owned())
            })?;
            expanded_total = expanded_total.checked_add(expanded).ok_or_else(|| {
                PackError::Limit("expanded entry sizes overflow".to_owned())
            })?;
            if compressed_total > limits.compressed_total || expanded_total > limits.expanded_total {
                return Err(PackError::Limit(format!(
                    "total entries exceed {} compressed or {} expanded bytes",
                    limits.compressed_total, limits.expanded_total
                )));
            }
            drop(file);
            let Some(name) = archive.name_for_index(i) else {
                continue;
            };
            let name = name
                .map_err(|e| PackError::Zip(e.to_string()))?
                .replace('\\', "/");
            if let Some(inner) = name.strip_prefix(ROOT).and_then(|n| n.strip_prefix('/'))
                && !inner.is_empty()
                && !inner.ends_with('/')
            {
                entries.insert(inner.to_owned(), i);
            }
        }
        if entries.is_empty() {
            return Err(PackError::NoShadersFolder);
        }
        Ok(Files::Zip {
            archive,
            entries,
            expanded_entry_limit: limits.expanded_entry,
        })
    }

    /// The pack with options at the values in `chosen` (name to value, `true` or `false` for a
    /// switch) instead of their defaults. A value the option doesn't list (a file edited by
    /// hand) is left out.
    pub fn with_chosen(mut self, chosen: BTreeMap<String, String>) -> Self {
        let options = crate::options(&self);
        self.chosen = chosen
            .into_iter()
            .filter(|(name, value)| options.get(name).is_some_and(|o| o.values.contains(value)))
            .collect();
        self
    }

    /// The folder or file name the pack was opened from (`BSL.zip`).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The bytes of a file, by its path inside `shaders` (`lang/en_us.lang`). `None` when it
    /// doesn't exist or the path leaves the folder.
    pub fn read(&self, path: &str) -> Option<Vec<u8>> {
        let path = normalize(path)?;
        match &self.files {
            Files::Folder(root) => fs::read(root.join(&path)).ok(),
            Files::Zip {
                archive,
                entries,
                expanded_entry_limit,
            } => {
                // A clone shares the bytes and the central directory; reading needs `&mut`.
                let mut archive = archive.clone();
                let entry = archive.by_index(*entries.get(&path)?).ok()?;
                // Do not trust ZIP's declared length when inflating. `take` also bounds a
                // corrupt entry that produces more bytes than its central directory claims.
                if entry.size() > *expanded_entry_limit {
                    return None;
                }
                let mut out = Vec::new();
                out.try_reserve(usize::try_from(entry.size()).ok()?).ok()?;
                entry.take(expanded_entry_limit + 1).read_to_end(&mut out).ok()?;
                (out.len() as u64 <= *expanded_entry_limit).then_some(out)
            }
        }
    }

    /// A text file, decoded as UTF-8 (invalid bytes replaced) without its byte-order mark, with
    /// the chosen option values in its sources.
    pub fn read_text(&self, path: &str) -> Option<String> {
        let bytes = self.read(path)?;
        let text = String::from_utf8_lossy(&bytes);
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
        let source = !path.ends_with(".properties") && !path.ends_with(".lang");
        Some(if source && !self.chosen.is_empty() {
            crate::options::apply(text, &self.chosen)
        } else {
            text.to_owned()
        })
    }

    pub fn exists(&self, path: &str) -> bool {
        let Some(path) = normalize(path) else {
            return false;
        };
        match &self.files {
            Files::Folder(root) => root.join(path).is_file(),
            Files::Zip { entries, .. } => entries.contains_key(&path),
        }
    }

    /// The names of the files directly inside a folder of the pack (`""` for `shaders` itself),
    /// sorted.
    pub fn list(&self, dir: &str) -> Vec<String> {
        let Some(dir) = normalize(dir) else {
            return Vec::new();
        };
        let mut names: Vec<String> = match &self.files {
            Files::Folder(root) => fs::read_dir(root.join(&dir))
                .into_iter()
                .flatten()
                .flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect(),
            Files::Zip { entries, .. } => {
                let prefix = if dir.is_empty() {
                    String::new()
                } else {
                    format!("{dir}/")
                };
                entries
                    .keys()
                    .filter_map(|k| k.strip_prefix(&prefix))
                    .filter(|rest| !rest.contains('/'))
                    .map(str::to_owned)
                    .collect()
            }
        };
        names.sort();
        names
    }

    /// Every file of the pack, by its path inside `shaders`, sorted.
    pub fn all_files(&self) -> Vec<String> {
        let mut names = match &self.files {
            Files::Folder(root) => {
                let mut names = Vec::new();
                let mut dirs = vec![String::new()];
                while let Some(dir) = dirs.pop() {
                    for e in fs::read_dir(root.join(&dir))
                        .into_iter()
                        .flatten()
                        .flatten()
                    {
                        let name = e.file_name().to_string_lossy().into_owned();
                        let path = if dir.is_empty() {
                            name
                        } else {
                            format!("{dir}/{name}")
                        };
                        match e.file_type() {
                            Ok(t) if t.is_dir() => dirs.push(path),
                            Ok(t) if t.is_file() => names.push(path),
                            _ => {}
                        }
                    }
                }
                names
            }
            Files::Zip { entries, .. } => entries.keys().cloned().collect(),
        };
        names.sort();
        names
    }
}

/// `a/./b/../c` → `a/c`; `None` for a path that climbs out of `shaders`.
pub(crate) fn normalize(path: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            _ => parts.push(part),
        }
    }
    Some(parts.join("/"))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Write;

    /// A zip of `(path, contents)` pairs, paths from the archive root.
    pub(crate) fn zip<C: AsRef<[u8]>>(files: &[(&str, C)]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (path, contents) in files {
            w.start_file(*path, options).unwrap();
            w.write_all(contents.as_ref()).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    /// A fresh empty folder under the system's temp folder.
    pub(crate) fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("horizon-shaders-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn zip_packs_read_only_the_shaders_folder() {
        let bytes = zip(&[
            ("License.txt", "x"),
            ("shaderFile_A.placebo/shaders.properties", "decoy"),
            ("shaders/shaders.properties", "sun=false"),
            ("shaders/lang/en_US.lang", "a=b"),
            ("shaders/lang/fr_FR.lang", "a=c"),
            ("shaders/lib/common.glsl", ""),
        ]);
        let pack = Pack::from_zip_bytes("P.zip", bytes).unwrap();
        assert_eq!(pack.read_text("shaders.properties").unwrap(), "sun=false");
        assert!(pack.exists("lib/common.glsl"));
        assert!(pack.exists("lib/../shaders.properties"));
        assert!(!pack.exists("../License.txt"));
        assert!(!pack.exists("License.txt"));
        assert_eq!(pack.list("lang"), ["en_US.lang", "fr_FR.lang"]);
        assert_eq!(pack.list(""), ["shaders.properties"]);

        // The disk entry point applies the same checks without reading an unlimited archive.
        let root = temp_dir("zip-on-disk");
        let path = root.join("P.zip");
        fs::write(&path, zip(&[("shaders/shaders.properties", "sun=false")])).unwrap();
        let from_disk = Pack::open(&path).unwrap();
        assert_eq!(from_disk.read_text("shaders.properties"), Some("sun=false".into()));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn archives_without_a_shaders_folder_are_not_packs() {
        let bytes = zip(&[("Pack/shaders/final.fsh", ""), ("readme.txt", "")]);
        assert!(matches!(
            Pack::from_zip_bytes("P.zip", bytes),
            Err(PackError::NoShadersFolder)
        ));
        assert!(matches!(
            Pack::from_zip_bytes("P.zip", b"not a zip".to_vec()),
            Err(PackError::Zip(_))
        ));
    }

    #[test]
    fn zip_limits_reject_archive_entry_and_expansion_budgets() {
        // A highly compressible input expands many times beyond its archive size.
        let bytes = zip(&[
            ("shaders/a.glsl", vec![b'A'; 4096]),
            ("shaders/b.glsl", vec![b'B'; 4096]),
        ]);
        assert!(bytes.len() < 4096);
        let rejected = |limits: ZipLimits| match Pack::zip_files_with_limits(bytes.clone(), limits)
        {
            Err(PackError::Limit(reason)) => reason,
            Err(error) => panic!("expected a size limit, got {error}"),
            Ok(_) => panic!("expected the archive to be rejected"),
        };

        let mut limits = ZIP_LIMITS;
        limits.archive = bytes.len() as u64 - 1;
        assert!(rejected(limits).contains("archive"));

        let mut limits = ZIP_LIMITS;
        limits.entries = 1;
        assert!(rejected(limits).contains("entries"));

        let mut limits = ZIP_LIMITS;
        limits.compressed_entry = 1;
        assert!(rejected(limits).contains("entry"));

        let mut limits = ZIP_LIMITS;
        limits.compressed_total = 1;
        assert!(rejected(limits).contains("total"));

        let mut limits = ZIP_LIMITS;
        limits.expanded_entry = 4095;
        assert!(rejected(limits).contains("entry"));

        let mut limits = ZIP_LIMITS;
        limits.expanded_total = 8191;
        assert!(rejected(limits).contains("total"));

        // Under an applicable budget, data is still read correctly, not truncated.
        let mut limits = ZIP_LIMITS;
        limits.expanded_entry = 4096;
        limits.expanded_total = 8192;
        let Files::Zip {
            archive,
            entries,
            expanded_entry_limit,
        } = Pack::zip_files_with_limits(bytes, limits).unwrap()
        else {
            panic!("expected ZIP files");
        };
        let pack = Pack {
            name: "bounded.zip".into(),
            files: Files::Zip {
                archive,
                entries,
                expanded_entry_limit,
            },
            chosen: BTreeMap::new(),
        };
        assert_eq!(pack.read("a.glsl"), Some(vec![b'A'; 4096]));
    }

    #[test]
    fn duplicate_zip_entries_still_count_toward_the_entry_limit() {
        let bytes = zip(&[
            ("shaders/a.glsl", "first"),
            ("shaders/a.glsl", "second"),
        ]);
        let mut limits = ZIP_LIMITS;
        limits.entries = 1;
        assert!(matches!(
            Pack::zip_files_with_limits(bytes, limits),
            Err(PackError::Limit(_))
        ));
    }

    #[test]
    fn folder_packs_read_the_same_way() {
        let dir = temp_dir("folder");
        fs::create_dir_all(dir.join("shaders/lang")).unwrap();
        fs::write(
            dir.join("shaders/lang/en_us.lang"),
            "\u{feff}option.A=Alpha",
        )
        .unwrap();
        fs::write(dir.join("secret.txt"), "outside").unwrap();
        let pack = Pack::open(&dir).unwrap();
        assert_eq!(pack.read_text("lang/en_us.lang").unwrap(), "option.A=Alpha");
        assert_eq!(pack.list("lang"), ["en_us.lang"]);
        assert!(pack.read("../secret.txt").is_none());
        fs::remove_dir_all(&dir).unwrap();
    }
}
