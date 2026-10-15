//! Finish-removal map surgery for the flags campaign.
//!
//! Unlike `sttc`, this operation does not move or replace any waypoint. It
//! removes finish blocks and finish items, together with baked clip records
//! owned by removed blocks. Ordinary items are never touched, including items
//! placed on top of a finish block.

use crate::sttc::{self, Ctx, WP_FINISH, WP_STARTFINISH};
use crate::DataStore;
use std::collections::{BTreeSet, HashSet};
use std::path::Path;
use tmmaps::map::MapFile;

const TRIGGER_8M_MODEL: &str = "ClothFinishTriggerFlag8m.Item.Gbx";
const TRIGGER_8M_ASSET: &[u8] = include_bytes!("../assets/ClothFinishTriggerFlag8m.Item.Gbx");
const TRIGGER_16M_MODEL: &str = "ClothFinishTriggerFlag16m.Item.Gbx";
const TRIGGER_16M_ASSET: &[u8] = include_bytes!("../assets/ClothFinishTriggerFlag16m.Item.Gbx");
const TRIGGER_8M_COLLECTION: u32 = 29;
const TRIGGER_16M_COLLECTION: u32 = 29;

#[derive(Default)]
pub struct Outcome {
    pub rows: Vec<String>,
    pub finish_blocks: usize,
    pub finish_items: usize,
    pub removed_baked: usize,
    pub added_triggers: usize,
    pub map_name: Option<String>,
}

fn is_finish(class: &sttc::Class) -> bool {
    matches!(class.effective(), Some(WP_FINISH) | Some(WP_STARTFINISH))
}

fn row(kind: &str, index: usize, name: &str, action: &str) -> String {
    format!("{kind}\t{index}\t{}\t{action}", name.replace('\t', " "))
}

fn is_visual_flag(model: &str) -> bool {
    matches!(model, "Flag8m" | "Flag16m")
}

fn trigger_model(flag_model: &str) -> &'static str {
    if flag_model == "Flag8m" {
        TRIGGER_8M_MODEL
    } else {
        TRIGGER_16M_MODEL
    }
}

fn trigger_collection(model: &str) -> u32 {
    if model == TRIGGER_8M_MODEL {
        TRIGGER_8M_COLLECTION
    } else {
        TRIGGER_16M_COLLECTION
    }
}

fn same_position(a: [f32; 3], b: [f32; 3]) -> bool {
    a.iter().zip(b).all(|(x, y)| (*x - y).abs() <= 0.001)
}

fn flag_cell(pos: [f32; 3]) -> [u8; 3] {
    let cell = [
        (pos[0] / 32.0).floor() as i32,
        (pos[1] / 8.0).floor() as i32 + 15,
        (pos[2] / 32.0).floor() as i32,
    ];
    [
        cell[0].clamp(0, u8::MAX as i32) as u8,
        cell[1].clamp(0, u8::MAX as i32) as u8,
        cell[2].clamp(0, u8::MAX as i32) as u8,
    ]
}

