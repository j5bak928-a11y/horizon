//! A world's data packs: `<world>/datapacks/`, each a `.zip` or a folder with a `pack.mcmeta`, as
//! the game discovers them (`datapack::discover`). The game turns on every pack it finds there
//! that `<world>/datapacks.json` doesn't list as disabled, when the world opens or on `/reload`,
//! so adding one is copying it in.
use crate::i18n::{tr, trf};
use crate::packs::{entry, plain};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

const SETTINGS: &str = "datapacks.json";
const MAX_FILES: usize = 65_536;
const MAX_PACK_BYTES: u64 = 512 << 20;
const MAX_JSON_BYTES: u64 = 16 << 20;
const MAX_METADATA_BYTES: u64 = 1 << 20;

#[derive(Clone, Debug, PartialEq)]
pub struct DataPack {
    /// Its file or folder name in `datapacks/`; the game's id is `file/<name>`.
    pub file: String,
    pub path: PathBuf,
    /// `pack.mcmeta`'s description as plain text.
    pub description: String,
    /// Off when `datapacks.json` lists it as disabled (the game's `/datapack disable`).
    pub enabled: bool,
}

fn settings(world: &Path) -> serde_json::Value {
    fs::read(world.join(SETTINGS))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_else(|| serde_json::json!({}))
}

fn disabled(settings: &serde_json::Value) -> Vec<String> {
    settings["disabled"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect()
}

/// The data packs of the world folder `world`, by name.
pub fn list(world: &Path) -> Vec<DataPack> {
    let off = disabled(&settings(world));
    let mut packs: Vec<DataPack> = fs::read_dir(world.join("datapacks"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            let file = e.file_name().to_str()?.to_string();
            let zip = path.is_file() && file.to_ascii_lowercase().ends_with(".zip");
            if !(zip || path.is_dir()) {
                return None;
            }
            let meta: serde_json::Value =
                serde_json::from_slice(&entry(&path, "pack.mcmeta")?).ok()?;
            Some(DataPack {
                enabled: !off.contains(&format!("file/{file}")),
                description: plain(&meta["pack"]["description"]),
                file,
                path,
            })
        })
        .collect();
    packs.sort_by_key(|p| p.file.to_lowercase());
    packs
}

/// Checks the archive as well as its contents before a user-controlled zip reaches a world.
fn check(file: &Path) -> Result<(), String> {
    coverage(file).map(|_| ())
}

/// Copies a data pack zip into the world's `datapacks/` under a free name. The game turns it on
/// the next time the world opens (or on `/reload`).
pub fn add(world: &Path, file: &Path) -> Result<DataPack, String> {
    check(file)?;
    let dir = world.join("datapacks");
    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let name = file.file_name().ok_or("A pack file has a name")?;
    let stem = Path::new(name)
        .file_stem()
        .unwrap_or(name)
        .to_string_lossy();
    let target = (1..)
        .map(|n| match n {
            1 => dir.join(name),
            n => dir.join(format!("{stem} ({n}).zip")),
        })
        .find(|p| !p.exists())
        .unwrap();
    fs::copy(file, &target).map_err(|e| format!("Could not copy {}: {e}", file.display()))?;
    let file_name = target.file_name().unwrap().to_string_lossy().into_owned();
    // A pack of the same name removed while it was disabled would stay off.
    forget(world, &file_name)?;
    list(world)
        .into_iter()
        .find(|p| p.file == file_name)
        .ok_or_else(|| format!("{} is not a data pack the game reads", target.display()))
}

/// Drops `file/<name>` from both lists of `datapacks.json`, when it is there.
fn forget(world: &Path, file: &str) -> Result<(), String> {
    let mut s = settings(world);
    let id = format!("file/{file}");
    let mut changed = false;
    for key in ["enabled", "disabled"] {
        if let Some(list) = s[key].as_array_mut() {
            let before = list.len();
            list.retain(|v| v.as_str() != Some(id.as_str()));
            changed |= list.len() != before;
        }
    }
    if !changed {
        return Ok(());
    }
    let text = serde_json::to_string_pretty(&s).unwrap();
    fs::write(world.join(SETTINGS), text).map_err(|e| format!("{SETTINGS}: {e}"))
}

/// The data pack a command names: its file name, with or without `.zip`.
pub fn find(world: &Path, query: &str) -> Result<DataPack, String> {
    list(world)
        .into_iter()
        .find(|p| p.file == query || p.file.strip_suffix(".zip") == Some(query))
        .ok_or_else(|| {
            format!(
                "No data pack {query} in {}",
                world.join("datapacks").display()
            )
        })
}

/// Moves the data pack to the system's trash and forgets it in `datapacks.json`.
pub fn remove(world: &Path, pack: &DataPack) -> Result<(), String> {
    crate::desktop_seams::to_trash(&pack.path)
        .map_err(|e| format!("Could not move {} to the trash: {e}", pack.file))?;
    forget(world, &pack.file)
}

/// A data pack's files by kind (`function`, `worldgen`, `item tag`...), with how many each.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Coverage {
    /// Files read by a Horizon runtime consumer, which may still reject their contents.
    pub read: Vec<(String, usize)>,
    /// Subset of `read` whose format or gameplay behavior is only partly supported.
    pub partial: Vec<(String, usize)>,
    /// Files that no runtime consumer currently reads (and invalid supported files).
    pub unread: Vec<(String, usize)>,
}

