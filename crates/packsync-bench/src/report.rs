//! The benchmark report, as Markdown.

use serde_json::Value;

use crate::hw::Hardware;
use crate::scenarios::{ChunkingResult, Suite, human};

pub fn suite_markdown(hw: &Hardware, s: &Suite) -> String {
    let mut o = String::new();
    o.push_str(&format!("### {} fixture, {} backend\n\n", s.fixture.size, s.backend));
    o.push_str(&format!(
        "Fixture: {} files on disk, {} synced files ({}), {} region files ({}), {} mod jars ({}); region block {} KiB, upload concurrency {}.\n\n",
        s.fixture.files, s.fixture.synced_files, human(s.fixture.synced_bytes), s.fixture.region_files, human(s.fixture.region_bytes), s.fixture.mod_files, human(s.fixture.mod_bytes), s.region_block / 1024, s.upload_concurrency
    ));
    o.push_str("| scenario | time | sent | received | logical requests | Drive HTTP requests | peak RSS |\n|---|---:|---:|---:|---:|---:|---:|\n");
    for sc in &s.scenarios {
        o.push_str(&format!(
            "| {} | {:.2} s | {} | {} | {} | {} | {:.0} MB |\n",
            sc.name,
            sc.metrics.secs,
            human(sc.metrics.storage.bytes_up),
            human(sc.metrics.storage.bytes_down),
            sc.metrics.storage.requests,
            sc.metrics.http.as_ref().map(|h| h.total().to_string()).unwrap_or_else(|| "–".into()),
            sc.metrics.peak_rss_mb
        ));
    }
    let b = &s.baseline;
    o.push_str(&format!(
        "\nBaseline — {}: {} compressed from {} in {:.1} s. Cloud storage used by PackSync after all scenarios: {} (for {} of synced data).\n\n",
        b["what"].as_str().unwrap_or(""),
        human(b["archive_bytes"].as_u64().unwrap_or(0)),
        human(b["raw_bytes"].as_u64().unwrap_or(0)),
        b["secs"].as_f64().unwrap_or(0.0),
        human(s.cloud_bytes_total),
        human(s.fixture.synced_bytes)
    ));
    let _ = hw;
    o
}

pub fn chunking_markdown(c: &ChunkingResult) -> String {
    let mut o = String::new();
    o.push_str(&format!(
        "Region data: {} files, {}. Edit model: {}.\n\n",
        c.region_files,
        human(c.region_bytes),
        c.edit_model
    ));
    o.push_str("| chunking | initial upload | blobs | initial metadata | edit 1: modified → uploaded (×) | edit 2: uploaded | edit metadata | edit time | restore time |\n|---|---:|---:|---:|---:|---:|---:|---:|---:|\n");
    for v in &c.variants {
        let e1 = &v.edits[0];
        let e2 = &v.edits[1];
        o.push_str(&format!(
            "| {} | {} | {} | {} | {} → {} ({:.1}×) | {} | {} | {:.2} s | {:.2} s |\n",
            v.label,
            human(v.initial_payload_bytes),
            v.blobs,
            human(v.metadata_bytes_initial),
            human(e1.bytes_modified),
            human(e1.payload_bytes_uploaded),
            e1.amplification,
            human(e2.payload_bytes_uploaded),
            human(e1.metadata_bytes_uploaded),
            e1.secs,
            v.restore_secs
        ));
    }
    o
}

pub fn hardware_markdown(hw: &Hardware) -> String {
    format!(
        "| | |\n|---|---|\n| CPU | {} ({} logical cores) |\n| RAM | {} GB |\n| OS | {} (kernel {}) |\n| Disk | {} on {} ({}) |\n| Work directory | `{}` |\n| Toolchain | {} |\n| Build profile | {} |\n",
        hw.cpu,
        hw.logical_cores,
        hw.ram_gb,
        hw.os,
        hw.kernel,
        hw.filesystem,
        hw.device,
        match hw.rotational {
            Some(true) => "rotational disk",
            Some(false) => "solid-state",
            None => "type unknown",
        },
        hw.work_dir,
        hw.rustc,
        hw.profile
    )
}

#[allow(dead_code)]
pub fn json_pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_default()
}
