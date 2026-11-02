//! Step 1b, the jars: for the mods the player asked to port, their own
//! download of each jar into `<work>/jars/`. Only from where the pack itself
//! points (a Modrinth pack's download URLs) or the same file on Modrinth's
//! public CDN (found by its SHA-1), and only kept when its SHA-512 or SHA-1 matches; or
//! copied from a folder the player already has (an installed instance, a
//! downloads folder). CurseForge's own downloads need an API key, so a file
//! only CurseForge has is listed as needing one. `<work>/fetch.json` says
//! what happened to each file.
use crate::{
    import::{Pack, PackFile, sha1_hex, sha512_hex},
    inventory::Record,
    modrinth::{self, Cache},
    pipeline::{Log, read_json, write_json},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashMap,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

/// Hosts a Modrinth pack may download from (the `.mrpack` format's list).
const HOSTS: [&str; 4] = [
    "https://cdn.modrinth.com/",
    "https://github.com/",
    "https://raw.githubusercontent.com/",
    "https://gitlab.com/",
];
/// No mod jar is this large; a bigger answer is not one.
const MAX_JAR: u64 = 512 << 20;

#[derive(Clone, Copy)]
pub(crate) enum Checksum<'a> {
    Sha512(&'a str),
    Sha1(&'a str),
}

impl<'a> Checksum<'a> {
    pub(crate) fn from_file(file: &'a PackFile) -> Result<Self, String> {
        // Prefer the stronger checksum. Do not silently downgrade an invalid SHA-512
        // to SHA-1, even if the pack supplied both.
        if let Some(hash) = file.sha512.as_deref() {
            return Self::validate(hash, 128, "SHA-512").map(|()| Self::Sha512(hash));
        }
        if let Some(hash) = file.sha1.as_deref() {
            return Self::validate(hash, 40, "SHA-1").map(|()| Self::Sha1(hash));
        }
        Err("no SHA-512 or SHA-1 checksum supplied; refusing unverified jar".into())
    }

    fn sha1(hash: Option<&'a str>) -> Result<Self, String> {
        let hash = hash.ok_or("missing SHA-1 checksum; refusing unverified download")?;
        Self::validate(hash, 40, "SHA-1")?;
        Ok(Self::Sha1(hash))
    }

    fn validate(hash: &str, length: usize, kind: &str) -> Result<(), String> {
        if hash.len() != length || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("invalid {kind} checksum: expected {length} hexadecimal digits"));
        }
        Ok(())
    }

    pub(crate) fn verify(self, bytes: &[u8]) -> Result<(), String> {
        let (kind, got, want) = match self {
            Self::Sha512(want) => ("SHA-512", sha512_hex(bytes), want),
            Self::Sha1(want) => ("SHA-1", sha1_hex(bytes), want),
        };
        if got.eq_ignore_ascii_case(want) {
            Ok(())
        } else {
            Err(format!("{kind} {got}, the pack says {want}"))
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum How {
    /// Already in `jars/` (or the imported instance).
    Present,
    /// Downloaded from the pack's URL or Modrinth's CDN, hash checked.
    Downloaded,
    /// Copied from the player's folder.
    Local,
    /// Only on CurseForge, whose downloads are coming soon (its API needs a paid key).
    NeedsKey,
    NotFound,
    Failed,
}

impl How {
    pub fn label(self) -> &'static str {
        match self {
            How::Present => "already here",
            How::Downloaded => "downloaded",
            How::Local => "copied from your folder",
            How::NeedsKey => "CurseForge downloads coming soon",
            How::NotFound => "not found",
            How::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Outcome {
    pub key: String,
    pub file: String,
    pub how: How,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Fetched {
    pub files: Vec<Outcome>,
}

impl Fetched {
    pub fn count(&self, how: How) -> usize {
        self.files.iter().filter(|o| o.how == how).count()
    }
    /// `<work>/fetch.json`, empty when the step has not run.
    pub fn load(work: &Path) -> Self {
        read_json(&work.join("fetch.json")).unwrap_or_default()
    }
}

/// A file name from a pack's index that stays inside `jars/`: one plain part, with no separator
/// of any platform, no drive and not `.` or `..`.
fn plain_file_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', ':', '\0'])
}

/// The jars of the records `want` picks that the work folder lacks.
pub fn fetch(
    work: &Path,
    records: &[Record],
    from: Option<&Path>,
    want: &dyn Fn(&Record) -> bool,
    log: Log,
) -> Result<Fetched, String> {
    let mut pack: Pack = read_json(&work.join("pack.json"))?;
    let cache = Cache::load(&work.join("modrinth.json"));
    let files: HashMap<&str, &PackFile> = pack.files.iter().map(|f| (f.path.as_str(), f)).collect();
    let jars = work.join("jars");
    fs::create_dir_all(&jars).map_err(|e| format!("{}: {e}", jars.display()))?;
    let local = from.map(Local::index);
    let wanted: Vec<&Record> = records.iter().filter(|r| want(r)).collect();
    let mut fetched = Fetched::default();
    let mut adopted: HashMap<String, String> = HashMap::new();
    for (i, r) in wanted.iter().enumerate() {
        let Some(file) = files.get(r.path.as_str()) else {
            continue;
        };
        let name = file.file_name();
        let outcome = |how, detail: String| Outcome {
            key: r.key.clone(),
            file: name.to_owned(),
            how,
            detail,
        };
        if !plain_file_name(name) {
            // A name like `..\..\x.dll` or `C:\x` would write outside jars/ on Windows.
            let o = outcome(How::Failed, format!("{name:?} is not a plain file name"));
            log(format!("{}: {} ({})", o.key, o.how.label(), o.detail));
            fetched.files.push(o);
            continue;
        }
        let dest = jars.join(name);
        let checksum = match Checksum::from_file(file) {
            Ok(checksum) => checksum,
            Err(e) => {
                // CurseForge manifests name projects/files but provide no hash.
                // Only an explicitly selected local instance or external
                // Record.jar can establish trusted provenance, never cached
                // jars/ content or the manifest's remote download URL.
                let hashless_curseforge = file.sha1.is_none()
                    && file.sha512.is_none()
                    && (file.curseforge.is_some() || pack.format == "curseforge");
                let source = r
                    .jar
                    .as_ref()
                    .filter(|p| p.as_path() != dest.as_path() && p.is_file())
                    .cloned()
                    .or_else(|| local.as_ref().and_then(|l| l.find(file)));
                let o = if hashless_curseforge {
                    if let Some(path) = source {
                        match adopt_local(&path, &dest) {
                            Ok(sha1) => {
                                adopted.insert(file.path.clone(), sha1);
                                outcome(How::Local, format!("trusted local source: {}", path.display()))
                            }
                            Err(err) => outcome(How::Failed, format!("{}: {err}", path.display())),
                        }
                    } else if dest.exists() || !file.downloads.is_empty() {
                        outcome(How::Failed, e)
                    } else {
                        outcome(How::NeedsKey, "no trusted local copy; CurseForge files have no checksum".into())
                    }
                } else {
                    outcome(How::Failed, e)
                };
                log(format!("{}: {} ({})", o.key, o.how.label(), o.detail));
                fetched.files.push(o);
                continue;
            }
        };
        let jar = r.jar.as_ref().filter(|p| matches(p, checksum));
        // Inventory may still point to an imported instance jar. A good cache
        // must not make an unverified *different* original jar look trusted,
        // since the carry step will subsequently read the original path.
        let invalid_original = r
            .jar
            .as_ref()
            .is_some_and(|p| p != &dest && !matches(p, checksum));
        let invalid_existing = dest.exists() && !matches(&dest, checksum);
        let o = if invalid_original {
            outcome(How::Failed, "the imported instance jar does not match its checksum".into())
        } else if jar.is_some() || matches(&dest, checksum) {
            outcome(How::Present, String::new())
        } else if let Some(path) = local.as_ref().and_then(|l| l.find(file)) {
            // Verify the actual copied bytes before atomically publishing them;
            // a local file indexed by name may have changed since indexing.
            match fs::File::open(&path)
                .map_err(|e| e.to_string())
                .and_then(|source| store_download_checked(source, checksum, &dest, true))
            {
                Ok(()) => outcome(How::Local, path.display().to_string()),
                Err(e) => outcome(How::Failed, format!("{}: {e}", path.display())),
            }
        } else if let Some(url) = url_of(file, &cache) {
            log(format!("{}/{} {name}", i + 1, wanted.len()));
            match download_verified(&url, file, &dest, true) {
                Ok(()) => outcome(How::Downloaded, url),
                Err(e) => outcome(How::Failed, e),
            }
        } else if invalid_existing || r.jar.is_some() {
            outcome(How::Failed, "existing jar does not match its checksum".into())
        } else if pack.format == "curseforge" || file.curseforge.is_some() {
            outcome(
                How::NeedsKey,
                "on CurseForge only: choose your installed instance, or put the jar in jars/"
                    .into(),
            )
        } else {
            outcome(How::NotFound, "no download link in the pack".into())
        };
        if matches!(o.how, How::Failed | How::NeedsKey | How::NotFound) {
            log(format!("{}: {} ({})", o.key, o.how.label(), o.detail));
        }
        fetched.files.push(o);
    }
    // A user-selected local file has an identifiable digest from now on.
    // Future cached jars must match this SHA-1; they are never trusted by name.
    drop(files);
    if !adopted.is_empty() {
        for file in &mut pack.files {
            if let Some(hash) = adopted.get(&file.path) {
                file.sha1 = Some(hash.clone());
            }
        }
        write_json(&work.join("pack.json"), &pack)?;
    }
    // The loader's own data (its convention tags), from its own repository.
    if let Some((dest, url)) = crate::worldgen::loader_jar(work, &pack.loader, &pack.loader_version)
        .filter(|(dest, _)| !dest.exists())
    {
        let checksum = format!("{url}.sha1");
        let sha1 = crate::net::agent(&checksum)
            .get(&checksum)
            .set("User-Agent", modrinth::AGENT)
            .call()
            .ok()
            .and_then(|r| r.into_string().ok())
            .map(|s| s.trim().to_owned());
        let made = dest.parent().map(fs::create_dir_all);
        match made
            .unwrap_or(Ok(()))
            .map_err(|e| e.to_string())
            .and_then(|()| download(&url, sha1.as_deref(), &dest, true))
        {
            Ok(()) => log(format!("loader: {}", dest.display())),
            Err(e) => log(format!(
                "loader: {e} (structures keep only vanilla biome tags)"
            )),
        }
    }
    write_json(&work.join("fetch.json"), &fetched)?;
    log(format!(
        "jars: {} downloaded, {} copied, {} already here, {} on CurseForge only (CurseForge downloads coming soon), {} not found, {} failed",
        fetched.count(How::Downloaded),
        fetched.count(How::Local),
        fetched.count(How::Present),
        fetched.count(How::NeedsKey),
        fetched.count(How::NotFound),
        fetched.count(How::Failed)
    ));
    Ok(fetched)
}

/// Whether the local file has the exact expected digest (never trust unhashed bytes).
fn matches(path: &Path, checksum: Checksum<'_>) -> bool {
    fs::read(path).is_ok_and(|bytes| checksum.verify(&bytes).is_ok())
}

/// Trust only a file the user explicitly selected, and pin it to a calculated
/// SHA-1 for every future lookup. The bounded copy is published via the same
/// verified staging path as downloaded bytes.
fn adopt_local(source: &Path, dest: &Path) -> Result<String, String> {
    let mut bytes = Vec::new();
    fs::File::open(source)
        .map_err(|e| e.to_string())?
        .take(MAX_JAR + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_JAR {
        return Err("local jar exceeds 512 MiB".into());
    }
    let sha1 = sha1_hex(&bytes);
    store_download_checked(bytes.as_slice(), Checksum::Sha1(&sha1), dest, true)?;
    Ok(sha1)
}

/// The pack's own download link, else the same file on Modrinth.
pub fn url_of(file: &PackFile, cache: &Cache) -> Option<String> {
    let allowed = |u: &&str| HOSTS.iter().any(|h| u.starts_with(h));
    if let Some(u) = file.downloads.iter().map(String::as_str).find(allowed) {
        return Some(u.to_owned());
    }
    let sha1 = file.sha1.as_deref()?;
    cache.versions.get(sha1)?["files"]
        .as_array()?
        .iter()
        .find(|f| f["hashes"]["sha1"].as_str() == Some(sha1))?["url"]
        .as_str()
        .filter(allowed)
        .map(str::to_owned)
}

/// SHA-1-only legacy API, also used by the resource-pack and loader fetchers.
pub(crate) fn download(
    url: &str,
    sha1: Option<&str>,
    dest: &Path,
    overwrite: bool,
) -> Result<(), String> {
    download_checked(url, Checksum::sha1(sha1)?, dest, overwrite)
}

pub(crate) fn download_checked(
    url: &str,
    checksum: Checksum<'_>,
    dest: &Path,
    overwrite: bool,
) -> Result<(), String> {
    let response = crate::net::agent(url)
        .get(url)
        .set("User-Agent", modrinth::AGENT)
        .call()
        .map_err(|e| format!("{url}: {e}"))?;
    store_download_checked(response.into_reader(), checksum, dest, overwrite)
        .map_err(|e| format!("{url}: {e}"))
}

/// Download a pack entry, preferring its SHA-512 and refusing a missing hash.
/// Resource-pack callers can use this without reimplementing checksum policy.
pub(crate) fn download_verified(
    url: &str,
    file: &PackFile,
    dest: &Path,
    overwrite: bool,
) -> Result<(), String> {
    if !HOSTS.iter().any(|host| url.starts_with(host)) {
        return Err(format!("{url}: download host is not allowed for modpacks"));
    }
    download_checked(url, Checksum::from_file(file)?, dest, overwrite)
}

/// Read and verify the whole bounded response before publishing it; a failed read leaves no file.
pub(crate) fn store_download(
    reader: impl Read,
    sha1: Option<&str>,
    dest: &Path,
    overwrite: bool,
) -> Result<(), String> {
    store_download_checked(reader, Checksum::sha1(sha1)?, dest, overwrite)
}

/// Offline/read-based counterpart for other pack entry fetchers and tests.
pub(crate) fn store_download_verified(
    reader: impl Read,
    file: &PackFile,
    dest: &Path,
    overwrite: bool,
) -> Result<(), String> {
    store_download_checked(reader, Checksum::from_file(file)?, dest, overwrite)
}

/// Rename a verified temporary file without losing the previous destination
/// if publication fails. Windows cannot rename a file over an existing file;
/// keep the old bytes in a same-directory backup until the new name exists.
fn replace_verified(
    part: &Path,
    dest: &Path,
    suffix: usize,
    publish: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
) -> Result<(), String> {
    if !dest.exists() {
        return publish(part, dest).map_err(|e| e.to_string());
    }
    if !dest.is_file() {
        return Err(format!("{} is not a regular file", dest.display()));
    }
    let backup = dest.with_file_name(format!(
        ".horizon-download-{}-{suffix}.backup",
        std::process::id()
    ));
    if backup.exists() {
        return Err(format!("{} already exists", backup.display()));
    }
    fs::rename(dest, &backup).map_err(|e| e.to_string())?;
    match publish(part, dest) {
        Ok(()) => fs::remove_file(&backup).map_err(|e| e.to_string()),
        Err(error) => match fs::rename(&backup, dest) {
            Ok(()) => Err(error.to_string()),
            Err(restore) => Err(format!(
                "{error}; failed to restore {} from {}: {restore}",
                dest.display(),
                backup.display()
            )),
        },
    }
}

pub(crate) fn store_download_checked(
    reader: impl Read,
    checksum: Checksum<'_>,
    dest: &Path,
    overwrite: bool,
) -> Result<(), String> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_JAR + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_JAR {
        return Err("download exceeds 512 MiB".into());
    }
    checksum.verify(&bytes)?;
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let suffix = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let part = dest.with_file_name(format!(
        ".horizon-download-{}-{suffix}.part",
        std::process::id()
    ));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&part)
        .map_err(|e| e.to_string())?;
    let result = file
        .write_all(&bytes)
        .map_err(|e| e.to_string())
        .and_then(|()| {
            drop(file);
            if overwrite {
                replace_verified(&part, dest, suffix, fs::rename)
            } else {
                // Publish without replacing a file that appeared while the request was in flight.
                match fs::hard_link(&part, dest) {
                    Ok(()) => fs::remove_file(&part).map_err(|e| e.to_string()),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(e.to_string()),
                    Err(_) => {
                        // No hard links here (FAT, some network mounts): copy into a new file.
                        let mut out = fs::OpenOptions::new()
                            .write(true)
                            .create_new(true)
                            .open(dest)
                            .map_err(|e| e.to_string())?;
                        let written = out.write_all(&bytes).map_err(|e| e.to_string());
                        drop(out);
                        if written.is_err() {
                            let _ = fs::remove_file(dest);
                        }
                        let _ = fs::remove_file(&part);
                        written
                    }
                }
            }
        });
    if result.is_err() {
        let _ = fs::remove_file(&part);
    }
    result
}

/// The jars in a folder of the player's, by SHA-1 and by name, and what a
/// CurseForge app instance (`minecraftinstance.json`) says each
/// project's file is called.
#[derive(Default)]
struct Local {
    by_sha1: HashMap<String, PathBuf>,
    by_sha512: HashMap<String, PathBuf>,
    by_name: HashMap<String, PathBuf>,
    curseforge: HashMap<(u64, u64), String>,
}

impl Local {
    fn index(dir: &Path) -> Local {
        let mut local = Local::default();
        local.walk(dir, 3);
        if let Ok(text) = fs::read_to_string(dir.join("minecraftinstance.json")) {
            if let Ok(v) = serde_json::from_str::<Value>(&text) {
                local.curseforge = curseforge_names(&v);
            }
        }
        local
    }

    fn walk(&mut self, dir: &Path, depth: usize) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for path in entries.flatten().map(|e| e.path()) {
            if path.is_dir() {
                if depth > 0 {
                    self.walk(&path, depth - 1);
                }
            } else if path.extension().is_some_and(|x| x == "jar") {
                if let Ok(bytes) = fs::read(&path) {
                    self.by_sha1.insert(sha1_hex(&bytes), path.clone());
                    self.by_sha512.insert(sha512_hex(&bytes), path.clone());
                }
                if let Some(n) = path.file_name() {
                    self.by_name
                        .insert(n.to_string_lossy().into_owned(), path.clone());
                }
            }
        }
    }

    fn find(&self, file: &PackFile) -> Option<PathBuf> {
        if let Some(h) = &file.sha512 {
            return self.by_sha512.get(&h.to_ascii_lowercase()).cloned();
        }
        if let Some(h) = &file.sha1 {
            return self.by_sha1.get(&h.to_ascii_lowercase()).cloned();
        }
        let name = file
            .curseforge
            .and_then(|ids| self.curseforge.get(&ids))
            .map_or(file.file_name(), String::as_str);
        self.by_name.get(name).cloned()
    }
}

/// `installedAddons[]`: (project, file) → the jar's file name.
pub fn curseforge_names(instance: &Value) -> HashMap<(u64, u64), String> {
    instance["installedAddons"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| {
            let file = &a["installedFile"];
            Some((
                (a["addonID"].as_u64()?, file["id"].as_u64()?),
                file["fileName"].as_str()?.to_owned(),
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_pack_index_name_must_stay_inside_jars() {
        assert!(plain_file_name("sodium-0.6.jar"));
        for name in ["", ".", "..", "..\\..\\evil.dll", "C:evil.jar", "a/b.jar"] {
            assert!(!plain_file_name(name), "{name:?}");
        }
    }

    fn file(sha1: Option<&str>, downloads: &[&str]) -> PackFile {
        PackFile {
            path: "mods/a.jar".into(),
            sha1: sha1.map(str::to_owned),
            downloads: downloads.iter().map(|d| (*d).to_owned()).collect(),
            ..PackFile::default()
        }
    }

    #[test]
    fn links_come_from_the_pack_or_modrinth_and_only_allowed_hosts() {
        let mut cache = Cache::default();
        cache.versions.insert(
            "ab".into(),
            json!({"files": [
                {"hashes": {"sha1": "cd"}, "url": "https://cdn.modrinth.com/other.jar"},
                {"hashes": {"sha1": "ab"}, "url": "https://cdn.modrinth.com/a.jar"}]}),
        );
        let pack_link = file(
            Some("ab"),
            &["https://evil.example/a.jar", "https://github.com/a.jar"],
        );
        assert_eq!(
            url_of(&pack_link, &cache).as_deref(),
            Some("https://github.com/a.jar")
        );
        let modrinth = file(Some("ab"), &["https://evil.example/a.jar"]);
        assert_eq!(
            url_of(&modrinth, &cache).as_deref(),
            Some("https://cdn.modrinth.com/a.jar")
        );
        assert_eq!(url_of(&file(Some("ef"), &[]), &cache), None);
        assert_eq!(url_of(&file(None, &[]), &cache), None);
        let rejected = download_verified(
            "https://untrusted.example/mod.jar",
            &PackFile {
                sha512: Some(sha512_hex(b"mod")),
                ..PackFile::default()
            },
            Path::new("do-not-create.jar"),
            true,
        );
        assert!(rejected.unwrap_err().contains("not allowed"));
    }

    #[test]
    fn reads_a_curseforge_instance() {
        let instance = json!({"installedAddons": [
            {"addonID": 238222, "installedFile": {"id": 5101366, "fileName": "jei-1.21.1.jar"}},
            {"addonID": 1, "installedFile": {"fileName": "no-id.jar"}}]});
        let names = curseforge_names(&instance);
        assert_eq!(names.len(), 1);
        assert_eq!(names[&(238222, 5101366)], "jei-1.21.1.jar");
    }

    #[test]
    fn copies_players_jars_and_preserves_curseforge_needs_key() {
        let root = std::env::temp_dir().join(format!("horizon-fetch-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let (work, mine) = (root.join("work"), root.join("instance/mods"));
        fs::create_dir_all(&mine).unwrap();
        fs::create_dir_all(&work).unwrap();
        fs::write(mine.join("renamed.jar"), b"jar a").unwrap();
        fs::write(mine.join("cf.jar"), b"jar b").unwrap();
        fs::write(
            root.join("instance/minecraftinstance.json"),
            json!({"installedAddons": [{"addonID": 7, "installedFile": {"id": 8, "fileName": "cf.jar"}}]})
                .to_string(),
        )
        .unwrap();
        let pack = Pack {
            format: "curseforge".into(),
            files: vec![
                PackFile {
                    path: "mods/a.jar".into(),
                    sha1: Some(sha1_hex(b"jar a")),
                    ..PackFile::default()
                },
                PackFile {
                    path: "mods/curseforge-7-8".into(),
                    sha1: Some(sha1_hex(b"jar b")),
                    curseforge: Some((7, 8)),
                    ..PackFile::default()
                },
                PackFile {
                    path: "mods/curseforge-9-9".into(),
                    curseforge: Some((9, 9)),
                    ..PackFile::default()
                },
            ],
            ..Pack::default()
        };
        write_json(&work.join("pack.json"), &pack).unwrap();
        let records: Vec<Record> = pack
            .files
            .iter()
            .map(|f| Record {
                key: f.file_name().into(),
                path: f.path.clone(),
                ..Record::default()
            })
            .collect();
        let fetched = fetch(
            &work,
            &records,
            Some(&root.join("instance")),
            &|_| true,
            &|_| {},
        )
        .unwrap();
        let hows: Vec<How> = fetched.files.iter().map(|o| o.how).collect();
        assert_eq!(hows, [How::Local, How::Local, How::NeedsKey]);
        assert!(fetched.files[2].detail.contains("no trusted local copy"));
        assert_eq!(fs::read(work.join("jars/a.jar")).unwrap(), b"jar a");
        assert_eq!(
            fs::read(work.join("jars/curseforge-7-8")).unwrap(),
            b"jar b"
        );
        // A second run finds them in place.
        let again = fetch(&work, &records, None, &|_| true, &|_| {}).unwrap();
        assert_eq!(again.count(How::Present), 2);
        assert_eq!(Fetched::load(&work).count(How::Present), 2);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn hashless_curseforge_accepts_only_explicit_local_source_and_pins_sha1() {
        let root = std::env::temp_dir().join(format!(
            "horizon-fetch-cf-local-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&root);
        let work = root.join("work");
        let mine = root.join("instance/mods");
        fs::create_dir_all(&work).unwrap();
        fs::create_dir_all(&mine).unwrap();
        fs::write(mine.join("original.jar"), b"local CF bytes").unwrap();
        fs::write(
            root.join("instance/minecraftinstance.json"),
            json!({"installedAddons": [{
                "addonID": 7, "installedFile": {"id": 8, "fileName": "original.jar"}
            }]})
            .to_string(),
        )
        .unwrap();
        let pack = Pack {
            format: "curseforge".into(),
            files: vec![PackFile {
                path: "mods/curseforge-7-8".into(),
                curseforge: Some((7, 8)),
                ..PackFile::default()
            }],
            ..Pack::default()
        };
        write_json(&work.join("pack.json"), &pack).unwrap();
        let records = [Record {
            key: "cf".into(),
            path: "mods/curseforge-7-8".into(),
            ..Record::default()
        }];
        let first = fetch(&work, &records, Some(&root.join("instance")), &|_| true, &|_| {})
            .unwrap();
        assert_eq!(first.files[0].how, How::Local);
        assert!(first.files[0].detail.contains("trusted local source"));
        let pinned: Pack = read_json(&work.join("pack.json")).unwrap();
        assert_eq!(pinned.files[0].sha1.as_deref(), Some(sha1_hex(b"local CF bytes").as_str()));
        assert_eq!(fs::read(work.join("jars/curseforge-7-8")).unwrap(), b"local CF bytes");
        let cached = fetch(&work, &records, None, &|_| true, &|_| {}).unwrap();
        assert_eq!(cached.files[0].how, How::Present);
        fs::write(work.join("jars/curseforge-7-8"), b"altered cache").unwrap();
        let corrupted = fetch(&work, &records, None, &|_| true, &|_| {}).unwrap();
        assert_eq!(corrupted.files[0].how, How::Failed);

        // An explicit original jar path also counts as local provenance.
        let second = root.join("second");
        fs::create_dir_all(&second).unwrap();
        write_json(&second.join("pack.json"), &pack).unwrap();
        let record = [Record {
            key: "cf".into(),
            path: "mods/curseforge-7-8".into(),
            jar: Some(mine.join("original.jar")),
            ..Record::default()
        }];
        let imported = fetch(&second, &record, None, &|_| true, &|_| {}).unwrap();
        assert_eq!(imported.files[0].how, How::Local);
        let pinned: Pack = read_json(&second.join("pack.json")).unwrap();
        assert_eq!(pinned.files[0].sha1.as_deref(), Some(sha1_hex(b"local CF bytes").as_str()));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sha512_only_local_file_is_verified_and_bad_cache_is_not_accepted() {
        let root = std::env::temp_dir().join(format!(
            "horizon-fetch-sha512-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&root);
        let work = root.join("work");
        let mine = root.join("mine");
        fs::create_dir_all(&mine).unwrap();
        fs::create_dir_all(work.join("jars")).unwrap();
        fs::write(mine.join("renamed.jar"), b"verified jar").unwrap();
        let sha512 = sha512_hex(b"verified jar");
        let pack = Pack {
            format: "mrpack".into(),
            files: vec![PackFile {
                path: "mods/test.jar".into(),
                sha512: Some(sha512.clone()),
                ..PackFile::default()
            }],
            ..Pack::default()
        };
        write_json(&work.join("pack.json"), &pack).unwrap();
        let records = [Record {
            key: "test".into(),
            path: "mods/test.jar".into(),
            ..Record::default()
        }];
        let result = fetch(&work, &records, Some(&mine), &|_| true, &|_| {}).unwrap();
        assert_eq!(result.files[0].how, How::Local);
        assert_eq!(fs::read(work.join("jars/test.jar")).unwrap(), b"verified jar");
        let again = fetch(&work, &records, None, &|_| true, &|_| {}).unwrap();
        assert_eq!(again.files[0].how, How::Present);

        // A previously downloaded jar may have been altered since inventory.
        fs::write(work.join("jars/test.jar"), b"tampered").unwrap();
        let rejected = fetch(&work, &records, None, &|_| true, &|_| {}).unwrap();
        assert_eq!(rejected.files[0].how, How::Failed);
        assert!(rejected.files[0].detail.contains("checksum"));
        assert_eq!(fs::read(work.join("jars/test.jar")).unwrap(), b"tampered");
        let repaired = fetch(&work, &records, Some(&mine), &|_| true, &|_| {}).unwrap();
        assert_eq!(repaired.files[0].how, How::Local);
        assert_eq!(fs::read(work.join("jars/test.jar")).unwrap(), b"verified jar");
        // Even with a good cache, do not trust an unverified original path:
        // the downstream carry step would read that original jar.
        let original = mine.join("corrupt-original.jar");
        fs::write(&original, b"corrupt original").unwrap();
        let bad_record = [Record {
            key: "test".into(),
            path: "mods/test.jar".into(),
            jar: Some(original),
            ..Record::default()
        }];
        let rejected = fetch(&work, &bad_record, None, &|_| true, &|_| {}).unwrap();
        assert_eq!(rejected.files[0].how, How::Failed);
        assert!(rejected.files[0].detail.contains("imported instance"));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn sha512_downloads_are_verified_before_publishing() {
        let root = std::env::temp_dir().join(format!(
            "horizon-fetch-digests-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let dest = root.join("test.jar");
        let hash = sha512_hex(b"correct jar");
        let hash_sha1 = sha1_hex(b"wrong jar");
        let expected = Checksum::Sha512(&hash);
        // SHA-512 wins when both hashes are supplied.
        assert!(Checksum::from_file(&PackFile {
            sha512: Some(hash.clone()),
            sha1: Some(hash_sha1),
            ..PackFile::default()
        })
        .unwrap()
        .verify(b"correct jar")
        .is_ok());
        assert!(store_download_checked(&b"tampered"[..], expected, &dest, true).is_err());
        assert!(!dest.exists());
        store_download_checked(&b"correct jar"[..], expected, &dest, true).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"correct jar");
        // Preserve existing SHA-1 callers (resource packs and loader jars).
        store_download(&b"legacy jar"[..], Some(&sha1_hex(b"legacy jar")), &dest, true).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"legacy jar");
        assert!(store_download_checked(&b"corrupt"[..], expected, &dest, true).is_err());
        assert_eq!(fs::read(&dest).unwrap(), b"legacy jar");
        assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn failed_publish_restores_original_jar_without_losing_verified_temp() {
        let root = std::env::temp_dir().join(format!(
            "horizon-fetch-restore-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let dest = root.join("original.jar");
        let part = root.join("verified.part");
        fs::write(&dest, b"original bytes").unwrap();
        fs::write(&part, b"verified replacement").unwrap();
        let err = replace_verified(&part, &dest, 123, |_, _| {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "simulated publication failure",
            ))
        })
        .unwrap_err();
        assert!(err.contains("simulated publication failure"));
        assert_eq!(fs::read(&dest).unwrap(), b"original bytes");
        assert_eq!(fs::read(&part).unwrap(), b"verified replacement");
        assert_eq!(fs::read_dir(&root).unwrap().count(), 2);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn missing_or_invalid_hashes_never_accept_files() {
        let root = std::env::temp_dir().join(format!(
            "horizon-fetch-nohash-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("jars")).unwrap();
        let dest = root.join("jars/nohash.jar");
        fs::write(&dest, b"untrusted local file").unwrap();
        let pack = Pack {
            format: "mrpack".into(),
            files: vec![PackFile {
                path: "mods/nohash.jar".into(),
                downloads: vec!["https://cdn.modrinth.com/data/nohash.jar".into()],
                ..PackFile::default()
            }],
            ..Pack::default()
        };
        write_json(&root.join("pack.json"), &pack).unwrap();
        let record = [Record {
            key: "nohash".into(),
            path: "mods/nohash.jar".into(),
            jar: Some(dest.clone()),
            ..Record::default()
        }];
        let result = fetch(&root, &record, None, &|_| true, &|_| {}).unwrap();
        assert_eq!(result.files[0].how, How::Failed);
        assert!(result.files[0].detail.contains("no SHA-512 or SHA-1"));
        assert_eq!(fs::read(&dest).unwrap(), b"untrusted local file");
        assert!(store_download(&b"untrusted"[..], None, &root.join("new.jar"), true).is_err());
        assert!(!root.join("new.jar").exists());
        for file in [
            PackFile { sha512: Some("xyz".into()), sha1: Some(sha1_hex(b"ok")), ..PackFile::default() },
            PackFile { sha1: Some("12nothex".into()), ..PackFile::default() },
        ] {
            assert!(Checksum::from_file(&file).is_err());
        }
        fs::remove_dir_all(&root).unwrap();
    }
}