impl Coverage {
    /// Separate recognized data from unrecognized data without promising its behavior works.
    pub fn summary(&self) -> String {
        let runs = match self.read.is_empty() {
            true => tr("No supported data pack files found.").to_string(),
            false => trf(
                "Horizon reads its {files} (valid game data required).",
                &[("files", &counts(&self.read))],
            ),
        };
        let mut lines = runs;
        if !self.partial.is_empty() {
            lines.push(' ');
            lines.push_str(&trf(
                "Limited support: {files}.",
                &[("files", &counts(&self.partial))],
            ));
        }
        if !self.unread.is_empty() {
            lines.push(' ');
            lines.push_str(&trf(
                "Unsupported or invalid: {files}.",
                &[("files", &self.not_read())],
            ));
        }
        lines
    }

    /// "1077 worldgen, 653 loot tables, 16 recipes": what Horizon doesn't read yet, the six
    /// biggest kinds and how many other kinds.
    pub fn not_read(&self) -> String {
        let mut biggest = self.unread.clone();
        biggest.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
        let others = biggest.len().saturating_sub(6);
        biggest.truncate(6);
        match others {
            0 => counts(&biggest),
            1 => format!("{} and 1 other kind", counts(&biggest)),
            n => format!("{} and {n} other kinds", counts(&biggest)),
        }
    }
}

