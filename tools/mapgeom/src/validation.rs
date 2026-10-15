use tmmaps::header::MapHeader;
use tmmaps::map::{GhostForm, MapFile};

/// Remove validation while preserving the map author and other challenge
/// metadata. The game represents an unvalidated map with a skeleton ghost and
/// all four times set to signed -1 milliseconds.
pub fn remove(m: &mut MapFile, header: &MapHeader) -> usize {
    let removed = m.strip_validation_ghost_to(GhostForm::Skeleton);
    m.set_times(header, u32::MAX, u32::MAX, u32::MAX, u32::MAX, false);
    removed
}
