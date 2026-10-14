//! Finish-removal map surgery for the flags campaign.
//!
//! Unlike `sttc`, this operation does not move or replace any waypoint. It
//! removes finish blocks and finish items, together with baked clip records
//! owned by removed blocks. Ordinary items are never touched, including items
//! placed on top of a finish block.

use crate::sttc::{self, Ctx, WP_FINISH, WP_STARTFINISH};
use crate::DataStore;
use std::collections::HashSet;
use std::path::Path;
use tmmaps::map::MapFile;

#[derive(Default)]
pub struct Outcome {
    pub rows: Vec<String>,
    pub finish_blocks: usize,
    pub finish_items: usize,
    pub removed_baked: usize,
}

fn is_finish(class: &sttc::Class) -> bool {
    matches!(class.effective(), Some(WP_FINISH) | Some(WP_STARTFINISH))
}

fn row(kind: &str, index: usize, name: &str, action: &str) -> String {
    format!("{kind}\t{index}\t{}\t{action}", name.replace('\t', " "))
}

/// Remove every finish from `src` and write the result to `out`.
pub fn remove_finishes(
    store: &mut DataStore,
    src: &Path,
    out: &Path,
    dry: bool,
) -> Result<Outcome, String> {
    let mut m = MapFile::try_load(src)?;
    let mut ctx = Ctx::new(store, &m);
    let mut outcome = Outcome::default();
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
    m.remove_blocks(
        |b| drop_blocks.contains(&b.index),
        |b| owned.contains(&b.index),
    );
    m.write_to(out).map_err(|e| e.to_string())?;

    let mut m = MapFile::try_load(out)?;
    m.remove_items(|it| drop_items.contains(&it.index));
    m.write_to(out).map_err(|e| e.to_string())?;
    Ok(outcome)
}

pub fn verify(store: &mut DataStore, out: &Path) -> Result<Vec<String>, String> {
    let m = MapFile::try_load(out)?;
    let mut ctx = Ctx::new(store, &m);
    let mut bad = Vec::new();
    for b in &m.blocks {
        if is_finish(&ctx.block_class(b)) {
            bad.push(format!("finish block remains: {}", b.name));
        }
    }
    for it in &m.items {
        if is_finish(&ctx.item_class(it)) {
            bad.push(format!("finish item remains: {}", it.model));
        }
    }
    Ok(bad)
}
