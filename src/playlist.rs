//! The playlist: an ordered list of cuts, possibly from several source files,
//! that is rendered out in one go.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The shortest cut the app will create, in seconds.
pub const MIN_CUT_LEN: f64 = 0.01;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CutId(pub u64);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cut {
    pub id: CutId,
    pub source: PathBuf,
    pub start: f64,
    pub end: f64,
    #[serde(default)]
    pub label: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

impl Cut {
    pub fn duration(&self) -> f64 {
        (self.end - self.start).max(0.0)
    }

    pub fn contains(&self, t: f64) -> bool {
        t >= self.start && t < self.end
    }

    /// The label if set, otherwise a fallback like `Cut 3`.
    pub fn display_label(&self, index: usize) -> String {
        if self.label.trim().is_empty() { format!("Cut {}", index + 1) } else { self.label.trim().to_string() }
    }
}

/// Errors returned by playlist edits.
#[derive(Debug, Clone, PartialEq)]
pub enum EditError {
    UnknownCut,
    TooShort,
    OutOfRange,
}

impl std::fmt::Display for EditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EditError::UnknownCut => write!(f, "That cut no longer exists"),
            EditError::TooShort => write!(f, "A cut must end after it starts"),
            EditError::OutOfRange => write!(f, "The time is outside the media"),
        }
    }
}

impl std::error::Error for EditError {}

/// Normalizes a pair of times into a valid `(start, end)` range within
/// `[0, duration]`. Swaps the ends if needed.
pub fn normalize_range(a: f64, b: f64, duration: f64) -> Result<(f64, f64), EditError> {
    let (start, end) = if a <= b { (a, b) } else { (b, a) };
    let start = start.max(0.0);
    let end = if duration > 0.0 { end.min(duration) } else { end };
    if end - start < MIN_CUT_LEN {
        return Err(EditError::TooShort);
    }
    Ok((start, end))
}