/// Remove every finish from `src` and write the result to `out`.
pub fn remove_finishes(
    store: &mut DataStore,
    src: &Path,
    out: &Path,
    dry: bool,
) -> Result<Outcome, String> {
    let mut working_src = src.to_path_buf();
    let source_header = tmmaps::header::read(
        src.to_str()
            .ok_or_else(|| format!("map path is not valid UTF-8: {}", src.display()))?,
    )?;
    let source_name = source_header.name.clone();
    if source_name == "-" || source_name.is_empty() {
        return Err(format!(
            "{} does not declare an internal map name",
            src.display()
        ));
    }
    let new_name = format!("{source_name} Flags");
    if !dry {
        let sttf_path = std::env::temp_dir().join(format!(
            "trackmania-tas-flags-{}-{}.Map.Gbx",
            std::process::id(),
            source_name
                .chars()
                .map(|c| if matches!(c, '\\' | '/' | ':' | ' ') { '_' } else { c })
                .collect::<String>()
        ));
        let sttf_map = MapFile::try_load(src)?;
        let mut sttf_ctx = Ctx::new(store, &sttf_map);
        crate::sttf::sttf(
            &mut sttf_ctx,
            src,
            &sttf_path,
            crate::sttf::CpMode::Plain,
            false,
        )?;
        working_src = sttf_path;
    }
    let mut m = MapFile::try_load(&working_src)?;
    let working_header = tmmaps::header::read(
        working_src
            .to_str()
            .ok_or_else(|| format!("map path is not valid UTF-8: {}", working_src.display()))?,
    )?;
    if working_src != src {
        std::fs::remove_file(&working_src).map_err(|e| e.to_string())?;
    }
    let mut ctx = Ctx::new(store, &m);
    let mut outcome = Outcome::default();
    outcome.map_name = Some(new_name.clone());
    let mut drop_blocks = HashSet::new();
    let mut drop_cells = HashSet::new();

    for b in m.blocks.clone() {
        let class = ctx.block_class(&b);
        if !is_finish(&class) {
            continue;
        }
        outcome.finish_blocks += 1;
        outcome.rows.push(row("block", b.index, &b.name, "removed"));
        drop_blocks.insert(b.index);
        if b.free_pos.is_none() {
            drop_cells.extend(ctx.world_cells(&b));
        }
    }

    let mut drop_items = HashSet::new();
    for it in m.items.clone() {
        if matches!(it.model.as_str(), TRIGGER_8M_MODEL | TRIGGER_16M_MODEL) {
            continue;
        }
        let class = ctx.item_class(&it);
        if !is_finish(&class) {
            continue;
        }
        outcome.finish_items += 1;
        outcome
            .rows
            .push(row("item", it.index, &it.model, "removed"));
        drop_items.insert(it.index);
    }

    let owned = sttc::owned_baked(&mut ctx, &m, &drop_cells);
    outcome.removed_baked = owned.len();
    for index in &owned {
        if let Some(b) = m.baked.iter().find(|b| b.index == *index) {
            outcome.rows.push(row("baked", b.index, &b.name, "removed"));
        }
    }

    if dry {
        return Ok(outcome);
    }
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    crate::validation::remove(&mut m, &working_header);
    m.edit_header_xml(&|xml| {
        if xml.contains("validated=\"1\"") {
            Some(xml.replace("validated=\"1\"", "validated=\"0\""))
        } else {
            None
        }
    });
    m.set_map_name(&source_name, &new_name);
    m.write_to(out).map_err(|e| e.to_string())?;
    m = MapFile::try_load(out)?;

    m.remove_blocks(
        |b| drop_blocks.contains(&b.index),
        |b| owned.contains(&b.index),
    );
    m.write_to(out).map_err(|e| e.to_string())?;

    let mut m = MapFile::try_load(out)?;
    m.remove_items(|it| drop_items.contains(&it.index));
    m.write_to(out).map_err(|e| e.to_string())?;
    m = MapFile::try_load(out)?;

    let flags: Vec<([f32; 3], &'static str, [u8; 3])> = m
        .items
        .iter()
        .filter(|it| is_visual_flag(&it.model))
        .map(|it| {
            (
                it.pos,
                trigger_model(&it.model),
                flag_cell(it.pos),
            )
        })
        .collect();
    let existing_triggers: Vec<[f32; 3]> = m
        .items
        .iter()
        .filter(|it| matches!(it.model.as_str(), TRIGGER_8M_MODEL | TRIGGER_16M_MODEL))
        .map(|it| it.pos)
        .collect();
    let trigger_positions: Vec<([f32; 3], &'static str, [u8; 3])> = flags
        .into_iter()
        .filter(|(pos, _, _)| !existing_triggers.iter().any(|other| same_position(*pos, *other)))
        .collect();
    let first_trigger = m.items.len();
    if !trigger_positions.is_empty() {
        m.append_item_clones(first_trigger + trigger_positions.len());
        m.write_to(out).map_err(|e| e.to_string())?;
        m = MapFile::try_load(out)?;
        if m.items.len() < first_trigger + trigger_positions.len() {
            return Err(format!(
                "appended item records did not persist: expected at least {}, found {}",
                first_trigger + trigger_positions.len(),
                m.items.len()
            ));
        }

        for (offset, (pos, model, cell)) in trigger_positions.iter().enumerate() {
            let index = first_trigger + offset;
            m.set_item_model(index, model);
            m.set_item_author(index, model);
            m.set_item_collection(index, trigger_collection(model));
            m.move_item(index, *pos, 0.0, (cell[0] as i32, cell[1] as i32, cell[2] as i32));
            m.set_item_frame(index, [0.0, 0.0, 0.0], [0.0, 0.0, 0.0]);
            m.set_item_scale(index, 1.0);
            m.set_item_flags(index, 0);
        }
        m.write_to(out).map_err(|e| e.to_string())?;
        m = MapFile::try_load(out)?;

        for offset in 0..trigger_positions.len() {
            let index = first_trigger + offset;
            m.set_item_collection(index, trigger_collection(trigger_positions[offset].1));
            m.set_item_waypoint_tag(index, Some("Goal"));
            outcome.rows.push(row(
                "item",
                index,
                trigger_positions[offset].1,
                "added",
            ));
        }
        m.write_to(out).map_err(|e| e.to_string())?;

        let mut manifest = BTreeSet::new();
        for item in &m.items {
            if let Some(author) = item.author.as_deref() {
                if author != "Nadeo" {
                    manifest.insert((
                        item.model.clone(),
                        item.collection_raw,
                        author.to_string(),
                    ));
                }
            }
        }
        for model in [TRIGGER_8M_MODEL, TRIGGER_16M_MODEL] {
            manifest.insert((
                model.to_string(),
                trigger_collection(model),
                model.to_string(),
            ));
        }
        let manifest_refs: Vec<(&str, u32, &str)> = manifest
            .iter()
            .map(|(model, collection, author)| (model.as_str(), *collection, author.as_str()))
            .collect();
        let (zip, has_archive) =
            tmmaps::header::embedded_zip_bytes(&m.gbx.body)
                .map(|(zip, names)| {
                    let mut zip = zip;
                    for (model, asset) in [
                        (TRIGGER_8M_MODEL, TRIGGER_8M_ASSET),
                        (TRIGGER_16M_MODEL, TRIGGER_16M_ASSET),
                    ] {
                        if !names.iter().any(|name| name.replace('\\', "/") == format!("Items/{model}")) {
                            zip = tmmaps::header::zip_add(&zip, &format!("Items/{model}"), asset);
                        }
                    }
                    (zip, true)
                })
                .unwrap_or_else(|| {
                    let mut zip = Vec::new();
                    for (model, asset) in [
                        (TRIGGER_8M_MODEL, TRIGGER_8M_ASSET),
                        (TRIGGER_16M_MODEL, TRIGGER_16M_ASSET),
                    ] {
                        zip = tmmaps::header::zip_add(&zip, &format!("Items/{model}"), asset);
                    }
                    (zip, false)
                });
        if has_archive {
            m.replace_embedded_objects_with_collections(&manifest_refs, &zip);
        } else {
            m.insert_embedded_objects_with_collections(&manifest_refs, &zip);
        }
        m.write_to(out).map_err(|e| e.to_string())?;
    }
    outcome.added_triggers = trigger_positions.len();
    Ok(outcome)
}

pub fn verify(
    store: &mut DataStore,
    out: &Path,
    expected_name: Option<&str>,
) -> Result<Vec<String>, String> {
    let m = MapFile::try_load(out)?;
    let mut ctx = Ctx::new(store, &m);
    let mut bad = Vec::new();
    if let Some(expected) = expected_name {
        let actual = tmmaps::header::read(
            out.to_str()
                .ok_or_else(|| format!("map path is not valid UTF-8: {}", out.display()))?,
        )?
        .name;
        if actual != expected {
            bad.push(format!("map name is {actual:?}, expected {expected:?}"));
        }
    }
    for it in &m.items {
        if matches!(it.model.as_str(), TRIGGER_8M_MODEL | TRIGGER_16M_MODEL) {
            if it.waypoint_tag.as_deref() != Some("Goal") {
                bad.push(format!("flag trigger is not tagged Goal: {}", it.index));
            }
            if it.collection_raw != trigger_collection(&it.model) {
                bad.push(format!(
                    "flag trigger has wrong collection: {} ({})",
                    it.index, it.collection_raw
                ));
            }
            if ctx.item_class(it).model != Some(1) {
                bad.push(format!(
                    "flag trigger is not a finish waypoint: {} ({})",
                    it.index, it.model
                ));
            }
        }
    }

    let flags: Vec<_> = m.items.iter().filter(|it| is_visual_flag(&it.model)).collect();
    let triggers: Vec<_> = m
        .items
        .iter()
        .filter(|it| matches!(it.model.as_str(), TRIGGER_8M_MODEL | TRIGGER_16M_MODEL))
        .collect();
    if triggers.len() != flags.len() {
        bad.push(format!(
            "expected one flag trigger per visual flag: {} flags, {} triggers",
            flags.len(),
            triggers.len()
        ));
    }
    for flag in flags {
        let matches = triggers
            .iter()
            .filter(|trigger| same_position(flag.pos, trigger.pos))
            .count();
        if matches != 1 {
            bad.push(format!(
                "visual flag at ({:.3}, {:.3}, {:.3}) has {} matching triggers",
                flag.pos[0], flag.pos[1], flag.pos[2], matches
            ));
        }
    }
    let embedded = tmmaps::header::embedded_zip_bytes(&m.gbx.body)
        .ok_or_else(|| format!("{} has no embedded-objects archive", out.display()))?;
    if ![
    "Items/ClothFinishTriggerFlag8m.Item.Gbx",
    "Items/ClothFinishTriggerFlag16m.Item.Gbx",
    ]
    .iter()
    .all(|required| embedded.1.iter().any(|name| name.replace('\\', "/") == *required))
    {
        bad.push("embedded flag trigger assets are missing".to_string());
    }
    Ok(bad)
}
