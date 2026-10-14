//! Start-To-The-Finish checkpoint conversion.
//!
//! Checkpoint blocks are replaced by their plain twins, preserving placement
//! flags and direction; checkpoint items and the validation ghost are removed.

use crate::blockmap::BlockInfoIndex;
use crate::sttc::{
    self, cell_str, has_chunk, owned_baked, pos_str, wp_str, Class, Ctx, WP_CHECKPOINT,
};
use crate::store::DataStore;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use tmmaps::map::{BlockRec, GhostForm, ItemRec, MapFile};

use super::sttc::Row;

// -------------------------------------------------------------------- sttf

/// Name candidates for the plain twin of a checkpoint block, most specific
/// first. Existence and geometry are checked by the caller.
pub fn twin_candidates(name: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |s: String| {
        if s != name && !out.contains(&s) {
            out.push(s);
        }
    };
    // the engine's own spellings: `CheckPoint` on the diagonal tech roads
    let norm = name.replace("CheckPoint", "Checkpoint");
    for (from, to) in [
        ("CheckpointTiltLeft", "TiltStraight"),
        ("CheckpointTiltRight", "TiltStraight"),
        ("CheckpointSlopeUp", "SlopeStraight"),
        ("CheckpointSlopeDown", "SlopeStraight"),
        ("CheckpointSlope2Up", "Slope2Base"),
        ("CheckpointSlope2Down", "Slope2Base"),
        ("CheckpointSlope2Left", "Slope2Base"),
        ("CheckpointSlope2Right", "Slope2Base"),
        ("CheckpointSlope2Up", "Slope2Straight"),
        ("CheckpointSlope2Down", "Slope2Straight"),
        ("CheckpointSlope2Left", "Slope2Straight"),
        ("CheckpointSlope2Right", "Slope2Straight"),
        ("CheckpointLeft", ""),
        ("CheckpointRight", ""),
        ("Checkpoint", "Straight"),
        ("Checkpoint", "StraightX2"),
        ("Checkpoint", "Base"),
        ("Checkpoint", ""),
    ] {
        if norm.contains(from) {
            push(norm.replacen(from, to, 1));
        }
    }
    out
}

/// The surface heights of a block model over its footprint, sampled on a
/// 2-m grid in the block's own frame: (x, z) → the sorted heights (and
/// physics materials) of every surface in that column.
pub(crate) struct Profile {
    pub(crate) w: f32,
    pub(crate) d: f32,
    pub(crate) cols: Vec<((f32, f32), Vec<(f32, String)>)>,
}

fn profile(asm: &mut crate::assemble::Assembler, name: &str) -> Option<Profile> {
    let lm = asm.block_model(name)?;
    let (w, d) = lm.size;
    let index = crate::probe::Index::build(&lm.scene, 8.0);
    let step = 2.0f32;
    let mut cols = Vec::new();
    let (nx, nz) = ((w / step).round() as i32, (d / step).round() as i32);
    for i in 0..nx {
        for j in 0..nz {
            let (x, z) = ((i as f32 + 0.5) * step, (j as f32 + 0.5) * step);
            let mut h = index.column(x, z);
            h.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
            cols.push(((x, z), h));
        }
    }
    Some(Profile { w, d, cols })
}

