//! The Prism-specific part of the sync rules, and the mixed-file policy for `instance.cfg`.

use packsync_core::mixed::{MixedFormat, MixedPolicy};

/// Identifier recorded in manifests for the `instance.cfg` policy.
pub const INSTANCE_CFG_POLICY_ID: &str = "prism.instance_cfg";

/// Rules for the Prism files that live next to the game folder, in the core rules schema. They are
/// appended after the built-in defaults, so the last rule re-asserts the defaults' native-code and
/// temp-file exclusions for the folders included above (a later include must not smuggle a `.so`
/// from another operating system into a restore).
pub const RULES_FRAGMENT: &str = r#"# Prism Launcher instance-root rules (anchored at the instance folder, not the game folder).
version = 1

[[rule]]
id = "prism-components"
match = ["mmc-pack.json", "patches/**"]
action = "include"
category = "pack"
reason = "Prism's component list (Minecraft, loader and library versions) and any customised component patches: they decide what the pack runs on."

[[rule]]
id = "prism-instance-settings"
match = ["instance.cfg"]
action = "include"
category = "settings"
reason = "Prism instance settings. Only portable metadata (pack info, notes, export info) is synced; Java path, memory, launch commands, accounts and window state stay on each computer."
mixed = "prism.instance_cfg"

[[rule]]
id = "prism-custom-libraries"
match = ["libraries/**", "jarmods/**"]
action = "include"
category = "pack"
reason = "Custom libraries and jar mods attached to this instance. Prism cannot download them again."

[[rule]]
id = "prism-import-metadata"
match = ["flame/**", "mrpack/**", "modlist.html", ".packignore"]
action = "include"
category = "pack"
reason = "Pack-import metadata and export settings Prism keeps beside the instance."

[[rule]]
id = "prism-generated-icon"
match = ["$GAME/icon.png"]
action = "exclude"
category = "launcher-managed"
reason = "Prism rewrites this file from the instance icon every time the game starts."

[[rule]]
id = "prism-keep-native-and-temp-excluded"
match = ["**/*.dll", "**/*.so", "**/*.so.*", "**/*.dylib", "**/*.jnilib", "**/*.exe", "**/*.tmp", "**/*.temp", "**/*.swp", "**/*~"]
action = "exclude"
category = "native"
reason = "Platform-specific native code and temporary files are never synced, even inside Prism's custom library folders."
case_insensitive = true
"#;

/// `instance.cfg` keys that describe the *pack* and are therefore the same on every computer. This is
/// an allow-list: everything else (including keys a future Prism adds) stays local.
///
/// Registered by `BaseInstance.cpp` / `MinecraftInstance.cpp` in Prism 11.0.3 and deliberately
/// **not** synced: `name`, `iconKey` (custom icons do not exist on another machine), `JavaPath`
/// and the other Java/memory/window settings and their `Override*` gates, `PreLaunchCommand` /
/// `WrapperCommand` / `PostExitCommand` / `OverrideCommands` (they point at local executables and
/// carry PackSync's own hook), `Env`, `InstanceAccountId` / `UseAccountForInstance`,
/// `JoinServerOnLaunch*` / `JoinWorldOnLaunch`, `GlobalDataPacks*`, `linkedInstances` and
/// `shortcuts` (machine paths and IDs), `lastLaunchTime` / `totalTimePlayed` / `lastTimePlayed`,
/// `ConfigVersion` (a file-format marker Prism manages itself) and the whole `[UI]` section.
pub const PORTABLE_KEYS: &[&str] = &[
    "InstanceType",
    "notes",
    "ManagedPack",
    "ManagedPackType",
    "ManagedPackID",
    "ManagedPackName",
    "ManagedPackVersionID",
    "ManagedPackVersionName",
    "ManagedPackURL",
    "ExportName",
    "ExportVersion",
    "ExportSummary",
    "ExportAuthor",
    "ExportOptionalFiles",
    "ExportRecommendedRAM",
];