/// The range that the in/out marks describe, given the playhead position.
///
/// With both marks set, that range. With only an in mark, it runs to the
/// playhead (or to the end when the playhead is before it). With only an out
/// mark, it runs from the playhead (or the start). `None` without marks or
/// when the range would be too short.
pub fn marked_range(mark_in: Option<f64>, mark_out: Option<f64>, position: f64, duration: f64) -> Option<(f64, f64)> {
    let range = match (mark_in, mark_out) {
        (Some(a), Some(b)) => (a, b),
        (Some(a), None) if position > a => (a, position),
        (Some(a), None) => (a, duration),
        (None, Some(b)) if position < b => (position, b),
        (None, Some(b)) => (0.0, b),
        (None, None) => return None,
    };
    normalize_range(range.0, range.1, duration).ok()
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Playlist {
    cuts: Vec<Cut>,
    next_id: u64,
}

impl Playlist {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cuts(&self) -> &[Cut] {
        &self.cuts
    }

    pub fn len(&self) -> usize {
        self.cuts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cuts.is_empty()
    }

    pub fn get(&self, id: CutId) -> Option<&Cut> {
        self.cuts.iter().find(|c| c.id == id)
    }

    pub fn index_of(&self, id: CutId) -> Option<usize> {
        self.cuts.iter().position(|c| c.id == id)
    }

    fn get_mut(&mut self, id: CutId) -> Result<&mut Cut, EditError> {
        self.cuts.iter_mut().find(|c| c.id == id).ok_or(EditError::UnknownCut)
    }

    fn alloc_id(&mut self) -> CutId {
        // Never reuse ids, even after a load that contains higher ones.
        let max_existing = self.cuts.iter().map(|c| c.id.0 + 1).max().unwrap_or(0);
        self.next_id = self.next_id.max(max_existing);
        let id = CutId(self.next_id);
        self.next_id += 1;
        id
    }

    /// Appends a new cut and returns its id.
    pub fn add(&mut self, source: impl Into<PathBuf>, start: f64, end: f64, label: impl Into<String>) -> CutId {
        let id = self.alloc_id();
        self.cuts.push(Cut { id, source: source.into(), start, end, label: label.into(), enabled: true });
        id
    }

    /// Inserts a new cut directly after `after` (or at the end).
    pub fn insert_after(
        &mut self,
        after: Option<CutId>,
        source: impl Into<PathBuf>,
        start: f64,
        end: f64,
        label: impl Into<String>,
    ) -> CutId {
        let id = self.add(source, start, end, label);
        if let Some(index) = after.and_then(|a| self.index_of(a)) {
            let cut = self.cuts.pop().expect("just pushed");
            self.cuts.insert(index + 1, cut);
        }
        id
    }

    pub fn remove(&mut self, id: CutId) -> Option<Cut> {
        let index = self.index_of(id)?;
        Some(self.cuts.remove(index))
    }

    pub fn clear(&mut self) {
        self.cuts.clear();
    }

    /// Moves a cut by `delta` positions, clamped to the list bounds.
    /// Returns true if the order changed.
    pub fn shift(&mut self, id: CutId, delta: isize) -> bool {
        let Some(from) = self.index_of(id) else {
            return false;
        };
        let to = (from as isize + delta).clamp(0, self.cuts.len() as isize - 1) as usize;
        self.move_to(from, to)
    }

    /// Moves the cut at `from` so that it ends up at index `to`.
    pub fn move_to(&mut self, from: usize, to: usize) -> bool {
        if from >= self.cuts.len() || to >= self.cuts.len() || from == to {
            return false;
        }
        let cut = self.cuts.remove(from);
        self.cuts.insert(to, cut);
        true
    }

    pub fn set_range(&mut self, id: CutId, start: f64, end: f64, duration: f64) -> Result<(), EditError> {
        if start < 0.0 || (duration > 0.0 && end > duration + 1e-6) {
            return Err(EditError::OutOfRange);
        }
        if end - start < MIN_CUT_LEN {
            return Err(EditError::TooShort);
        }
        let cut = self.get_mut(id)?;
        cut.start = start;
        cut.end = end;
        Ok(())
    }

    pub fn set_start(&mut self, id: CutId, start: f64, duration: f64) -> Result<(), EditError> {
        let end = self.get(id).ok_or(EditError::UnknownCut)?.end;
        self.set_range(id, start, end, duration)
    }

    pub fn set_end(&mut self, id: CutId, end: f64, duration: f64) -> Result<(), EditError> {
        let start = self.get(id).ok_or(EditError::UnknownCut)?.start;
        self.set_range(id, start, end, duration)
    }

    pub fn set_label(&mut self, id: CutId, label: impl Into<String>) -> Result<(), EditError> {
        self.get_mut(id)?.label = label.into();
        Ok(())
    }

    pub fn set_enabled(&mut self, id: CutId, enabled: bool) -> Result<(), EditError> {
        self.get_mut(id)?.enabled = enabled;
        Ok(())
    }

    /// Splits a cut in two at `at`. The second half is inserted right after
    /// the first and its id is returned.
    pub fn split(&mut self, id: CutId, at: f64) -> Result<CutId, EditError> {
        let cut = self.get(id).ok_or(EditError::UnknownCut)?.clone();
        if at - cut.start < MIN_CUT_LEN || cut.end - at < MIN_CUT_LEN {
            return Err(EditError::OutOfRange);
        }
        self.get_mut(id)?.end = at;
        let new_id = self.insert_after(Some(id), cut.source.clone(), at, cut.end, cut.label.clone());
        if let Ok(new_cut) = self.get_mut(new_id) {
            new_cut.enabled = cut.enabled;
        }
        Ok(new_id)
    }

    pub fn duplicate(&mut self, id: CutId) -> Result<CutId, EditError> {
        let cut = self.get(id).ok_or(EditError::UnknownCut)?.clone();
        let new_id = self.insert_after(Some(id), cut.source, cut.start, cut.end, cut.label);
        Ok(new_id)
    }

    /// Sorts the cuts of each source by start time while keeping sources in
    /// the order they first appear.
    pub fn sort_by_time(&mut self) {
        let mut order: Vec<PathBuf> = Vec::new();
        for cut in &self.cuts {
            if !order.contains(&cut.source) {
                order.push(cut.source.clone());
            }
        }
        self.cuts.sort_by(|a, b| {
            let ia = order.iter().position(|p| *p == a.source);
            let ib = order.iter().position(|p| *p == b.source);
            ia.cmp(&ib).then(a.start.total_cmp(&b.start)).then(a.end.total_cmp(&b.end))
        });
    }

    /// Cuts that are included in the render, in playlist order.
    pub fn enabled(&self) -> impl Iterator<Item = &Cut> {
        self.cuts.iter().filter(|c| c.enabled)
    }

    pub fn total_duration(&self) -> f64 {
        self.enabled().map(Cut::duration).sum()
    }

    pub fn for_source<'a>(&'a self, source: &'a Path) -> impl Iterator<Item = (usize, &'a Cut)> + 'a {
        self.cuts.iter().enumerate().filter(move |(_, c)| c.source == source)
    }

    /// The topmost cut of `source` containing `t`, preferring the last in
    /// playlist order (the one drawn on top).
    pub fn cut_at(&self, source: &Path, t: f64) -> Option<&Cut> {
        self.cuts.iter().rev().find(|c| c.source == source && c.contains(t))
    }

    /// Distinct sources referenced by the playlist, in order of appearance.
    pub fn sources(&self) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = Vec::new();
        for cut in &self.cuts {
            if !out.contains(&cut.source) {
                out.push(cut.source.clone());
            }
        }
        out
    }

    /// Creates cuts covering the gaps between the existing cuts of `source`
    /// (i.e. "keep everything except these"). Existing cuts of that source are
    /// replaced. Returns the number of cuts created.
    pub fn invert_for_source(&mut self, source: &Path, duration: f64) -> usize {
        let mut ranges: Vec<(f64, f64)> = self.for_source(source).map(|(_, c)| (c.start, c.end)).collect();
        if ranges.is_empty() || duration <= 0.0 {
            return 0;
        }
        ranges.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut gaps = Vec::new();
        let mut cursor = 0.0;
        for (start, end) in ranges {
            if start - cursor >= MIN_CUT_LEN {
                gaps.push((cursor, start));
            }
            cursor = f64::max(cursor, end);
        }
        if duration - cursor >= MIN_CUT_LEN {
            gaps.push((cursor, duration));
        }
        let insert_at = self.cuts.iter().position(|c| c.source == source).unwrap_or(self.cuts.len());
        self.cuts.retain(|c| c.source != source);
        let count = gaps.len();
        for (offset, (start, end)) in gaps.into_iter().enumerate() {
            let id = self.alloc_id();
            self.cuts.insert(
                insert_at + offset,
                Cut { id, source: source.to_path_buf(), start, end, label: String::new(), enabled: true },
            );
        }
        count
    }
}