/// How well `twin` placed with `k` extra quarter turns reproduces `cp`:
/// (containment fraction, coverage fraction). Containment: of the columns
/// where the twin has a surface, the share whose every twin surface has a
/// checkpoint surface within 0.15 m of the same material class. Coverage: of
/// the columns where the checkpoint has a surface, the share where the twin
/// has one too.
pub(crate) fn match_score(cp: &Profile, twin: &Profile, k: u8) -> Option<(f32, f32)> {
    // the twin's footprint must be the checkpoint's, turned
    let (tw, td) = if k % 2 == 1 { (twin.d, twin.w) } else { (twin.w, twin.d) };
    if (tw - cp.w).abs() > 0.5 || (td - cp.d).abs() > 0.5 {
        return None;
    }
    // the twin column that lands on the checkpoint column (x, z) after the turn
    let lookup: HashMap<(i32, i32), &Vec<(f32, String)>> = twin.cols.iter().map(|((x, z), h)| (((x * 2.0) as i32, (z * 2.0) as i32), h)).collect();
    let (mut contained, mut twin_cols, mut covered, mut cp_cols) = (0usize, 0usize, 0usize, 0usize);
    for ((x, z), hcp) in &cp.cols {
        // world-local (lx, lz) = (x, z) of the checkpoint frame (dir 0); the
        // twin at dir k maps its local (xt, zt) to (lx, lz) by map-blocks.md §2
        let (xt, zt) = match k % 4 {
            0 => (*x, *z),
            1 => (*z, twin.d - *x),
            2 => (twin.w - *x, twin.d - *z),
            _ => (twin.w - *z, *x),
        };
        let key = ((xt * 2.0).round() as i32, (zt * 2.0).round() as i32);
        let ht: &[(f32, String)] = lookup.get(&key).map(|v| v.as_slice()).unwrap_or(&[]);
        if !hcp.is_empty() {
            cp_cols += 1;
            if !ht.is_empty() {
                covered += 1;
            }
        }
        if !ht.is_empty() {
            twin_cols += 1;
            let ok = ht.iter().all(|(h, m)| hcp.iter().any(|(h2, m2)| (h - h2).abs() <= 0.15 && m == m2));
            if ok {
                contained += 1;
            }
        }
    }
    if twin_cols == 0 || cp_cols == 0 {
        return None;
    }
    Some((contained as f32 / twin_cols as f32, covered as f32 / cp_cols as f32))
}

pub struct SttfOutcome {
    pub rows: Vec<Row>,
    pub replaced: usize,
    pub removed_blocks: usize,
    pub removed_items: usize,
    pub removed_baked: usize,
    pub mismatches: usize,
    pub unresolved: usize,
}

/// Start-To-The-Finish: write `out` from `src` with every checkpoint gone.
/// What a checkpoint BLOCK becomes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CpMode {
    /// swapped for its plain twin (geometry-verified), removed when it has none
    Plain,
    /// deleted, the cell left empty (the twin is still looked up and reported)
    Remove,
}

