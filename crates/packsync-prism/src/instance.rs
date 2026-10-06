//! Reading one Prism instance folder: game root, pack identity (`mmc-pack.json`), local settings
//! (`instance.cfg`) and the actionable errors for folders that are *almost* an instance.

use std::path::Path;

use packsync_core::error::{Error, Result};
use packsync_core::launcher::{ComponentInfo, LoaderInfo, LocalSettings, PackIdentity};
use serde::Deserialize;

use crate::discovery::{INSTANCE_CFG, PACK_JSON, list_instance_dirs, read_instance_cfg};
use crate::files::read_bounded;
use crate::qtini::{IniDoc, QtValue, parse_bool};

const MAX_PACK_JSON_BYTES: usize = 1024 * 1024;
const MAX_COMPONENTS: usize = 500;
const MAX_UID_BYTES: usize = 200;

/// Mirror of `MinecraftInstance::gameRoot()`: `.minecraft` only when it exists and `minecraft` does
/// not; **`minecraft` in every other case**, including when neither exists (Prism then creates
/// `minecraft`). `exists()` follows symlinks like `QFileInfo::exists`.
pub fn game_root_name(root: &Path) -> &'static str {
    let mc = root.join("minecraft").exists();
    let dot = root.join(".minecraft").exists();
    if dot && !mc { ".minecraft" } else { "minecraft" }
}

#[derive(Deserialize)]
struct PackFile {
    #[serde(rename = "formatVersion")]
    format_version: Option<i64>,
    components: Option<Vec<PackComponent>>,
}

#[derive(Deserialize)]
struct PackComponent {
    uid: Option<String>,
    version: Option<String>,
    #[serde(rename = "cachedVersion")]
    cached_version: Option<String>,
    disabled: Option<bool>,
}

/// Loader component uids, in priority order (a pack never legitimately has two of the first four).
const LOADERS: &[(&str, &str)] = &[
    ("net.neoforged", "neoforge"),
    ("net.minecraftforge", "forge"),
    ("net.fabricmc.fabric-loader", "fabric"),
    ("org.quiltmc.quilt-loader", "quilt"),
    ("com.mumfrey.liteloader", "liteloader"),
];
const LWJGL3IFY_PREFIX: &str = "me.eigenraven.lwjgl3ify.";

/// Parse `mmc-pack.json` into the pack identity. Every (first-seen) component is kept; the loader is
/// derived from the known loader uids.
pub fn identity_from_pack_json(bytes: &[u8]) -> Result<PackIdentity> {
    let pack: PackFile = serde_json::from_slice(bytes)
        .map_err(|e| Error::invalid("mmc-pack.json", format!("not valid JSON ({e}); the component list is damaged")))?;
    match pack.format_version {
        Some(1) => {}
        Some(v) => {
            return Err(Error::invalid(
                "mmc-pack.json",
                format!("unsupported formatVersion {v} (Prism 11 writes formatVersion 1)"),
            ));
        }
        None => return Err(Error::invalid("mmc-pack.json", "formatVersion is missing")),
    }
    let list = pack.components.ok_or_else(|| Error::invalid("mmc-pack.json", "the components list is missing"))?;
    if list.len() > MAX_COMPONENTS {
        return Err(Error::invalid("mmc-pack.json", format!("more than {MAX_COMPONENTS} components")));
    }
    let mut components: Vec<ComponentInfo> = Vec::with_capacity(list.len());
    let mut disabled: Vec<String> = Vec::new();
    for c in list {
        let Some(uid) = c.uid else {
            return Err(Error::invalid("mmc-pack.json", "a component has no uid"));
        };
        if uid.is_empty() || uid.len() > MAX_UID_BYTES || uid.chars().any(|ch| ch.is_control()) {
            return Err(Error::invalid("mmc-pack.json", "a component has an invalid uid"));
        }
        if components.iter().any(|x| x.uid == uid) {
            continue; // Prism ignores duplicate entries
        }
        if c.disabled == Some(true) {
            disabled.push(uid.clone());
        }
        let version = c.version.filter(|v| !v.is_empty()).or(c.cached_version.filter(|v| !v.is_empty()));
        components.push(ComponentInfo { uid, version });
    }
    let version_of = |uid: &str| components.iter().find(|c| c.uid == uid).and_then(|c| c.version.clone());
    let active = |uid: &str| components.iter().any(|c| c.uid == uid) && !disabled.iter().any(|d| d == uid);
    let mut loader = None;
    for (uid, kind) in LOADERS {
        if active(uid) {
            loader = Some(LoaderInfo { kind: (*kind).to_string(), version: version_of(uid).unwrap_or_default() });
            break;
        }
    }
    if loader.is_none()
        && let Some(c) = components.iter().find(|c| c.uid.starts_with(LWJGL3IFY_PREFIX) && !disabled.contains(&c.uid))
    {
        loader = Some(LoaderInfo { kind: "lwjgl3ify".into(), version: c.version.clone().unwrap_or_default() });
    }
    Ok(PackIdentity { minecraft_version: version_of("net.minecraft"), loader, components })
}

