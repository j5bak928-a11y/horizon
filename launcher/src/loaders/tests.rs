//! Lists and profiles shaped like Fabric's and Quilt's, served by a fetch that never reaches the
//! network.
use super::*;
use crate::sources::sha1_hex;
use std::collections::HashMap;
use std::fs;

#[test]
fn versions_name_the_stable_ones() {
    let fabric = r#"[{"loader": {"version": "0.17.0-beta.1", "stable": false}},
                     {"loader": {"version": "0.16.9", "stable": true}}]"#;
    let listed = parse_versions(fabric).unwrap();
    assert_eq!(pick(Kind::Fabric, &listed, None).unwrap(), "0.16.9");
    assert_eq!(
        pick(Kind::Fabric, &listed, Some("0.17.0-beta.1")).unwrap(),
        "0.17.0-beta.1"
    );
    assert!(pick(Kind::Fabric, &listed, Some("0.1"))
        .unwrap_err()
        .contains("Fabric has no loader 0.1"));
    // Quilt's comes in no order and says nothing of stability: a beta has a `-`.
    let quilt = [
        "0.20.0-beta.9",
        "0.24.0",
        "0.20.0-beta.10",
        "0.9.0",
        "0.20.0",
        "0.30.1-beta.1",
    ]
    .map(|v| format!(r#"{{"loader": {{"version": "{v}"}}}}"#))
    .join(",");
    let listed = parse_versions(&format!("[{quilt}]")).unwrap();
    let order: Vec<&str> = listed.iter().map(|l| l.version.as_str()).collect();
    assert_eq!(
        order,
        [
            "0.30.1-beta.1",
            "0.24.0",
            "0.20.0",
            "0.20.0-beta.10",
            "0.20.0-beta.9",
            "0.9.0"
        ]
    );
    assert_eq!(
        pick(Kind::Quilt, &listed, Some("latest")).unwrap(),
        "0.24.0"
    );
    assert_eq!(
        Kind::Quilt.profile_id("0.19.2", "1.21.4"),
        "quilt-loader-0.19.2-1.21.4"
    );
    assert_eq!(Kind::parse(" Fabric ").unwrap(), Kind::Fabric);
    assert!(Kind::parse("forge").is_err());
}

#[test]
fn maven_names_are_paths() {
    assert_eq!(
        maven_path("net.fabricmc:fabric-loader:0.16.9").unwrap(),
        "net/fabricmc/fabric-loader/0.16.9/fabric-loader-0.16.9.jar"
    );
    assert_eq!(
        maven_path("org.lwjgl:lwjgl:3.3.3:natives-macos@zip").unwrap(),
        "org/lwjgl/lwjgl/3.3.3/lwjgl-3.3.3-natives-macos.zip"
    );
    assert!(maven_path("just-a-name").is_err());
}

#[test]
fn loader_versions_reuse_verified_cache_offline_and_refresh_when_online() {
    let cache = std::env::temp_dir().join(format!(
        "horizon-loader-version-cache-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&cache);
    let old = br#"[{"loader":{"version":"0.18.1-beta.1","stable":false}},
                  {"loader":{"version":"0.17.0","stable":true}}]"#;
    let newer = br#"[{"loader":{"version":"0.18.0","stable":true}}]"#;
    let online = |url: &str| {
        assert_eq!(url, "https://meta.fabricmc.net/v2/versions/loader/1.21.4");
        Ok(old.to_vec())
    };
    let versions = versions_in(Kind::Fabric, "1.21.4", &cache, &online).unwrap();
    assert_eq!(pick(Kind::Fabric, &versions, None).unwrap(), "0.17.0");
    let offline = |_: &str| Err("no network".to_string());
    let cached = versions_in(Kind::Fabric, "1.21.4", &cache, &offline).unwrap();
    assert_eq!(cached, versions);
    // An authoritative empty list is not a network error and must override stale metadata.
    assert!(versions_in(Kind::Fabric, "1.21.4", &cache, &|_| Ok(b"[]".to_vec()))
        .unwrap_err()
        .contains("no loader for Minecraft 1.21.4"));
    // An empty response also retires the previous positive cache: an offline request
    // must not resurrect a loader the metadata service explicitly stopped offering.
    assert_eq!(
        versions_in(Kind::Fabric, "1.21.4", &cache, &offline).unwrap_err(),
        "no network"
    );
    // A successful response must update the offline selection, never keep stale versions.
    let refreshed = versions_in(Kind::Fabric, "1.21.4", &cache, &|_| Ok(newer.to_vec())).unwrap();
    assert_eq!(pick(Kind::Fabric, &refreshed, None).unwrap(), "0.18.0");
    assert_eq!(
        pick(
            Kind::Fabric,
            &versions_in(Kind::Fabric, "1.21.4", &cache, &offline).unwrap(),
            None,
        )
        .unwrap(),
        "0.18.0"
    );
    // One loader's list must not be reused for another loader or Minecraft version.
    assert_eq!(
        versions_in(Kind::Quilt, "1.21.4", &cache, &offline).unwrap_err(),
        "no network"
    );
    assert_eq!(
        versions_in(Kind::Fabric, "1.20.1", &cache, &offline).unwrap_err(),
        "no network"
    );
    fs::remove_dir_all(&cache).unwrap();
}

#[test]
fn invalid_loader_cache_cannot_invent_a_version() {
    let cache = std::env::temp_dir().join(format!(
        "horizon-loader-version-invalid-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&cache);
    fs::create_dir_all(&cache).unwrap();
    let file = version_cache_file(Kind::Quilt, "26.3", &cache);
    fs::write(&file, b"not valid metadata").unwrap();
    let offline = |_: &str| Err("no network".to_string());
    assert_eq!(
        versions_in(Kind::Quilt, "26.3", &cache, &offline).unwrap_err(),
        "no network"
    );
    let fresh = br#"[{"loader":{"version":"0.28.0"}},{"loader":{"version":"0.29.0-beta.1"}}]"#;
    let valid = versions_in(Kind::Quilt, "26.3", &cache, &|_| Ok(fresh.to_vec())).unwrap();
    assert_eq!(pick(Kind::Quilt, &valid, None).unwrap(), "0.28.0");
    // A corrupt online result may use a previously checked cache, not its invalid data.
    let fallback = versions_in(Kind::Quilt, "26.3", &cache, &|_| Ok(b"{".to_vec())).unwrap();
    assert_eq!(fallback, valid);
    fs::write(&file, vec![b'x'; MAX_VERSION_LIST_BYTES + 1]).unwrap();
    assert_eq!(
        versions_in(Kind::Quilt, "26.3", &cache, &offline).unwrap_err(),
        "no network"
    );
    fs::remove_dir_all(&cache).unwrap();
}

#[test]
fn a_profile_comes_with_its_libraries_checked() {
    let dir = std::env::temp_dir().join(format!("horizon-loaders-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let (loader_jar, asm_jar) = (b"loader".to_vec(), b"asm".to_vec());
    let profile = json!({
        "id": "fabric-loader-0.16.9-1.21.4", "inheritsFrom": "1.21.4",
        "mainClass": "net.fabricmc.loader.impl.launch.knot.KnotClient",
        "libraries": [
            {"name": "net.fabricmc:fabric-loader:0.16.9", "url": "https://maven.fabricmc.net/",
             "sha1": sha1_hex(&loader_jar)},
            {"name": "org.ow2.asm:asm:9.7", "url": "https://maven.fabricmc.net"}
        ]
    });
    let asm = "https://maven.fabricmc.net/org/ow2/asm/asm/9.7/asm-9.7.jar";
    let mut served: HashMap<String, Vec<u8>> = HashMap::from([
        (
            "https://meta.fabricmc.net/v2/versions/loader/1.21.4/0.16.9/profile/json".into(),
            profile.to_string().into_bytes(),
        ),
        (
            "https://maven.fabricmc.net/net/fabricmc/fabric-loader/0.16.9/fabric-loader-0.16.9.jar"
                .into(),
            loader_jar,
        ),
        (asm.into(), asm_jar.clone()),
        (
            format!("{asm}.sha1"),
            format!("{}  asm-9.7.jar", sha1_hex(&asm_jar)).into_bytes(),
        ),
    ]);
    let loader = Loader {
        kind: Kind::Fabric,
        version: "0.16.9".into(),
    };
    let install_with = |served: &HashMap<String, Vec<u8>>, minecraft: &str| {
        let fetch = |url: &str| served.get(url).cloned().ok_or(format!("404 {url}"));
        install(&loader, minecraft, &dir, &fetch, &|_| {})
    };
    let id = install_with(&served, "1.21.4").unwrap();
    assert_eq!(id, Kind::Fabric.profile_id("0.16.9", "1.21.4"));
    let kept: Value =
        serde_json::from_slice(&fs::read(dir.join(format!("versions/{id}/{id}.json"))).unwrap())
            .unwrap();
    assert_eq!(
        kept["libraries"][1]["downloads"]["artifact"]["path"],
        "org/ow2/asm/asm/9.7/asm-9.7.jar"
    );
    assert_eq!(
        fs::read(dir.join("libraries/org/ow2/asm/asm/9.7/asm-9.7.jar")).unwrap(),
        b"asm"
    );
    // A profile for another version, or a file that isn't the one listed, is refused.
    served.insert(
        "https://meta.fabricmc.net/v2/versions/loader/1.20.1/0.16.9/profile/json".into(),
        profile.to_string().into_bytes(),
    );
    assert!(install_with(&served, "1.20.1")
        .unwrap_err()
        .contains("not Minecraft 1.20.1"));
    fs::remove_file(dir.join("libraries/org/ow2/asm/asm/9.7/asm-9.7.jar")).unwrap();
    served.insert(asm.into(), b"tampered".to_vec());
    assert!(install_with(&served, "1.21.4")
        .unwrap_err()
        .contains("asm-9.7.jar is not the one"));
    fs::remove_dir_all(&dir).unwrap();
}

/// The real services and repositories (network): `cargo test loaders -- --ignored`.
#[test]
#[ignore]
fn the_real_loaders_install() {
    for kind in ["fabric", "quilt"] {
        let dir =
            std::env::temp_dir().join(format!("horizon-loaders-{kind}-{}", std::process::id()));
        let loader = choose(kind, "1.21.4", None).unwrap();
        let id = install(&loader, "1.21.4", &dir, &crate::backend::get_bytes, &|l| {
            eprintln!("{l}")
        })
        .unwrap();
        assert_eq!(id, loader.kind.profile_id(&loader.version, "1.21.4"));
        let kept: Value = serde_json::from_slice(
            &fs::read(dir.join(format!("versions/{id}/{id}.json"))).unwrap(),
        )
        .unwrap();
        for library in kept["libraries"].as_array().unwrap() {
            let path = library["downloads"]["artifact"]["path"].as_str().unwrap();
            assert!(dir.join("libraries").join(path).is_file(), "{path}");
        }
        fs::remove_dir_all(&dir).unwrap();
    }
}