pub fn sttf(ctx: &mut Ctx, src: &Path, out: &Path, cp: CpMode, dry: bool) -> Result<SttfOutcome, String> {
    let map_label = src.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let mut m = MapFile::try_load(src)?;
    let mut rows = Vec::new();
    let (mut replaced, mut removed_blocks, mut removed_items, mut mismatches, mut unresolved) = (0, 0, 0, 0, 0);
    // (block index, twin, dir)
    let mut renames: Vec<(usize, String, u8)> = Vec::new();
    let mut drop_blocks: HashSet<usize> = HashSet::new();
    let mut drop_cells: HashSet<[i32; 3]> = HashSet::new();
    let mut drop_items: HashSet<usize> = HashSet::new();
    let cps: Vec<(usize, BlockRec)> = m.blocks.iter().enumerate().filter(|(_, b)| b.flags != 0xFFFF_FFFF).map(|(i, b)| (i, b.clone())).collect();
    let mut classified: Vec<(usize, BlockRec, Class)> = Vec::new();
    for (i, b) in cps {
        let c = ctx.block_class(&b);
        if !c.resolved && c.tag.is_some() {
            unresolved += 1;
        }
        if c.mismatch() {
            mismatches += 1;
            let mut r = Row::new(&map_label, "sttf", "block", i, &b.name);
            r.tag = b.waypoint_tag.clone().unwrap_or_default();
            r.model_wp = wp_str(c.model);
            r.action = "MISMATCH".into();
            r.from = cell_str(b.coords());
            r.note = "the record's tag and the block info disagree; the block info decides".into();
            rows.push(r);
        }
        classified.push((i, b, c));
    }
    let mut twin_cache: HashMap<(String, u8, u32), Result<(String, u8, String), String>> = HashMap::new();
    for (i, b, c) in &classified {
        if c.effective() != Some(WP_CHECKPOINT) {
            continue;
        }
        let mut r = Row::new(&map_label, "sttf", "block", *i, &b.name);
        r.tag = b.waypoint_tag.clone().unwrap_or_default();
        r.model_wp = wp_str(c.model);
        r.dir_from = b.dir.to_string();
        r.from = match b.free_pos {
            Some(p) => format!("free {}", pos_str(p)),
            None => cell_str(b.coords()),
        };
        let key = (b.name.clone(), b.dir, b.flags & 0x0FFF_FFFF);
        let twin = match twin_cache.get(&key) {
            Some(t) => t.clone(),
            None => {
                // the assembler and the index both read the packs: disjoint
                // field borrows of the context
                let t = {
                    let mut asm = crate::assemble::Assembler::new(&mut *ctx.store);
                    find_twin_split(&mut ctx.idx, &mut asm, b)
                };
                twin_cache.insert(key, t.clone());
                t
            }
        };
        match twin {
            Ok((name, dir, note)) if cp == CpMode::Remove => {
                r.action = "removed".into();
                r.to_name = name;
                r.dir_to = dir.to_string();
                r.note = format!("--cp remove (the plain twin would be {}): {note}", r.to_name);
                drop_blocks.insert(*i);
                if b.free_pos.is_none() {
                    for c in ctx.world_cells(b) {
                        drop_cells.insert(c);
                    }
                }
                removed_blocks += 1;
            }
            Ok((name, dir, note)) => {
                r.action = "replaced".into();
                r.to_name = name.clone();
                r.dir_to = dir.to_string();
                r.note = note;
                renames.push((*i, name, dir));
                replaced += 1;
            }
            Err(why) => {
                r.action = "removed".into();
                r.note = why;
                drop_blocks.insert(*i);
                if b.free_pos.is_none() {
                    for c in ctx.world_cells(b) {
                        drop_cells.insert(c);
                    }
                }
                removed_blocks += 1;
            }
        }
        rows.push(r);
    }
    for (i, it) in m.items.iter().enumerate() {
        let c = ctx.item_class(it);
        if c.mismatch() {
            mismatches += 1;
            let mut r = Row::new(&map_label, "sttf", "item", i, &it.model);
            r.tag = it.waypoint_tag.clone().unwrap_or_default();
            r.model_wp = wp_str(c.model);
            r.action = "MISMATCH".into();
            r.from = pos_str(it.pos);
            r.note = "the placement's tag and the item model disagree; the model decides".into();
            rows.push(r);
        }
        if !c.resolved && c.tag.is_some() {
            unresolved += 1;
        }
        if c.effective() == Some(WP_CHECKPOINT) {
            let mut r = Row::new(&map_label, "sttf", "item", i, &it.model);
            r.tag = it.waypoint_tag.clone().unwrap_or_default();
            r.model_wp = wp_str(c.model);
            r.action = "removed".into();
            r.from = pos_str(it.pos);
            r.note = if c.resolved { String::new() } else { "model not read: the tag decided".into() };
            rows.push(r);
            drop_items.insert(i);
            removed_items += 1;
        }
    }
    let owned = if drop_cells.is_empty() { Vec::new() } else { owned_baked(ctx, &m, &drop_cells) };
    let removed_baked = owned.len();
    for idx in &owned {
        if let Some(rec) = m.baked.iter().find(|r| r.index == *idx) {
            let mut r = Row::new(&map_label, "sttf", "baked", rec.index, &rec.name);
            r.action = "removed".into();
            r.dir_from = rec.dir.to_string();
            r.from = cell_str(rec.coords());
            r.note = "a clip owned by a removed checkpoint block".into();
            rows.push(r);
        }
    }
    if dry {
        return Ok(SttfOutcome { rows, replaced, removed_blocks, removed_items, removed_baked, mismatches, unresolved });
    }
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    // pass A: the twins (a rename re-encodes the Id table; dirs are in-place bytes)
    if !renames.is_empty() {
        for (i, name, dir) in &renames {
            m.set_block_name(*i, name);
            if *dir != m.blocks[*i].dir {
                m.set_block_dir(*i, *dir);
            }
        }
        m.write_to(out).map_err(|e| e.to_string())?;
        m = MapFile::try_load(out)?;
    } else {
        m.write_to(out).map_err(|e| e.to_string())?;
        m = MapFile::try_load(out)?;
    }
    // pass A2: the replaced blocks' waypoint nodes go. The game decides by the
    // model, but every tool that reads tags (`tmmaps waypoints`, the start
    // check, the ladder) would still see a checkpoint. The node is the record's
    // tail — `u32 node index` then, on its first use, the inline class + chunks
    // — so the tail is cut and flag bit 20 cleared; a record with a skin
    // (bit 15: author + skin node sit in between) or whose node index another
    // record shares keeps the residue and says so.
    let mut stripped: HashSet<usize> = HashSet::new();
    {
        let spans = m.block_spans();
        let body = m.gbx.body.clone();
        let rd = |o: usize| u32::from_le_bytes(body[o..o + 4].try_into().unwrap());
        let mut idx_count: HashMap<u32, usize> = HashMap::new();
        for b in m.blocks.iter().filter(|b| b.flags != 0xFFFF_FFFF && b.flags & 0x100000 != 0 && b.flags & 0x8000 == 0) {
            *idx_count.entry(rd(b.coord_off + 7)).or_default() += 1;
        }
        for (i, _, _) in &renames {
            let b = m.blocks[*i].clone();
            if b.flags & 0x100000 == 0 || b.flags & 0x8000 != 0 {
                continue;
            }
            let (start, end) = (b.coord_off + 7, spans[*i].1);
            let idx = rd(start);
            if idx != 0xFFFF_FFFF && idx_count.get(&idx).copied().unwrap_or(0) > 1 {
                continue;
            }
            if idx != 0xFFFF_FFFF && rd(start + 4) != 0x2E00_9000 && end != start + 4 {
                continue; // not the layout this expects: leave it
            }
            m.raw_splices.push(((start, end), Vec::new()));
            m.raw_patches.push((b.coord_off + 3, (b.flags & !0x100000).to_le_bytes().to_vec()));
            stripped.insert(*i);
        }
        if !stripped.is_empty() {
            m.write_to(out).map_err(|e| e.to_string())?;
            m = MapFile::try_load(out)?;
        }
    }
    for r in rows.iter_mut().filter(|r| r.action == "replaced") {
        let i: usize = r.index.parse().unwrap_or(usize::MAX);
        if stripped.contains(&i) {
            r.note = format!("waypoint node stripped; {}", r.note);
        } else {
            r.note = format!("WAYPOINT TAG LEFT (skinned or shared node; the game ignores it); {}", r.note);
        }
    }
    // pass B: the checkpoint blocks with no twin, and the clips they owned
    if !drop_blocks.is_empty() {
        let owned_set: HashSet<usize> = owned.iter().copied().collect();
        m.remove_blocks(|b| drop_blocks.contains(&b.index), |r| owned_set.contains(&r.index));
        m.write_to(out).map_err(|e| e.to_string())?;
        m = MapFile::try_load(out)?;
    }
    // pass C: the checkpoint items
    if !drop_items.is_empty() {
        m.remove_items(|it| drop_items.contains(&it.index));
        m.write_to(out).map_err(|e| e.to_string())?;
        m = MapFile::try_load(out)?;
    }
    // pass D: the validation ghost (it drove through checkpoints that no longer exist)
    m.strip_validation_ghost_to(GhostForm::Remove);
    m.write_to(out).map_err(|e| e.to_string())?;
    Ok(SttfOutcome { rows, replaced, removed_blocks, removed_items, removed_baked, mismatches, unresolved })
}

