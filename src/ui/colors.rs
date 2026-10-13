//! Colors used to tell cuts apart. The same color marks a cut in the
//! timeline and in the playlist.

use gpui_kit::{Hsla, hsla};

use crate::playlist::CutId;

const HUES: [f32; 8] = [0.58, 0.08, 0.36, 0.83, 0.13, 0.48, 0.97, 0.70];

/// The color of a cut. It follows the cut, so reordering the playlist
/// doesn't repaint everything.
pub fn cut_color(id: CutId, dark: bool) -> Hsla {
    let hue = HUES[(id.0 % HUES.len() as u64) as usize];
    if dark { hsla(hue, 0.70, 0.60, 1.0) } else { hsla(hue, 0.65, 0.45, 1.0) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors_cycle() {
        assert_eq!(cut_color(CutId(0), true), cut_color(CutId(8), true));
        assert_ne!(cut_color(CutId(0), true), cut_color(CutId(1), true));
        assert_ne!(cut_color(CutId(0), true), cut_color(CutId(0), false));
    }
}
