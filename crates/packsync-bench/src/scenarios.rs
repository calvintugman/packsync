//! The measured scenarios. Every number is produced by running the real engine end to end against a
//! storage backend; nothing is estimated.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use packsync_core::engine::{
    EnableRequest, ReconcileOpts, ReconcileReport, RestoreNewRequest, SnapshotOpts, SnapshotResult,
};
use packsync_core::hash::Hash;
use packsync_core::testkit::fixtures::{self, FixtureInfo, FixtureSpec};
use packsync_core::testkit::harness::is_synced_path;
use serde::Serialize;
use serde_json::{Value, json};

use crate::lab::{Backend, Device, Lab, Metrics};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Size {
    Tiny,
    Small,
    Medium,
    Large,
}

impl Size {
    pub fn parse(s: &str) -> Result<Size, String> {
        match s {
            "tiny" => Ok(Size::Tiny),
            "small" => Ok(Size::Small),
            "medium" => Ok(Size::Medium),
            "large" => Ok(Size::Large),
            other => Err(format!("unknown size {other:?}: use tiny, small, medium or large")),
        }
    }
    pub fn spec(&self) -> FixtureSpec {
        match self {
            Size::Tiny => FixtureSpec::tiny(),
            Size::Small => FixtureSpec::small(),
            Size::Medium => FixtureSpec::medium(),
            Size::Large => FixtureSpec::large(),
        }
    }
    pub fn name(&self) -> &'static str {
        match self {
            Size::Tiny => "tiny",
            Size::Small => "small",
            Size::Medium => "medium",
            Size::Large => "large",
        }
    }
}