/// `find_twin` over the index and an assembler that share no borrow: the
/// assembler owns a `&mut DataStore`, the index only needs one for a cache
/// miss — the misses are served through the assembler's store.
fn find_twin_split(idx: &mut BlockInfoIndex, asm: &mut crate::assemble::Assembler, b: &BlockRec) -> Result<(String, u8, String), String> {
    let cands = twin_candidates(&b.name);
    let Some(cp) = profile(asm, &b.name) else {
        // Dedicated/server packs contain the block-info records but not
        // necessarily the prefab geometry. The checkpoint/plain naming pair
        // is still authoritative in that case: keep the placement's flags
        // (including air/ground mode) and rotation, and only change the model
        // name. This is preferable to deleting a checkpoint whose plain twin
        // is unambiguous.
        for cand in &cands {
            let Some(path) = idx.resolve_one(&mut *asm.store, cand) else {
                continue;
            };
            let ident = match idx.load(&mut *asm.store, &path) {
                Ok(bi) if !bi.ident.is_empty() => bi.ident.clone(),
                Ok(bi) => bi.name.clone(),
                Err(_) => cand.clone(),
            };
            let dir = fallback_twin_dir(b, cand);
            return Ok((
                ident,
                dir,
                format!("geometry unavailable; selected named plain twin {cand}, direction {dir}"),
            ));
        }
        if let Some(cand) = cands.first() {
            let dir = fallback_twin_dir(b, cand);
            return Ok((
                cand.clone(),
                dir,
                format!("geometry and pack metadata unavailable; selected named plain twin {cand}, direction {dir}"),
            ));
        }

        return Err(format!("no geometry for {} and no named plain twin", b.name));
    };
    let mut tried: Vec<String> = Vec::new();
    let mut best: Option<(f32, f32, String, u8)> = None;
    for cand in &cands {
        let Some(path) = idx.resolve_one(&mut *asm.store, cand) else {
            continue;
        };
        let ident = match idx.load(&mut *asm.store, &path) {
            Ok(bi) if !bi.ident.is_empty() => bi.ident.clone(),
            Ok(bi) => bi.name.clone(),
            Err(_) => cand.clone(),
        };
        let Some(tp) = profile(asm, cand) else {
            tried.push(format!("{cand}:no-geometry"));
            continue;
        };
        for k in 0..4u8 {
            if let Some((c, cov)) = match_score(&cp, &tp, k) {
                tried.push(format!("{cand}+{k}:{c:.2}/{cov:.2}"));
                let better = match &best {
                    None => true,
                    Some((bc, bcov, _, _)) => c + cov * 0.5 > bc + bcov * 0.5,
                };
                if better {
                    best = Some((c, cov, ident.clone(), (b.dir + k) % 4));
                }

            }
        }
        if let Some((c, cov, _, _)) = &best {
            if *c >= 0.95 && *cov >= 0.8 {
                break;
            }
        }
    }
    match best {
        Some((c, cov, ident, dir)) if c >= 0.95 && cov >= 0.8 => Ok((ident, dir, format!("contained {c:.2} covered {cov:.2}; tried {}", tried.join(" ")))),
        Some((c, cov, ident, _)) => Err(format!("best {ident} contained {c:.2} covered {cov:.2} < 0.95/0.80; tried {}", tried.join(" "))),
        None => Err(format!("no candidate exists among {:?}", cands)),
    }
}