/// The settings of an `instance.cfg` that PackSync reads. Values are exactly what Prism sees.
pub struct CfgView<'a> {
    doc: IniDoc<'a>,
}

impl<'a> CfgView<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<CfgView<'a>> {
        Ok(CfgView { doc: IniDoc::parse(bytes)? })
    }

    pub fn doc(&self) -> &IniDoc<'a> {
        &self.doc
    }

    /// A string setting as Prism's `settings->get(key).toString()` returns it (empty when unset).
    pub fn string(&self, key: &str) -> Option<String> {
        self.doc.get(key).map(|v| v.prism_string())
    }

    pub fn plain(&self, key: &str) -> Option<String> {
        match self.doc.get(key)? {
            QtValue::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn flag(&self, key: &str) -> bool {
        self.string(key).map(|v| parse_bool(&v)).unwrap_or(false)
    }

    pub fn display_name(&self) -> Option<String> {
        self.plain("name").map(|n| n.trim().to_string()).filter(|n| !n.is_empty())
    }

    /// Instance-level Java/memory settings, honouring the `Override*` gates exactly like Prism's
    /// `OverrideSetting`: when a gate is off the instance value is ignored (the launcher-wide value
    /// applies, which we deliberately do not read).
    pub fn local_settings(&self) -> LocalSettings {
        let mut out = LocalSettings::default();
        if self.flag("OverrideJavaLocation") {
            out.java_path = self.plain("JavaPath").filter(|s| !s.is_empty());
        }
        if self.flag("OverrideMemory") {
            out.min_memory_mb = self.plain("MinMemAlloc").and_then(|s| s.trim().parse().ok());
            out.max_memory_mb = self.plain("MaxMemAlloc").and_then(|s| s.trim().parse().ok());
        }
        if self.flag("OverrideJavaArgs") {
            out.jvm_args = self.plain("JvmArgs");
        }
        out
    }
}

/// Why `root` is not a Prism instance folder, phrased as an instruction.
fn not_an_instance(root: &Path) -> Error {
    let shown = root.display();
    // 1. the game folder of an instance
    let name = root.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if (name == ".minecraft" || name == "minecraft")
        && let Some(parent) = root.parent()
        && parent.join(INSTANCE_CFG).is_file()
    {
        return Error::invalid(
            "selected folder",
            format!(
                "{shown} is the game folder of a Prism instance. Select the instance folder {} instead (the folder that contains instance.cfg and mmc-pack.json).",
                parent.display()
            ),
        );
    }
    // 2. a folder that contains instances (the `instances` folder, or a group folder)
    let found = list_instance_dirs(root);
    if !found.is_empty() {
        let mut names: Vec<String> = found.iter().take(5).map(|e| format!("\"{}\"", e.id)).collect();
        if found.len() > 5 {
            names.push(format!("and {} more", found.len() - 5));
        }
        return Error::invalid(
            "selected folder",
            format!(
                "{shown} is not a Prism instance, but it contains {} instance(s): {}. Select one of those folders instead.",
                found.len(),
                names.join(", ")
            ),
        );
    }
    // 3. the Prism data folder
    if root.join(crate::discovery::LAUNCHER_CFG).is_file() || root.join("instances").is_dir() {
        return Error::invalid(
            "selected folder",
            format!(
                "{shown} looks like the Prism Launcher data folder. Open its instances folder and select the folder of one instance."
            ),
        );
    }
    // 4. a Minecraft folder (parent of .minecraft, or the game folder of something else)
    if root.join(".minecraft").is_dir()
        || root.join("minecraft").is_dir()
        || root.join("saves").is_dir()
        || root.join("mods").is_dir()
    {
        return Error::invalid(
            "selected folder",
            format!(
                "{shown} looks like a plain Minecraft folder, not a Prism instance. In Prism, right-click the instance, choose Folder > Instance Folder, and select that folder: it contains instance.cfg and mmc-pack.json."
            ),
        );
    }
    Error::invalid(
        "selected folder",
        format!(
            "{shown} has no instance.cfg, so it is not a Prism instance folder. Select the folder that contains instance.cfg."
        ),
    )
}

/// Everything `describe` needs from a validated instance folder.
#[derive(Debug)]
pub struct InstanceSummary {
    pub name: String,
    pub game_root: &'static str,
    pub identity: PackIdentity,
}

/// Strict validation for manual selection: `instance.cfg` and a valid `mmc-pack.json` are required.
pub fn validate_instance(root: &Path) -> Result<InstanceSummary> {
    if !root.is_dir() {
        return Err(Error::invalid("selected folder", format!("{} is not a folder", root.display())));
    }
    if !root.join(INSTANCE_CFG).is_file() {
        return Err(not_an_instance(root));
    }
    let cfg_bytes = read_instance_cfg(root)?;
    let cfg = CfgView::parse(&cfg_bytes)?;
    let pack_path = root.join(PACK_JSON);
    if !pack_path.is_file() {
        return Err(Error::invalid(
            "Prism instance",
            format!(
                "{} has instance.cfg but no {PACK_JSON}, so Prism cannot tell which Minecraft version it uses. Open the instance once in Prism (it may repair itself) or restore the folder from a backup.",
                root.display()
            ),
        ));
    }
    let pack = read_bounded(&pack_path, MAX_PACK_JSON_BYTES)?;
    let identity = identity_from_pack_json(&pack).map_err(|e| match e {
        Error::Invalid { detail, .. } => Error::invalid(
            "Prism instance",
            format!(
                "{}: {detail}. Open the instance once in Prism or restore the folder from a backup.",
                pack_path.display()
            ),
        ),
        other => other,
    })?;
    let name = cfg
        .display_name()
        .or_else(|| root.file_name().and_then(|n| n.to_str()).map(|s| s.to_string()))
        .unwrap_or_else(|| "Unnamed Instance".to_string());
    Ok(InstanceSummary { name, game_root: game_root_name(root), identity })
}

/// Lenient variant for discovery: Prism lists every folder with an `instance.cfg`, even a broken
/// one, so we do too (with an empty identity when the pack file cannot be read).
pub fn summarize_leniently(root: &Path, folder: &str) -> InstanceSummary {
    let name = read_instance_cfg(root)
        .ok()
        .and_then(|b| CfgView::parse(&b).ok().and_then(|c| c.display_name()))
        .unwrap_or_else(|| folder.to_string());
    let identity = read_bounded(&root.join(PACK_JSON), MAX_PACK_JSON_BYTES)
        .ok()
        .and_then(|b| identity_from_pack_json(&b).ok())
        .unwrap_or_default();
    InstanceSummary { name, game_root: game_root_name(root), identity }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PACK: &str = r#"{
    "components": [
        {"cachedName": "LWJGL 3", "cachedVersion": "3.3.1", "cachedVolatile": true, "dependencyOnly": true, "uid": "org.lwjgl3", "version": "3.3.1"},
        {"cachedName": "Minecraft", "cachedRequires": [{"suggests": "3.3.1", "uid": "org.lwjgl3"}], "cachedVersion": "1.20.1", "important": true, "uid": "net.minecraft", "version": "1.20.1"},
        {"cachedName": "Forge", "cachedRequires": [{"equals": "1.20.1", "uid": "net.minecraft"}], "cachedVersion": "47.4.0", "uid": "net.minecraftforge", "version": "47.4.0"}
    ],
    "formatVersion": 1
}"#;

    #[test]
    fn game_root_mirrors_prisms_rule_including_the_neither_exists_case() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(game_root_name(d.path()), "minecraft", "neither exists -> minecraft");
        std::fs::create_dir(d.path().join(".minecraft")).unwrap();
        assert_eq!(game_root_name(d.path()), ".minecraft");
        std::fs::create_dir(d.path().join("minecraft")).unwrap();
        assert_eq!(game_root_name(d.path()), "minecraft", "both exist -> minecraft wins");
        std::fs::remove_dir(d.path().join(".minecraft")).unwrap();
        assert_eq!(game_root_name(d.path()), "minecraft");
        // a *file* named minecraft also counts as existing (QFileInfo::exists)
        let e = tempfile::tempdir().unwrap();
        std::fs::create_dir(e.path().join(".minecraft")).unwrap();
        std::fs::write(e.path().join("minecraft"), b"x").unwrap();
        assert_eq!(game_root_name(e.path()), "minecraft");
    }

    #[test]
    fn identity_keeps_every_component_and_derives_the_loader() {
        let id = identity_from_pack_json(PACK.as_bytes()).unwrap();
        assert_eq!(id.minecraft_version.as_deref(), Some("1.20.1"));
        assert_eq!(id.loader, Some(LoaderInfo { kind: "forge".into(), version: "47.4.0".into() }));
        let uids: Vec<&str> = id.components.iter().map(|c| c.uid.as_str()).collect();
        assert_eq!(uids, vec!["org.lwjgl3", "net.minecraft", "net.minecraftforge"]);
    }

    #[test]
    fn loader_uids_and_gtnh_style_lwjgl3ify_packs() {
        for (uid, kind) in [
            ("net.neoforged", "neoforge"),
            ("net.fabricmc.fabric-loader", "fabric"),
            ("org.quiltmc.quilt-loader", "quilt"),
            ("com.mumfrey.liteloader", "liteloader"),
        ] {
            let j = format!(
                r#"{{"formatVersion":1,"components":[{{"uid":"net.minecraft","version":"1.21.1"}},{{"uid":"{uid}","version":"9.9"}}]}}"#
            );
            let id = identity_from_pack_json(j.as_bytes()).unwrap();
            assert_eq!(id.loader.unwrap(), LoaderInfo { kind: kind.into(), version: "9.9".into() });
        }
        // GTNH: Forge 1.7.10 plus the lwjgl3ify patch components
        let gtnh = r#"{"formatVersion":1,"components":[
            {"uid":"net.minecraft","version":"1.7.10"},
            {"uid":"net.minecraftforge","version":"10.13.4.1614"},
            {"uid":"me.eigenraven.lwjgl3ify.forgepatches","version":"1.0.0"},
            {"uid":"org.lwjgl3","version":"3.3.3"}]}"#;
        let id = identity_from_pack_json(gtnh.as_bytes()).unwrap();
        assert_eq!(id.loader.as_ref().unwrap().kind, "forge");
        assert_eq!(id.components.len(), 4, "the lwjgl3ify component is kept");
        // lwjgl3ify alone is reported as such rather than dropped
        let only = r#"{"formatVersion":1,"components":[{"uid":"net.minecraft","version":"1.7.10"},{"uid":"me.eigenraven.lwjgl3ify.forgepatches","version":"2.0"}]}"#;
        assert_eq!(identity_from_pack_json(only.as_bytes()).unwrap().loader.unwrap().kind, "lwjgl3ify");
        // vanilla
        let v = r#"{"formatVersion":1,"components":[{"uid":"net.minecraft","version":"1.20.1"}]}"#;
        assert!(identity_from_pack_json(v.as_bytes()).unwrap().loader.is_none());
        // disabled loader components are not the loader
        let dis = r#"{"formatVersion":1,"components":[{"uid":"net.minecraft","version":"1.20.1"},{"uid":"net.minecraftforge","version":"47.4.0","disabled":true}]}"#;
        assert!(identity_from_pack_json(dis.as_bytes()).unwrap().loader.is_none());
        // cachedVersion is the fallback, duplicates collapse like in Prism
        let c = r#"{"formatVersion":1,"components":[{"uid":"net.minecraft","cachedVersion":"1.19.2"},{"uid":"net.minecraft","version":"9"}]}"#;
        let id = identity_from_pack_json(c.as_bytes()).unwrap();
        assert_eq!(id.minecraft_version.as_deref(), Some("1.19.2"));
        assert_eq!(id.components.len(), 1);
    }

    #[test]
    fn corrupt_pack_files_are_rejected_with_clear_reasons() {
        for (bad, needle) in [
            ("not json", "not valid JSON"),
            ("[]", "not valid JSON"),
            ("{}", "formatVersion is missing"),
            (r#"{"formatVersion":2,"components":[]}"#, "unsupported formatVersion 2"),
            (r#"{"formatVersion":1}"#, "components list is missing"),
            (r#"{"formatVersion":1,"components":[{"version":"1"}]}"#, "no uid"),
            (r#"{"formatVersion":1,"components":[{"uid":""}]}"#, "invalid uid"),
            (r#"{"formatVersion":1,"components":[{"uid":"a\nb"}]}"#, "invalid uid"),
        ] {
            let e = identity_from_pack_json(bad.as_bytes()).unwrap_err().to_string();
            assert!(e.contains(needle), "{bad:?}: {e}");
        }
        let many: Vec<String> = (0..501).map(|i| format!(r#"{{"uid":"c{i}"}}"#)).collect();
        let big = format!(r#"{{"formatVersion":1,"components":[{}]}}"#, many.join(","));
        assert!(identity_from_pack_json(big.as_bytes()).unwrap_err().to_string().contains("more than"));
    }

    #[test]
    fn local_settings_honour_the_override_gates() {
        let cfg = "[General]\nJavaPath=/opt/java 17/bin/java\nMinMemAlloc=1024\nMaxMemAlloc=8192\nJvmArgs=-XX:+UseZGC -Dfoo=bar\nOverrideJavaLocation=true\nOverrideMemory=false\nOverrideJavaArgs=true\n";
        let v = CfgView::parse(cfg.as_bytes()).unwrap();
        let s = v.local_settings();
        assert_eq!(s.java_path.as_deref(), Some("/opt/java 17/bin/java"));
        assert_eq!(s.min_memory_mb, None, "memory gate is off");
        assert_eq!(s.max_memory_mb, None);
        assert_eq!(s.jvm_args.as_deref(), Some("-XX:+UseZGC -Dfoo=bar"));
        let cfg = "[General]\nOverrideMemory=true\nMinMemAlloc=512\nMaxMemAlloc=\"6144\"\n";
        let s = CfgView::parse(cfg.as_bytes()).unwrap().local_settings();
        assert_eq!((s.min_memory_mb, s.max_memory_mb), (Some(512), Some(6144)));
        assert!(CfgView::parse(b"").unwrap().local_settings() == LocalSettings::default());
    }

    fn make(root: &Path, game: Option<&str>, pack: Option<&str>) {
        std::fs::create_dir_all(root).unwrap();
        std::fs::write(root.join("instance.cfg"), "[General]\nConfigVersion=1.3\nInstanceType=OneSix\nname=Ünï Pack\n")
            .unwrap();
        if let Some(p) = pack {
            std::fs::write(root.join("mmc-pack.json"), p).unwrap();
        }
        if let Some(g) = game {
            std::fs::create_dir_all(root.join(g)).unwrap();
        }
    }

    #[test]
    fn validate_accepts_both_layouts_and_reports_the_display_name() {
        let d = tempfile::tempdir().unwrap();
        let a = d.path().join("A pack (1)");
        make(&a, Some(".minecraft"), Some(PACK));
        let s = validate_instance(&a).unwrap();
        assert_eq!((s.name.as_str(), s.game_root), ("Ünï Pack", ".minecraft"));
        let b = d.path().join("B");
        make(&b, Some("minecraft"), Some(PACK));
        assert_eq!(validate_instance(&b).unwrap().game_root, "minecraft");
        let c = d.path().join("C");
        make(&c, None, Some(PACK));
        assert_eq!(validate_instance(&c).unwrap().game_root, "minecraft");
    }

    #[test]
    fn missing_or_corrupt_pack_json_is_actionable() {
        let d = tempfile::tempdir().unwrap();
        let a = d.path().join("a");
        make(&a, Some(".minecraft"), None);
        let e = validate_instance(&a).unwrap_err().to_string();
        assert!(e.contains("no mmc-pack.json") && e.contains("restore"), "{e}");
        std::fs::write(a.join("mmc-pack.json"), "{ truncated").unwrap();
        let e = validate_instance(&a).unwrap_err().to_string();
        assert!(e.contains("not valid JSON") && e.contains("Open the instance once in Prism"), "{e}");
    }

    #[test]
    fn selecting_the_wrong_folder_says_what_to_select() {
        let d = tempfile::tempdir().unwrap();
        let inst = d.path().join("instances");
        let a = inst.join("My Pack");
        make(&a, Some(".minecraft"), Some(PACK));
        make(&inst.join("Other"), Some("minecraft"), Some(PACK));
        // the game folder
        let e = validate_instance(&a.join(".minecraft")).unwrap_err().to_string();
        assert!(e.contains("game folder of a Prism instance") && e.contains(&a.display().to_string()), "{e}");
        // the instances folder
        let e = validate_instance(&inst).unwrap_err().to_string();
        assert!(e.contains("contains 2 instance(s)") && e.contains("\"My Pack\"") && e.contains("\"Other\""), "{e}");
        // the data folder
        std::fs::write(d.path().join("prismlauncher.cfg"), "[General]\n").unwrap();
        let e = validate_instance(d.path()).unwrap_err().to_string();
        assert!(e.contains("data folder") || e.contains("contains 2"), "{e}");
        let data = tempfile::tempdir().unwrap();
        std::fs::write(data.path().join("prismlauncher.cfg"), "[General]\n").unwrap();
        std::fs::create_dir_all(data.path().join("instances")).unwrap();
        let e = validate_instance(data.path()).unwrap_err().to_string();
        assert!(e.contains("data folder"), "{e}");
        // a plain minecraft directory and an empty one
        let plain = d.path().join("plain");
        std::fs::create_dir_all(plain.join("saves")).unwrap();
        let e = validate_instance(&plain).unwrap_err().to_string();
        assert!(e.contains("plain Minecraft folder") && e.contains("Instance Folder"), "{e}");
        let empty = d.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(validate_instance(&empty).unwrap_err().to_string().contains("no instance.cfg"));
        assert!(validate_instance(&d.path().join("missing")).unwrap_err().to_string().contains("not a folder"));
    }

    #[test]
    fn lenient_summary_never_fails() {
        let d = tempfile::tempdir().unwrap();
        let a = d.path().join("Broken ünï");
        make(&a, None, Some("not json"));
        let s = summarize_leniently(&a, "Broken ünï");
        assert_eq!(s.name, "Ünï Pack");
        assert!(s.identity.components.is_empty());
        std::fs::write(a.join("instance.cfg"), b"\xff\xfe garbage \x00").unwrap();
        assert_eq!(summarize_leniently(&a, "Broken ünï").name, "Broken ünï");
    }
}