/// A bounded undo/redo history of playlist snapshots.
#[derive(Debug, Default)]
pub struct History {
    undo: Vec<Playlist>,
    redo: Vec<Playlist>,
}

const HISTORY_LIMIT: usize = 200;

impl History {
    /// Records `before` as an undo point. Call before mutating.
    pub fn record(&mut self, before: &Playlist) {
        if self.undo.last() == Some(before) {
            return;
        }
        self.undo.push(before.clone());
        if self.undo.len() > HISTORY_LIMIT {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Returns the playlist to restore, given the current one.
    pub fn undo(&mut self, current: &Playlist) -> Option<Playlist> {
        let previous = self.undo.pop()?;
        self.redo.push(current.clone());
        Some(previous)
    }

    pub fn redo(&mut self, current: &Playlist) -> Option<Playlist> {
        let next = self.redo.pop()?;
        self.undo.push(current.clone());
        Some(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(name: &str) -> PathBuf {
        PathBuf::from(format!("/media/{name}.mp4"))
    }

    fn sample() -> (Playlist, CutId, CutId, CutId) {
        let mut p = Playlist::new();
        let a = p.add(src("a"), 0.0, 10.0, "intro");
        let b = p.add(src("a"), 20.0, 30.0, "");
        let c = p.add(src("b"), 5.0, 6.0, "other");
        (p, a, b, c)
    }

    #[test]
    fn ids_are_unique_and_stable() {
        let (mut p, a, b, c) = sample();
        assert_ne!(a, b);
        assert_ne!(b, c);
        p.remove(c);
        let d = p.add(src("a"), 1.0, 2.0, "");
        assert_ne!(d, c, "ids must not be reused after removal");
    }

    #[test]
    fn ids_survive_serialization() {
        let (p, ..) = sample();
        let json = serde_json::to_string(&p).unwrap();
        let mut loaded: Playlist = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded, p);
        let id = loaded.add(src("a"), 1.0, 2.0, "");
        assert!(loaded.cuts().iter().filter(|c| c.id == id).count() == 1);
    }

    #[test]
    fn normalizes_ranges() {
        assert_eq!(normalize_range(5.0, 2.0, 10.0), Ok((2.0, 5.0)));
        assert_eq!(normalize_range(-1.0, 20.0, 10.0), Ok((0.0, 10.0)));
        assert_eq!(normalize_range(3.0, 3.0, 10.0), Err(EditError::TooShort));
    }

    #[test]
    fn marked_ranges() {
        assert_eq!(marked_range(Some(2.0), Some(5.0), 9.0, 10.0), Some((2.0, 5.0)));
        assert_eq!(marked_range(Some(2.0), None, 4.0, 10.0), Some((2.0, 4.0)));
        assert_eq!(marked_range(Some(2.0), None, 1.0, 10.0), Some((2.0, 10.0)));
        assert_eq!(marked_range(None, Some(5.0), 1.0, 10.0), Some((1.0, 5.0)));
        assert_eq!(marked_range(None, Some(5.0), 7.0, 10.0), Some((0.0, 5.0)));
        assert_eq!(marked_range(None, None, 1.0, 10.0), None);
        assert_eq!(marked_range(Some(3.0), Some(3.0), 1.0, 10.0), None);
    }

    #[test]
    fn reorders() {
        let (mut p, a, b, c) = sample();
        assert!(p.shift(a, 1));
        assert_eq!(p.cuts().iter().map(|c| c.id).collect::<Vec<_>>(), vec![b, a, c]);
        assert!(p.shift(a, 10));
        assert_eq!(p.index_of(a), Some(2));
        assert!(!p.shift(a, 1), "already last");
        assert!(p.move_to(2, 0));
        assert_eq!(p.index_of(a), Some(0));
    }

    #[test]
    fn edits_ranges_with_validation() {
        let (mut p, a, ..) = sample();
        assert_eq!(p.set_start(a, 11.0, 100.0), Err(EditError::TooShort));
        assert_eq!(p.set_end(a, 200.0, 100.0), Err(EditError::OutOfRange));
        assert_eq!(p.set_start(a, -1.0, 100.0), Err(EditError::OutOfRange));
        p.set_start(a, 2.0, 100.0).unwrap();
        p.set_end(a, 8.0, 100.0).unwrap();
        assert_eq!(p.get(a).unwrap().duration(), 6.0);
        assert_eq!(p.set_label(CutId(999), "x"), Err(EditError::UnknownCut));
    }

    #[test]
    fn splits_in_place() {
        let (mut p, a, b, _) = sample();
        let new = p.split(a, 4.0).unwrap();
        assert_eq!(p.index_of(new), Some(1));
        assert_eq!(p.index_of(b), Some(2));
        assert_eq!(p.get(a).unwrap().end, 4.0);
        let n = p.get(new).unwrap();
        assert_eq!((n.start, n.end, n.label.as_str()), (4.0, 10.0, "intro"));
        assert_eq!(p.split(a, 0.0), Err(EditError::OutOfRange));
    }

    #[test]
    fn duplicates_after_original() {
        let (mut p, a, ..) = sample();
        let d = p.duplicate(a).unwrap();
        assert_eq!(p.index_of(d), Some(1));
        assert_eq!(p.get(d).unwrap().start, 0.0);
    }

    #[test]
    fn totals_only_enabled() {
        let (mut p, _, b, _) = sample();
        assert_eq!(p.total_duration(), 21.0);
        p.set_enabled(b, false).unwrap();
        assert_eq!(p.total_duration(), 11.0);
    }

    #[test]
    fn finds_cut_at_time() {
        let (p, a, b, _) = sample();
        assert_eq!(p.cut_at(&src("a"), 5.0).map(|c| c.id), Some(a));
        assert_eq!(p.cut_at(&src("a"), 25.0).map(|c| c.id), Some(b));
        assert_eq!(p.cut_at(&src("a"), 15.0), None);
        assert_eq!(p.cut_at(&src("b"), 25.0), None);
    }

    #[test]
    fn sorts_within_sources() {
        let mut p = Playlist::new();
        let x = p.add(src("b"), 10.0, 11.0, "");
        let y = p.add(src("a"), 9.0, 10.0, "");
        let z = p.add(src("b"), 1.0, 2.0, "");
        p.sort_by_time();
        assert_eq!(p.cuts().iter().map(|c| c.id).collect::<Vec<_>>(), vec![z, x, y]);
        assert_eq!(p.sources(), vec![src("b"), src("a")]);
    }

    #[test]
    fn inverts_cuts() {
        let (mut p, _, _, c) = sample();
        let created = p.invert_for_source(&src("a"), 40.0);
        assert_eq!(created, 2);
        let ranges: Vec<_> = p.for_source(&src("a")).map(|(_, c)| (c.start, c.end)).collect();
        assert_eq!(ranges, vec![(10.0, 20.0), (30.0, 40.0)]);
        assert_eq!(p.index_of(c), Some(2), "other sources keep their place");
    }

    #[test]
    fn insert_after_places_correctly() {
        let (mut p, a, b, _) = sample();
        let n = p.insert_after(Some(a), src("a"), 1.0, 2.0, "");
        assert_eq!(p.index_of(n), Some(1));
        assert_eq!(p.index_of(b), Some(2));
        let m = p.insert_after(None, src("a"), 1.0, 2.0, "");
        assert_eq!(p.index_of(m), Some(p.len() - 1));
    }

    #[test]
    fn undo_redo_roundtrip() {
        let (mut p, a, ..) = sample();
        let mut h = History::default();
        assert!(!h.can_undo());

        h.record(&p);
        p.remove(a);
        let after_remove = p.clone();

        let restored = h.undo(&p).unwrap();
        assert!(restored.get(a).is_some());
        p = restored;
        assert!(h.can_redo());

        let redone = h.redo(&p).unwrap();
        assert_eq!(redone, after_remove);
        p = redone;

        // A new edit clears the redo stack.
        if let Some(r) = h.undo(&p) {
            p = r;
        }
        h.record(&p);
        p.clear();
        assert!(!h.can_redo());
    }

    #[test]
    fn history_skips_duplicate_snapshots() {
        let (p, ..) = sample();
        let mut h = History::default();
        h.record(&p);
        h.record(&p);
        assert!(h.undo(&p).is_some());
        assert!(h.undo(&p).is_none());
    }
}