fn fallback_twin_dir(block: &BlockRec, twin: &str) -> u8 {
    if block.name.contains("CheckpointTilt") && twin.contains("TiltStraight") {
        (block.dir + 2) % 4
    } else {
        block.dir
    }
}

pub fn verify_sttf(src: &Path, out: &Path, outcome: &SttfOutcome) -> Result<Vec<String>, String> {
    let a = MapFile::try_load(src)?;
    let b = MapFile::try_load(out)?;
    let mut bad = Vec::new();
    let removed_b: usize = outcome.rows.iter().filter(|r| r.kind == "block" && r.action == "removed").count();
    let removed_i: usize = outcome.rows.iter().filter(|r| r.kind == "item" && r.action == "removed").count();
    if a.blocks.len() - removed_b != b.blocks.len() {
        bad.push(format!("block count {} - {removed_b} != {}", a.blocks.len(), b.blocks.len()));
    }
    if a.items.len() - removed_i != b.items.len() {
        bad.push(format!("item count {} - {removed_i} != {}", a.items.len(), b.items.len()));
    }
    if a.baked.len() - outcome.removed_baked != b.baked.len() {
        bad.push(format!("baked count {} - {} != {}", a.baked.len(), outcome.removed_baked, b.baked.len()));
    }
    let cps = b.waypoints().iter().filter(|w| w.tag == "Checkpoint" || w.tag == "LinkedCheckpoint").count();
    // the tags ride on renamed blocks (the model decides, map-blocks.md §5):
    // every surviving checkpoint TAG must sit on a replaced block, never on an item
    let item_cps = b.items.iter().filter(|it| matches!(it.waypoint_tag.as_deref(), Some("Checkpoint") | Some("LinkedCheckpoint"))).count();
    if item_cps > 0 {
        bad.push(format!("{item_cps} checkpoint-tagged items remain"));
    }
    let replaced: HashSet<String> = outcome.rows.iter().filter(|r| r.action == "replaced").map(|r| r.to_name.clone()).collect();
    let residue_ok = outcome.rows.iter().filter(|r| r.action == "replaced" && r.note.starts_with("WAYPOINT TAG LEFT")).count();
    let mut residue = 0usize;
    for bl in b.blocks.iter().filter(|bl| matches!(bl.waypoint_tag.as_deref(), Some("Checkpoint") | Some("LinkedCheckpoint"))) {
        if !replaced.contains(&bl.name) {
            bad.push(format!("checkpoint-tagged block {} is not a replaced twin", bl.name));
        } else {
            residue += 1;
        }
    }
    if residue > residue_ok {
        bad.push(format!("{residue} checkpoint tags remain on replaced blocks, {residue_ok} expected"));
    }
    let _ = cps;
    // every replaced block carries the twin's name and the dir the geometry chose
    // (block indices are unchanged by a rename; removals come after, so map
    // the source index through the dropped set)
    {
        let dropped: Vec<usize> = {
            let mut v: Vec<usize> = outcome.rows.iter().filter(|r| r.kind == "block" && r.action == "removed").filter_map(|r| r.index.parse().ok()).collect();
            v.sort_unstable();
            v
        };
        for r in outcome.rows.iter().filter(|r| r.kind == "block" && r.action == "replaced") {
            let Ok(i) = r.index.parse::<usize>() else { continue };
            let shift = dropped.iter().filter(|d| **d < i).count();
            let want_dir: u8 = r.dir_to.parse().unwrap_or(255);
            match b.blocks.get(i - shift) {
                Some(bl) if bl.name == r.to_name && bl.dir == want_dir => {}
                Some(bl) => bad.push(format!("block#{i} should be {} dir {} and is {} dir {}", r.to_name, r.dir_to, bl.name, bl.dir)),
                None => bad.push(format!("block#{i} missing from the output")),
            }
        }
    }
    // nothing else moved: every kept block / item keeps its pose
    let kept_blocks: Vec<&BlockRec> = {
        let dropped: HashSet<usize> = outcome.rows.iter().filter(|r| r.kind == "block" && r.action == "removed").filter_map(|r| r.index.parse().ok()).collect();
        a.blocks.iter().filter(|x| !dropped.contains(&x.index)).collect()
    };
    for (x, y) in kept_blocks.iter().zip(b.blocks.iter()) {
        if x.coords() != y.coords() || x.free_pos != y.free_pos || (x.flags & !0x100000) != (y.flags & !0x100000) {
            bad.push(format!("block {} ({:?}) changed pose/flags", x.name, x.coords()));
            break;
        }
    }
    let kept_items: Vec<&ItemRec> = {
        let dropped: HashSet<usize> = outcome.rows.iter().filter(|r| r.kind == "item" && r.action == "removed").filter_map(|r| r.index.parse().ok()).collect();
        a.items.iter().filter(|x| !dropped.contains(&x.index)).collect()
    };
    for (x, y) in kept_items.iter().zip(b.items.iter()) {
        if x.pos != y.pos || x.model != y.model || x.yaw != y.yaw {
            bad.push(format!("item {} ({:?}) changed", x.model, x.pos));
            break;
        }
    }
    if has_chunk(&b.gbx.body, 0x0305_B00F) {
        bad.push("validation ghost chunk still present".into());
    }
    Ok(bad)
}
