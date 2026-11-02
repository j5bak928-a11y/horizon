use super::*;
use std::io::Write;

fn scratch(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "horizon-launcher-datapacks-{name}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    root
}

fn zip_file(path: &Path, files: &[(&str, &str)]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut zip = zip::ZipWriter::new(fs::File::create(path).unwrap());
    for (name, text) in files {
        zip.start_file(*name, zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(text.as_bytes()).unwrap();
    }
    zip.finish().unwrap();
}

const META: &str = r#"{"pack":{"pack_format":88,"description":"More trades"}}"#;

#[test]
fn lists_adds_and_removes_as_the_game_reads_them() {
    let root = scratch("all");
    let world = root.join("saves/My World");
    zip_file(
        &world.join("datapacks/Trades.zip"),
        &[
            ("pack.mcmeta", META),
            ("data/t/function/a.mcfunction", "say hi"),
        ],
    );
    fs::create_dir_all(world.join("datapacks/folder pack")).unwrap();
    fs::write(
        world.join("datapacks/folder pack/pack.mcmeta"),
        r#"{"pack":{"description":{"text":"Mine"}}}"#,
    )
    .unwrap();
    fs::write(world.join("datapacks/readme.txt"), "").unwrap();
    fs::write(
        world.join("datapacks.json"),
        r#"{"enabled":[],"disabled":["file/folder pack","file/Gone.zip"]}"#,
    )
    .unwrap();
    let packs = list(&world);
    let seen: Vec<(&str, &str, bool)> = packs
        .iter()
        .map(|p| (p.file.as_str(), p.description.as_str(), p.enabled))
        .collect();
    assert_eq!(
        seen,
        [
            ("folder pack", "Mine", false),
            ("Trades.zip", "More trades", true)
        ]
    );

    let drops = root.join("drops");
    zip_file(
        &drops.join("Gone.zip"),
        &[("pack.mcmeta", META), ("data/g/tags/x.json", "{}")],
    );
    zip_file(
        &drops.join("Trades.zip"),
        &[("pack.mcmeta", META), ("data/t/function/b.mcfunction", "")],
    );
    zip_file(
        &drops.join("Textures.zip"),
        &[
            ("pack.mcmeta", META),
            ("assets/minecraft/textures/a.png", ""),
        ],
    );
    // A pack disabled before it was taken out comes back on.
    let gone = add(&world, &drops.join("Gone.zip")).unwrap();
    assert!(gone.enabled);
    assert!(!fs::read_to_string(world.join("datapacks.json"))
        .unwrap()
        .contains("Gone.zip"));
    assert_eq!(
        add(&world, &drops.join("Trades.zip")).unwrap().file,
        "Trades (2).zip"
    );
    let refused = add(&world, &drops.join("Textures.zip")).unwrap_err();
    assert!(refused.contains("resource pack"), "{refused}");

    let folder = find(&world, "folder pack").unwrap();
    let trash = root.join("trash");
    std::env::set_var(crate::desktop_seams::TRASH_ENV, &trash);
    remove(&world, &folder).unwrap();
    std::env::remove_var(crate::desktop_seams::TRASH_ENV);
    assert!(trash.join("folder pack/pack.mcmeta").is_file());
    assert!(!fs::read_to_string(world.join("datapacks.json"))
        .unwrap()
        .contains("folder pack"));
    assert_eq!(find(&world, "Trades").unwrap().file, "Trades.zip");
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn coverage_reports_consumed_partial_and_unread_files() {
    let root = scratch("coverage");
    let file = root.join("p.zip");
    zip_file(
        &file,
        &[
            ("pack.mcmeta", META),
            ("data/a/function/x.mcfunction", ""),
            ("data/a/predicate/p.json", "{}"),
            ("data/a/item_modifier/m.json", "{}"),
            ("data/a/tags/item/i.json", "{}"),
            ("data/a/tags/worldgen/biome/has_structure.json", "{}"),
            ("data/a/loot_table/l.json", "{}"),
            ("data/a/loot_table/k.json", "{}"),
            ("data/a/worldgen/biome/b.json", "{}"),
            ("data/a/worldgen/unknown_type/o.json", "{}"),
            ("assets/a/lang/en_us.json", "{}"),
        ],
    );
    let c = coverage(&file).unwrap();
    assert_eq!(
        c.summary(),
        "Horizon reads its 1 biome tag, 1 function, 1 item modifier, 1 item tag, 2 loot tables, \
         1 predicate, 1 worldgen (valid game data required). Limited support: 1 biome tag, \
         1 item tag, 2 loot tables, 1 worldgen. Unsupported or invalid: 1 unsupported worldgen."
    );
    assert_eq!(c.read.iter().map(|(_, n)| n).sum::<usize>(), 8);
    assert_eq!(c.partial.iter().map(|(_, n)| n).sum::<usize>(), 5);
    assert_eq!(c.unread, [("unsupported worldgen".into(), 1)]);
    let many = Coverage {
        read: Vec::new(),
        unread: (1..=8).map(|n| (format!("kind{n}"), n)).collect(),
        ..Coverage::default()
    };
    assert_eq!(
        many.not_read(),
        "8 kind8s, 7 kind7s, 6 kind6s, 5 kind5s, 4 kind4s, 3 kind3s and 2 other kinds"
    );
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn data_pack_coverage_requires_recognized_paths_and_valid_json() {
    let root = scratch("recognized");
    let file = root.join("data.zip");
    for (path, content, read) in [
        ("data/test/recipe/example.json", "{}", true),
        ("data/test/loot_table/chest.json", "{}", true),
        ("data/test/advancement/first.json", "{}", true),
        ("data/test/tags/block/mineable.json", "{}", true),
        ("data/test/worldgen/noise_settings/overworld.json", "{}", true),
        ("data/test/dimension_type/surface.json", "{}", true),
        ("data/test/worldgen/biome/new.json", "{}", true),
        ("data/test/worldgen/alien/unknown.json", "{}", false),
        ("data/test/tags/alien/missing.json", "{}", false),
        ("data/test/recipe/example.txt", "{}", false),
        ("data/test/recipe/invalid.json", "invalid", false),
    ] {
        zip_file(&file, &[("pack.mcmeta", META), (path, content)]);
        let result = coverage(&file).unwrap();
        assert_eq!(result.read.iter().map(|(_, n)| n).sum::<usize>() > 0, read, "{path}");
        if read {
            assert!(!result.partial.is_empty(), "{path} may have partial runtime support");
        } else {
            assert!(!result.unread.is_empty(), "{path}");
        }
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn zip_validation_refuses_unsafe_paths_missing_or_invalid_metadata_and_duplicates() {
    let root = scratch("unsafe");
    let file = root.join("data.zip");
    for name in [
        "../escape",
        "/absolute",
        "data/../escape",
        "data/x\\bad",
        "C:/drive",
    ] {
        zip_file(&file, &[("pack.mcmeta", META), (name, "{}")]);
        assert!(coverage(&file).is_err(), "{name}");
    }
    zip_file(&file, &[("data/a/recipe/a.json", "{}")]);
    assert!(coverage(&file).unwrap_err().contains("pack.mcmeta"));
    zip_file(
        &file,
        &[("pack.mcmeta", "broken"), ("data/a/recipe/a.json", "{}")],
    );
    assert!(coverage(&file).unwrap_err().contains("pack.mcmeta"));
    zip_file(&file, &[("pack.mcmeta", META), ("pack.mcmeta", META)]);
    assert!(coverage(&file).unwrap_err().contains("Duplicate"));
    fs::remove_dir_all(root).unwrap();
}