fn counts(list: &[(String, usize)]) -> String {
    list.iter()
        .map(|(k, n)| {
            let k = k.replace('_', " ");
            match *n == 1 || k.ends_with("worldgen") {
                true => format!("{n} {k}"),
                false => format!("{n} {k}s"),
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Which runtime recognizes this file path. Partial means its consumer may implement only
/// some behaviors or codec variants, not that the file is guaranteed to change a world.
fn recognized(name: &str) -> Option<(String, bool)> {
    let rest = name.strip_prefix("data/")?;
    let (namespace, path) = rest.split_once('/')?;
    if namespace.is_empty() {
        return None;
    }
    if let Some(path) = path.strip_prefix("function/") {
        return path
            .ends_with(".mcfunction")
            .then(|| ("function".into(), false));
    }
    if !path.ends_with(".json") {
        return None;
    }
    if let Some(tag) = path.strip_prefix("tags/") {
        let (kind, rest) = tag.split_once('/')?;
        let (label, partial) = match kind {
            "function" => ("function tag", false),
            "worldgen" if rest.starts_with("biome/") => ("biome tag", true),
            "item" | "block" | "fluid" | "entity_type" | "enchantment" | "damage_type"
            | "dialog" | "potion" | "villager_trade" | "banner_pattern" | "instrument"
            | "game_event" => (kind, true),
            _ => return None,
        };
        return Some((
            if label.ends_with(" tag") {
                label.into()
            } else {
                format!("{label} tag")
            },
            partial,
        ));
    }
    if path.starts_with("worldgen/") {
        let (kind, _) = path.strip_prefix("worldgen/")?.split_once('/')?;
        if matches!(
            kind,
            "biome"
                | "block_state_provider"
                | "carver"
                | "feature"
                | "density_function"
                | "material_condition"
                | "material_rule"
                | "noise"
                | "noise_settings"
                | "placed_feature"
                | "processor_list"
                | "structure"
                | "structure_set"
                | "template_pool"
        ) {
            return Some(("worldgen".into(), true));
        }
        return None;
    }
    let (kind, _) = path.split_once('/')?;
    match kind {
        "predicate" | "item_modifier" => Some((kind.into(), false)),
        "recipe" | "recipe_filter" | "loot_table" | "advancement" | "trade_set"
        | "villager_trade" | "dialog" | "data_maps" | "dimension" | "dimension_type" => {
            Some((kind.into(), true))
        }
        _ => None,
    }
}

fn safe_zip_path(name: &str) -> bool {
    let name = name.trim_end_matches('/');
    !name.is_empty()
        && name.len() <= 4096
        && name.split('/').count() <= 64
        && name.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && !part.ends_with(['.', ' '])
                && !part.chars().any(|c| c.is_control() || "\\:*?\"<>|".contains(c))
        })
}

/// Inspect a zip's paths and bounded contents, then count real runtime-consumed file kinds.
/// Unknown paths and malformed JSON do not make an otherwise unsupported pack installable.
pub fn coverage(file: &Path) -> Result<Coverage, String> {
    let mut zip = fs::File::open(file)
        .ok()
        .and_then(|f| zip::ZipArchive::new(f).ok())
        .ok_or_else(|| format!("{} is not a zip file that can be read.", file.display()))?;
    if zip.len() > MAX_FILES {
        return Err(trf(
            "{file} has too many data pack entries.",
            &[("file", &file.display().to_string())],
        ));
    }
    let mut kinds = BTreeMap::<String, (usize, usize, usize)>::new();
    let mut seen = HashSet::new();
    let mut total = 0u64;
    let mut has_data = false;
    let mut metadata = false;
    for index in 0..zip.len() {
        let mut entry = zip.by_index(index).map_err(|e| e.to_string())?;
        let name = entry.name().map_err(|e| e.to_string())?.into_owned();
        if !safe_zip_path(&name) {
            return Err(trf("Unsafe data pack zip path: {name}", &[("name", &name)]));
        }
        if entry.unix_mode().is_some_and(|mode| mode & 0o170000 == 0o120000) {
            return Err(trf(
                "Data pack contains a symbolic link: {name}",
                &[("name", &name)],
            ));
        }
        if entry.is_dir() {
            continue;
        }
        if !seen.insert(name.to_lowercase()) {
            return Err(trf("Duplicate data pack zip entry: {name}", &[("name", &name)]));
        }
        total = total
            .checked_add(entry.size())
            .ok_or_else(|| tr("Data pack is too large").to_string())?;
        if total > MAX_PACK_BYTES {
            return Err(tr("Data pack exceeds 512 MiB unpacked").to_string());
        }
        if name == "pack.mcmeta" {
            if entry.size() > MAX_METADATA_BYTES {
                return Err(tr("Data pack metadata is too large").to_string());
            }
            let mut bytes = Vec::new();
            entry
                .take(MAX_METADATA_BYTES + 1)
                .read_to_end(&mut bytes)
                .map_err(|e| format!("pack.mcmeta: {e}"))?;
            if bytes.len() as u64 > MAX_METADATA_BYTES {
                return Err(tr("Data pack metadata is too large").to_string());
            }
            let meta: serde_json::Value = serde_json::from_slice(&bytes)
                .map_err(|e| format!("pack.mcmeta: {e}"))?;
            if !meta["pack"].is_object() {
                return Err(tr("pack.mcmeta has no pack object").to_string());
            }
            metadata = true;
            continue;
        }
        if !name.starts_with("data/") {
            continue;
        }
        has_data = true;
        let Some((kind, partial)) = recognized(&name) else {
            let label = name
                .strip_prefix("data/")
                .and_then(|p| p.split_once('/'))
                .and_then(|(_, p)| p.split_once('/'))
                .map_or("unknown", |(kind, _)| match kind {
                    "tags" => "unsupported tag",
                    "worldgen" => "unsupported worldgen",
                    kind => kind,
                });
            kinds.entry(label.into()).or_default().2 += 1;
            continue;
        };
        if entry.size() > MAX_JSON_BYTES {
            return Err(trf(
                "Data pack file {name} exceeds 16 MiB",
                &[("name", &name)],
            ));
        }
        let mut bytes = Vec::new();
        entry
            .take(MAX_JSON_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| format!("{name}: {e}"))?;
        if bytes.len() as u64 > MAX_JSON_BYTES {
            return Err(trf(
                "Data pack file {name} exceeds 16 MiB",
                &[("name", &name)],
            ));
        }
        if name.ends_with(".json") {
            if serde_json::from_slice::<serde_json::Value>(&bytes).is_err() {
                kinds.entry("invalid JSON".into()).or_default().2 += 1;
                continue;
            }
        } else if std::str::from_utf8(&bytes).is_err() {
            kinds.entry("invalid function text".into()).or_default().2 += 1;
            continue;
        }
        let counts = kinds.entry(kind).or_default();
        counts.0 += 1;
        if partial {
            counts.1 += 1;
        }
    }
    if !metadata {
        return Err(tr("Data pack has no pack.mcmeta").to_string());
    }
    if !has_data {
        return Err(
            tr("Data pack has no data folder: it is a resource pack, not a data pack.")
                .to_string(),
        );
    }
    let mut coverage = Coverage::default();
    for (kind, (read, partial, unread)) in kinds {
        if read > 0 {
            coverage.read.push((kind.clone(), read));
        }
        if partial > 0 {
            coverage.partial.push((kind.clone(), partial));
        }
        if unread > 0 {
            coverage.unread.push((kind, unread));
        }
    }
    Ok(coverage)
}

#[cfg(test)]
#[path = "datapacks/tests.rs"]
mod tests;