#[derive(Debug, Clone)]
pub struct RunOpts {
    pub size: Size,
    pub backend: Backend,
    pub work: PathBuf,
    pub region_block: Option<u32>,
    pub upload_concurrency: Option<usize>,
    pub verify: bool,
    pub keep: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct FixtureReport {
    pub size: String,
    pub files: u64,
    pub synced_files: u64,
    pub synced_bytes: u64,
    pub region_files: usize,
    pub region_bytes: u64,
    pub mod_files: usize,
    pub mod_bytes: u64,
    pub generation_secs: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Scenario {
    pub name: String,
    pub what: String,
    #[serde(flatten)]
    pub metrics: Metrics,
    pub details: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct Suite {
    pub backend: String,
    pub region_block: u32,
    pub upload_concurrency: usize,
    pub fixture: FixtureReport,
    pub scenarios: Vec<Scenario>,
    pub cloud_bytes_total: u64,
    pub baseline: Value,
}

fn total_size(files: &[PathBuf]) -> u64 {
    files.iter().map(|p| fs::metadata(p).map(|m| m.len()).unwrap_or(0)).sum()
}

fn dir_files(root: &Path) -> u64 {
    walkdir::WalkDir::new(root).into_iter().flatten().filter(|e| e.file_type().is_file()).count() as u64
}

fn digest(root: &Path) -> Vec<(String, Hash)> {
    let mut v = fixtures::tree_digest(root, is_synced_path);
    let policy = packsync_core::mixed::options_txt_policy("");
    for (p, h) in v.iter_mut() {
        if p == "options.txt" {
            *h = Hash::of(&packsync_core::mixed::project(&policy, &fs::read(root.join(&*p)).unwrap()).unwrap());
        }
    }
    v
}

pub fn make_fixture(work: &Path, size: Size) -> (PathBuf, FixtureInfo, f64) {
    let root = work.join("instance");
    let _ = fs::remove_dir_all(&root);
    let t = Instant::now();
    let info = fixtures::generate(&root, &size.spec());
    (root, info, t.elapsed().as_secs_f64())
}

// ---- edits (deterministic) --------------------------------------------------------------------

fn edit_small(root: &Path, seed: u64) -> u64 {
    let mut r = fastrand::Rng::with_seed(seed);
    let mut bytes = 0;
    for i in 0..10 {
        let text: String = (0..200)
            .map(|_| ["enabled", "true", "false", "recipe", "maxStack", "= "][r.usize(..6)])
            .collect::<Vec<_>>()
            .join(" ");
        let p = root.join(format!("config/mod{}/settings-{}.toml", i % 25, i * 3));
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, text.as_bytes()).unwrap();
        bytes += text.len() as u64;
    }
    let mut opts = fs::read(root.join("options.txt")).unwrap_or_default();
    opts.extend_from_slice(format!("\nrenderDistance:{}\n", 8 + r.u8(..24)).as_bytes());
    fs::write(root.join("options.txt"), &opts).unwrap();
    let mut servers = vec![0u8; 600];
    r.fill(&mut servers);
    fs::write(root.join("servers.dat"), &servers).unwrap();
    bytes + 600
}

/// What the game does between two saves: a handful of chunks change in a few region files.
fn edit_regions(regions: &[PathBuf], files: usize, chunks: usize, seed: u64) -> (u64, u64) {
    let (mut touched, mut region_bytes) = (0u64, 0u64);
    for (i, p) in regions.iter().take(files).enumerate() {
        region_bytes += fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        touched += fixtures::edit_region(p, chunks, seed + i as u64) as u64;
    }
    (touched, region_bytes)
}

fn replace_mods(mods: &[PathBuf], n: usize, seed: u64) -> u64 {
    let mut r = fastrand::Rng::with_seed(seed);
    let mut bytes = 0;
    for p in mods.iter().take(n) {
        let len = fs::metadata(p).map(|m| m.len()).unwrap_or(1000) as usize;
        let mut data = vec![0u8; len];
        r.fill(&mut data);
        fs::write(p, &data).unwrap();
        bytes += len as u64;
    }
    bytes
}

fn rename_mods(mods: &[PathBuf], from: usize, n: usize) -> u64 {
    let mut bytes = 0;
    for p in mods.iter().skip(from).take(n) {
        let new = p.with_file_name(format!("renamed-{}", p.file_name().unwrap().to_string_lossy()));
        bytes += fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        fs::rename(p, new).unwrap();
    }
    bytes
}

// ---- helpers -------------------------------------------------------------------------------------

fn created(r: &SnapshotResult) -> bool {
    matches!(r, SnapshotResult::Created { .. })
}

fn scenario(name: &str, what: &str, m: Metrics, details: Value) -> Scenario {
    println!(
        "  {name:<28} {:>8.2} s  up {:>9}  down {:>9}  requests {:>5}  rss {:>6.0} MB",
        m.secs,
        human(m.storage.bytes_up),
        human(m.storage.bytes_down),
        m.storage.requests,
        m.peak_rss_mb
    );
    Scenario { name: name.into(), what: what.into(), metrics: m, details }
}

pub fn human(n: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let (mut v, mut u) = (n as f64, 0);
    while v >= 1000.0 && u < U.len() - 1 {
        v /= 1000.0;
        u += 1;
    }
    if u == 0 { format!("{n} B") } else { format!("{v:.1} {}", U[u]) }
}

struct Counting<W: Write>(W, u64);
impl<W: Write> Write for Counting<W> {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        let n = self.0.write(b)?;
        self.1 += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

/// What "just upload a fresh archive every time" costs: all synced files through zstd level 3 (a
/// conservative stand-in for a ZIP: zstd is faster and smaller than deflate).
fn baseline_archive(root: &Path, paths: &[String]) -> Value {
    let t = Instant::now();
    let mut enc = zstd::Encoder::new(Counting(std::io::sink(), 0), 3).unwrap();
    let mut raw = 0u64;
    for rel in paths {
        if let Ok(mut f) = fs::File::open(root.join(rel)) {
            raw += std::io::copy(&mut f, &mut enc).unwrap_or(0);
        }
    }
    let c = enc.finish().unwrap();
    json!({ "what": "full archive of the synced files (zstd level 3 stream), re-uploaded after every change", "raw_bytes": raw, "archive_bytes": c.1, "secs": t.elapsed().as_secs_f64() })
}

fn snapshot_and_upload(
    a: &Device,
    id: packsync_core::ids::InstanceId,
) -> (packsync_core::engine::SnapshotReport, packsync_core::engine::UploadSummary) {
    let rep = a.engine.snapshot(id, SnapshotOpts::default(), &a.ctx()).unwrap();
    let up = a.engine.upload_pending(id, true, &a.ctx()).unwrap();
    (rep, up)
}

pub fn run_suite(o: &RunOpts) -> Suite {
    println!("Generating the {} fixture under {} …", o.size.name(), o.work.display());
    let (root, info, gen_secs) = make_fixture(&o.work, o.size);
    let fx = FixtureReport {
        size: o.size.name().into(),
        files: dir_files(&root),
        synced_files: info.synced_files,
        synced_bytes: info.synced_bytes,
        region_files: info.regions.len(),
        region_bytes: total_size(&info.regions),
        mod_files: info.mods.len(),
        mod_bytes: total_size(&info.mods),
        generation_secs: gen_secs,
    };
    println!(
        "  {} files, {} synced ({}), {} region files ({}), {} mods ({}); generated in {:.1} s",
        fx.files,
        fx.synced_files,
        human(fx.synced_bytes),
        fx.region_files,
        human(fx.region_bytes),
        fx.mod_files,
        human(fx.mod_bytes),
        gen_secs
    );

    let lab = Lab::new(&o.work.join("lab"), o.backend);
    let defaults = packsync_core::engine::EngineConfig::new("x");
    let region_block = o.region_block.unwrap_or(defaults.snapshot.chunking.region_block);
    let conc = o.upload_concurrency.unwrap_or(defaults.upload_concurrency);
    let tweak = move |c: &mut packsync_core::engine::EngineConfig| {
        c.snapshot.chunking.region_block = region_block;
        c.upload_concurrency = conc;
        c.restore.concurrency = conc;
    };
    let a = lab.device("A", tweak);
    let mut sc: Vec<Scenario> = vec![];
    println!("Scenarios ({}):", lab.backend.name());

    // ---- initial sync -------------------------------------------------------------------------------
    let (en, m) = a.measure(|| {
        a.engine
            .enable_sync(
                EnableRequest {
                    root: root.clone(),
                    launcher: "generic".into(),
                    name: Some("bench".into()),
                    rules_overrides: String::new(),
                    decisions: vec![],
                    launcher_ref: None,
                },
                &a.ctx(),
            )
            .unwrap()
    });
    let id = en.instance;
    sc.push(scenario(
        "initial: scan+pack (local)",
        "first snapshot: walk, hash, chunk, compress, pack — nothing is sent yet",
        m,
        json!({ "stats": en.stats }),
    ));
    let (up, m) = a.measure(|| a.engine.upload_pending(id, true, &a.ctx()).unwrap());
    sc.push(scenario(
        "initial: upload",
        "send packs, index and manifest, verify, publish",
        m,
        json!({ "bytes_uploaded": up.bytes_uploaded, "snapshots": up.snapshots.len() }),
    ));

    // ---- nothing changed ----------------------------------------------------------------------------
    let (pending, m) = a.measure(|| a.engine.pending_changes(id, &a.ctx()).unwrap());
    sc.push(scenario(
        "no change: scan (warm cache)",
        "is the instance dirty? stat every file, hash only what the cache cannot vouch for",
        m,
        json!({ "dirty": pending.dirty, "files": fx.synced_files }),
    ));
    let (rep, m) = a.measure(|| a.engine.snapshot(id, SnapshotOpts::default(), &a.ctx()).unwrap());
    assert!(!created(&rep.result), "an unchanged instance must not create a snapshot");
    sc.push(scenario(
        "no change: snapshot",
        "snapshot of an unchanged instance (creates nothing)",
        m,
        json!({ "hashed_files": rep.stats.hashed_files, "reused_files": rep.stats.reused_files }),
    ));
    let (r, m) = a.measure(|| a.engine.reconcile(id, ReconcileOpts::default(), &a.ctx()).unwrap());
    assert!(matches!(r, ReconcileReport::UpToDate { .. }), "{r:?}");
    sc.push(scenario(
        "no change: cloud check",
        "ask the cloud whether anything is newer (what a launch does)",
        m,
        json!({}),
    ));
    a.engine.rescan(id).unwrap();
    let (rep, m) = a.measure(|| a.engine.snapshot(id, SnapshotOpts::default(), &a.ctx()).unwrap());
    sc.push(scenario("no change: cold rescan", "forced full re-hash of every file (after a restore, or the 14-day verification)", m, json!({ "hashed_files": rep.stats.hashed_files, "hashed_bytes": rep.stats.hashed_bytes, "result": format!("{:?}", rep.result).chars().take(12).collect::<String>() })));

    // ---- small edit ----------------------------------------------------------------------------------
    let touched = edit_small(&root, 11);
    let ((rep, up), m) = a.measure(|| snapshot_and_upload(&a, id));
    sc.push(scenario(
        "small edit (configs, options)",
        "10 config files, options.txt and servers.dat change",
        m,
        json!({ "bytes_modified": touched, "stats": rep.stats, "uploaded": up.bytes_uploaded }),
    ));

    // ---- region edit ---------------------------------------------------------------------------------
    let (files, chunks) = (info.regions.len().min(3), 6);
    let (touched, region_bytes) = edit_regions(&info.regions, files, chunks, 21);
    let ((rep, up), m) = a.measure(|| snapshot_and_upload(&a, id));
    let payload = m.storage.payload_bytes_up();
    sc.push(scenario(
        "region edit (game save)",
        &format!("{chunks} chunks rewritten in each of {files} region files"),
        m,
        json!({ "bytes_modified": touched, "region_file_bytes": region_bytes, "payload_bytes_uploaded": payload, "amplification": if touched > 0 { payload as f64 / touched as f64 } else { 0.0 }, "stats": rep.stats, "uploaded": up.bytes_uploaded }),
    ));

    // ---- mod replacement -----------------------------------------------------------------------------
    let replaced = replace_mods(&info.mods, 5, 31);
    let ((rep, up), m) = a.measure(|| snapshot_and_upload(&a, id));
    let payload = m.storage.payload_bytes_up();
    sc.push(scenario("mod replacement (5 jars)", "five mod jars replaced by new versions (incompressible)", m, json!({ "bytes_replaced": replaced, "payload_bytes_uploaded": payload, "stats": rep.stats, "uploaded": up.bytes_uploaded })));
    let renamed = rename_mods(&info.mods, 10, 5);
    let ((rep, up), m) = a.measure(|| snapshot_and_upload(&a, id));
    let payload = m.storage.payload_bytes_up();
    sc.push(scenario("mod rename (5 jars)", "five jars renamed, content unchanged: deduplicated, nothing to upload", m, json!({ "bytes_renamed": renamed, "payload_bytes_uploaded": payload, "stats": rep.stats, "uploaded": up.bytes_uploaded })));
    let latest = a.engine.instance(id).unwrap().base_snapshot.unwrap();

    // ---- restore on another computer -----------------------------------------------------------------
    let b = lab.device("B", tweak);
    let parent = o.work.join("restored");
    let _ = fs::remove_dir_all(&parent);
    fs::create_dir_all(&parent).unwrap();
    let (restored, m) = b.measure(|| {
        b.engine
            .restore_new(
                RestoreNewRequest {
                    instance: id,
                    snapshot: None,
                    parent_dir: parent.clone(),
                    folder_name: "bench".into(),
                    launcher: "generic".into(),
                    launcher_ref: None,
                    local: Default::default(),
                    skip_incompatible: false,
                },
                &b.ctx(),
            )
            .unwrap()
    });
    let identical = if o.verify { Some(digest(&restored.root) == digest(&root)) } else { None };
    sc.push(scenario("restore on a second computer", "download everything into a new instance, verify every blob and file", m, json!({ "written": restored.report.written, "bytes_downloaded": restored.report.bytes_downloaded, "identical_to_source": identical })));
    if let Some(false) = identical {
        panic!("the restored instance differs from the source: the benchmark found a bug");
    }
    // the next play session on A, then B catches up
    let (touched, _) = edit_regions(&info.regions, 2, 4, 41);
    edit_small(&root, 12);
    let ((_, _), _) = a.measure(|| snapshot_and_upload(&a, id));
    let (rec, m) = b.measure(|| b.engine.reconcile(id, ReconcileOpts::default(), &b.ctx()).unwrap());
    let identical = if o.verify { Some(digest(&restored.root) == digest(&root)) } else { None };
    sc.push(scenario("restore: catch up after a session", "B fast-forwards over one play session of A", m, json!({ "bytes_modified_by_session": touched, "result": format!("{rec:?}").chars().take(40).collect::<String>(), "identical_to_source": identical })));
    if let Some(false) = identical {
        panic!("the fast-forwarded instance differs from the source");
    }

    // ---- baseline ------------------------------------------------------------------------------------
    let manifest = a.engine.manifest(id, latest, &a.ctx()).unwrap();
    let paths: Vec<String> = manifest.entries.iter().filter(|e| e.is_file()).map(|e| e.p.clone()).collect();
    println!("Baseline (full archive of {} files) …", paths.len());
    let baseline = baseline_archive(&root, &paths);

    let cloud_bytes_total = lab.cloud_bytes();
    if !o.keep {
        let _ = fs::remove_dir_all(&parent);
    }
    Suite {
        backend: lab.backend.name().into(),
        region_block,
        upload_concurrency: conc,
        fixture: fx,
        scenarios: sc,
        cloud_bytes_total,
        baseline,
    }
}

// ---- chunking experiment ---------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct Variant {
    pub label: String,
    pub rule: String,
    pub initial_secs: f64,
    pub initial_payload_bytes: u64,
    pub blobs: u64,
    pub metadata_bytes_initial: u64,
    pub edits: Vec<VariantEdit>,
    pub restore_secs: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct VariantEdit {
    pub bytes_modified: u64,
    pub payload_bytes_uploaded: u64,
    pub metadata_bytes_uploaded: u64,
    pub secs: f64,
    pub amplification: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChunkingResult {
    pub region_files: usize,
    pub region_bytes: u64,
    pub edit_model: String,
    pub variants: Vec<Variant>,
}

fn rules_for(strategy: &str) -> String {
    format!(
        "version = 1\n[[rule]]\nid = \"bench-region\"\nmatch = [\"**/*.mca\"]\naction = \"include\"\nstrategy = \"{strategy}\"\ncategory = \"world\"\nreason = \"benchmark\"\n"
    )
}

/// Compare ways of chunking region files on the same data and the same edits.
pub fn chunking_experiment(work: &Path, big: bool) -> ChunkingResult {
    let spec = FixtureSpec {
        seed: 99,
        mods: 0,
        mod_size: (1, 1),
        configs: 0,
        worlds: if big { 3 } else { 2 },
        regions_per_world: if big { 24 } else { 8 },
        chunks_per_region: 900,
        chunk_sectors: (1, 6),
        junk: false,
        dot_minecraft: false,
    };
    let variants: Vec<(&str, String)> = vec![
        ("fixed 4 KiB", "fixed:4096".into()),
        ("fixed 8 KiB", "fixed:8192".into()),
        ("fixed 16 KiB", "fixed:16384".into()),
        ("fixed 32 KiB", "fixed:32768".into()),
        ("fixed 64 KiB", "fixed:65536".into()),
        ("cdc 4/16/64 KiB", "cdc:4096:16384:65536".into()),
        ("cdc 8/32/128 KiB", "cdc:8192:32768:131072".into()),
        ("whole file", "whole".into()),
    ];
    let mut out = ChunkingResult { region_files: 0, region_bytes: 0, edit_model: "per save: 3 region files × 8 chunks rewritten in place (same sector count, new payload), twice with different seeds".into(), variants: vec![] };
    for (label, strategy) in variants {
        println!("variant {label} …");
        let root = work.join("chunk-instance");
        let _ = fs::remove_dir_all(&root);
        let info = fixtures::generate(&root, &spec);
        out.region_files = info.regions.len();
        out.region_bytes = total_size(&info.regions);
        let lab = Lab::new(&work.join("chunk-lab"), Backend::Local);
        let a = lab.device("A", |_| {});
        let (en, m) = a.measure(|| {
            a.engine
                .enable_sync(
                    EnableRequest {
                        root: root.clone(),
                        launcher: "generic".into(),
                        name: Some("c".into()),
                        rules_overrides: rules_for(&strategy),
                        decisions: vec![],
                        launcher_ref: None,
                    },
                    &a.ctx(),
                )
                .unwrap()
        });
        let id = en.instance;
        let (_, mu) = a.measure(|| a.engine.upload_pending(id, true, &a.ctx()).unwrap());
        let mut v = Variant {
            label: label.into(),
            rule: strategy.clone(),
            initial_secs: m.secs + mu.secs,
            initial_payload_bytes: mu.storage.payload_bytes_up(),
            blobs: en.stats.new_blobs,
            metadata_bytes_initial: mu.storage.metadata_bytes_up(),
            edits: vec![],
            restore_secs: 0.0,
        };
        for seed in [7u64, 8] {
            let (touched, _) = edit_regions(&info.regions, 3, 8, seed);
            let ((_, _), me) = a.measure(|| snapshot_and_upload(&a, id));
            let payload = me.storage.payload_bytes_up();
            v.edits.push(VariantEdit {
                bytes_modified: touched,
                payload_bytes_uploaded: payload,
                metadata_bytes_uploaded: me.storage.metadata_bytes_up(),
                secs: me.secs,
                amplification: if touched > 0 { payload as f64 / touched as f64 } else { 0.0 },
            });
        }
        let b = lab.device("B", |_| {});
        let parent = work.join("chunk-restored");
        let _ = fs::remove_dir_all(&parent);
        fs::create_dir_all(&parent).unwrap();
        let (_, mr) = b.measure(|| {
            b.engine
                .restore_new(
                    RestoreNewRequest {
                        instance: id,
                        snapshot: None,
                        parent_dir: parent.clone(),
                        folder_name: "c".into(),
                        launcher: "generic".into(),
                        launcher_ref: None,
                        local: Default::default(),
                        skip_incompatible: false,
                    },
                    &b.ctx(),
                )
                .unwrap()
        });
        v.restore_secs = mr.secs;
        println!(
            "  initial {} up in {:.1}s, edit {} up, restore {:.1}s",
            human(v.initial_payload_bytes),
            v.initial_secs,
            human(v.edits[0].payload_bytes_uploaded),
            v.restore_secs
        );
        out.variants.push(v);
        let _ = fs::remove_dir_all(&parent);
    }
    out
}