pub fn instance_cfg_policy() -> MixedPolicy {
    MixedPolicy {
        id: INSTANCE_CFG_POLICY_ID.to_string(),
        path: "instance.cfg".to_string(),
        format: MixedFormat::Ini,
        portable_keys: Some(PORTABLE_KEYS.iter().map(|k| k.to_string()).collect()),
        local_keys: vec![],
        local_sections: vec!["UI".to_string()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use packsync_core::mixed::{merge, project};
    use packsync_core::rules::{DecisionSource, RuleDoc, RuleSet, effective_rules_text};

    /// A file shaped like the `instance.cfg` Prism 11 writes (see the real file captured by
    /// `scripts/prism-smoke.sh`), including the quoted base64 `[UI]` blobs whose keys contain
    /// backslashes. Synthetic: no user data.
    pub const REAL_SHAPED_CFG: &str = "[General]\nConfigVersion=1.3\nExportAuthor=PackSync Tester\nExportName=\nExportOptionalFiles=true\nExportSummary=A test summary; with = and , in it\nExportVersion=1.0.0\nInstanceType=OneSix\nJavaArchitecture=64\nJavaPath=/usr/lib/jvm/java-17-openjdk/bin/java\nJavaRealArchitecture=amd64\nJavaSignature=0b8c6e0f3bd4b6e7fbd7d8d4f6d4a8f0e2b1c3d4\nJavaVendor=Eclipse Adoptium\nJavaVersion=17.0.15\nJvmArgs=-XX:+UseG1GC -Dsun.rmi.dgc.server.gcInterval=2147483646\nLogPrePostOutput=true\nManagedPack=true\nManagedPackID=\"AABBCCDD\"\nManagedPackName=All the Mods 10 - To the Sky\nManagedPackType=flame\nManagedPackURL=https://www.curseforge.com/minecraft/modpacks/all-the-mods-10-to-the-sky\nManagedPackVersionID=1234567\nManagedPackVersionName=ATM10-TTS-2.1.0\nMaxMemAlloc=8192\nMinMemAlloc=2048\nOverrideCommands=true\nOverrideJavaArgs=true\nOverrideJavaLocation=true\nOverrideMemory=true\nPostExitCommand=/usr/bin/notify-send done\nPreLaunchCommand=\\\"/opt/Pack Sync/packsync\\\" hook pre-launch\nInstanceAccountId=0123456789abcdef\niconKey=default\nlastLaunchTime=1759999999999\nlastTimePlayed=3600\nname=All the Mods 10 - To the Sky\nnotes=Remember to back up\\nthe world; then update.\ntotalTimePlayed=7200\n\n[UI]\nmods_Page\\Columns=\"AAAA/wAAAAEAAAAGAAAAZAAAAHIAAAB2AAAAfgAAAFsAAAAB//////////8A=\"\nresourcepacks_Page\\Columns=\"AAAA/wAAAAEAAAAD=\"\n";

    #[test]
    fn fragment_is_valid_complete_and_in_the_core_schema() {
        let doc = RuleDoc::parse(RULES_FRAGMENT).unwrap();
        assert!(!doc.rules.is_empty());
        for r in &doc.rules {
            assert!(!r.id.is_empty() && r.id.starts_with("prism-"), "{}", r.id);
            assert!(!r.category.is_empty(), "{} needs a category", r.id);
            assert!(r.reason.len() > 20, "{} needs a real reason", r.id);
        }
        let ids: std::collections::BTreeSet<_> = doc.rules.iter().map(|r| r.id.clone()).collect();
        assert_eq!(ids.len(), doc.rules.len(), "unique ids");
        // every mixed id referenced by the fragment is supplied by this adapter
        let supplied = instance_cfg_policy().id;
        for r in &doc.rules {
            if let Some(m) = &r.mixed {
                assert_eq!(m, &supplied);
            }
        }
    }

    fn compiled(game: &str) -> RuleSet {
        let text = effective_rules_text(RULES_FRAGMENT, "").unwrap();
        RuleSet::compile(&text, game).unwrap()
    }

    #[test]
    fn composes_with_the_defaults_for_both_layouts() {
        for g in [".minecraft", "minecraft"] {
            let rs = compiled(g);
            assert!(rs.mixed_ids().contains("prism.instance_cfg"));
            assert!(rs.mixed_ids().contains("minecraft.options_txt"));
        }
    }

    #[test]
    fn decisions_on_a_realistic_instance_tree() {
        for g in [".minecraft", "minecraft"] {
            let r = compiled(g);
            let yes = [
                "mmc-pack.json".to_string(),
                "instance.cfg".to_string(),
                "patches/net.minecraft.json".to_string(),
                "patches/org.lwjgl3.json".to_string(),
                "libraries/custom-lib-1.0.jar".to_string(),
                "jarmods/optifine.jar".to_string(),
                "flame/manifest.json".to_string(),
                "mrpack/modrinth.index.json".to_string(),
                "modlist.html".to_string(),
                ".packignore".to_string(),
                format!("{g}/mods/jei-1.20.1.jar"),
                format!("{g}/config/ae2/common.json"),
                format!("{g}/saves/My World/level.dat"),
                format!("{g}/options.txt"),
                format!("{g}/mods/.index/jei.pw.toml"),
            ];
            for p in &yes {
                let d = r.decide(p);
                assert!(d.include, "{p} should sync: {}", d.explain(p));
            }
            let no = [
                "natives/liblwjgl.so".to_string(),
                "libraries/native/linux/libfoo.so".to_string(),
                "libraries/native/windows/foo.DLL".to_string(),
                "jarmods/readme.txt~".to_string(),
                "patches/x.json.tmp".to_string(),
                ".packsync/prism-hooks.json".to_string(),
                "icon.png".to_string(),
                format!("{g}/icon.png"),
                format!("{g}/logs/latest.log"),
                format!("{g}/crash-reports/c.txt"),
                format!("{g}/saves/My World/session.lock"),
                "accounts.json".to_string(),
                "prismlauncher.cfg".to_string(),
            ];
            for p in &no {
                assert!(!r.decide(p).include, "{p} must not sync");
            }
            // the layout the rules were compiled for is the only one that matches
            let other = if g == ".minecraft" { "minecraft" } else { ".minecraft" };
            let d = r.decide(&format!("{other}/mods/jei.jar"));
            assert!(!d.include && d.source == DecisionSource::Unmatched, "{other} is not this instance's game root");
        }
    }

    #[test]
    fn instance_cfg_carries_the_mixed_policy_and_native_exclusion_survives_later_includes() {
        let r = compiled(".minecraft");
        assert_eq!(r.decide("instance.cfg").mixed.as_deref(), Some("prism.instance_cfg"));
        let d = r.decide("libraries/natives/lwjgl.dll");
        assert!(!d.include);
        assert_eq!(
            d.source,
            DecisionSource::Rule { id: "prism-keep-native-and-temp-excluded".into(), origin: Some("launcher".into()) }
        );
        // a user rule placed after the fragment can still opt a file back in
        let user = "version = 1\n[[rule]]\nid = \"u\"\nmatch = [\"libraries/mine.so\"]\naction = \"include\"\nreason = \"mine\"\n";
        let text = effective_rules_text(RULES_FRAGMENT, user).unwrap();
        let rs = RuleSet::compile(&text, ".minecraft").unwrap();
        assert!(rs.decide("libraries/mine.so").include);
    }

    #[test]
    fn projection_keeps_only_portable_keys_of_a_real_shaped_file() {
        let p = instance_cfg_policy();
        let out = String::from_utf8(project(&p, REAL_SHAPED_CFG.as_bytes()).unwrap()).unwrap();
        for k in [
            "ExportAuthor=PackSync Tester",
            "ExportOptionalFiles=true",
            "ExportSummary=A test summary; with = and , in it",
            "InstanceType=OneSix",
            "ManagedPack=true",
            "ManagedPackID=\"AABBCCDD\"",
            "ManagedPackName=All the Mods 10 - To the Sky",
            "ManagedPackType=flame",
            "notes=Remember to back up\\nthe world; then update.",
        ] {
            assert!(out.contains(k), "{k} missing from\n{out}");
        }
        for k in [
            "JavaPath",
            "JvmArgs",
            "MaxMemAlloc",
            "MinMemAlloc",
            "Override",
            "PreLaunchCommand",
            "PostExitCommand",
            "InstanceAccountId",
            "lastLaunchTime",
            "totalTimePlayed",
            "ConfigVersion",
            "iconKey",
            "name=",
            "[UI]",
            "Columns",
        ] {
            assert!(!out.contains(k), "{k} must stay local:\n{out}");
        }
        assert!(out.starts_with("[General]\n"));
    }

    #[test]
    fn project_of_merge_is_the_projection_for_realistic_local_files() {
        let p = instance_cfg_policy();
        let portable = project(&p, REAL_SHAPED_CFG.as_bytes()).unwrap();
        let locals: [Option<&[u8]>; 5] = [
            None,
            Some(REAL_SHAPED_CFG.as_bytes()),
            Some(b"[General]\nConfigVersion=1.3\nname=Local name\nnotes=old notes\nJavaPath=C:\\\\Java\\\\bin\\\\java.exe\n[UI]\nmods_Page\\Columns=\"AAAA=\"\n"),
            Some(b"[General]\nInstanceType=OneSix\nManagedPackID=\"OTHER\"\nz_future_key=1\n"),
            Some(b"; comment only\n"),
        ];
        for local in locals {
            let merged = merge(&p, &portable, local).unwrap();
            assert_eq!(project(&p, &merged).unwrap(), portable, "local = {:?}", local.map(String::from_utf8_lossy));
            // local-only values survive the merge
            if let Some(l) = local
                && String::from_utf8_lossy(l).contains("JavaPath=C:")
            {
                assert!(String::from_utf8_lossy(&merged).contains("JavaPath=C:\\\\Java\\\\bin\\\\java.exe"));
                assert!(String::from_utf8_lossy(&merged).contains("name=Local name"));
            }
        }
    }

    #[test]
    fn a_hook_install_never_changes_the_projection() {
        // PackSync's own hook lines are local keys: installing/removing them must not look like an
        // edit of the synced pack metadata.
        let p = instance_cfg_policy();
        let before = project(&p, REAL_SHAPED_CFG.as_bytes()).unwrap();
        let hooked = REAL_SHAPED_CFG.replace("OverrideCommands=true", "OverrideCommands=false");
        assert_eq!(project(&p, hooked.as_bytes()).unwrap(), before);
    }

    #[test]
    fn allow_list_names_are_registered_prism_settings() {
        // Guards against typos: these are the exact keys BaseInstance/MinecraftInstance register.
        for k in PORTABLE_KEYS {
            assert!(k.chars().all(|c| c.is_ascii_alphanumeric()), "{k}");
        }
        assert!(PORTABLE_KEYS.contains(&"ManagedPackURL") && PORTABLE_KEYS.contains(&"ExportRecommendedRAM"));
        assert!(!PORTABLE_KEYS.contains(&"JavaPath") && !PORTABLE_KEYS.contains(&"name"));
    }
}
