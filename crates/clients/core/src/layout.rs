//! Layout system — split tree to positioned rectangles.
//!
//! Binary split layout: a LayoutTree is either a Leaf (single tile)
//! or a Split (two children with a ratio on a given axis).

use crate::ids::TileId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Positioned rectangle for a tile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl Rect {
    pub fn new(x: u16, y: u16, w: u16, h: u16) -> Self {
        Self { x, y, w, h }
    }

    pub fn zero() -> Self {
        Self {
            x: 0,
            y: 0,
            w: 0,
            h: 0,
        }
    }

    pub fn area(&self) -> u32 {
        self.w as u32 * self.h as u32
    }

    pub fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }
}

/// Split axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SplitAxis {
    Horizontal, // left/right split
    Vertical,   // top/bottom split
}

/// Recursive layout tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum LayoutTree {
    Leaf(TileId),
    Split {
        axis: SplitAxis,
        ratio: f32,
        first: Box<LayoutTree>,
        second: Box<LayoutTree>,
    },
}

impl LayoutTree {
    /// Collect all tile IDs in the tree.
    pub fn tile_ids(&self) -> Vec<TileId> {
        match self {
            LayoutTree::Leaf(id) => vec![*id],
            LayoutTree::Split { first, second, .. } => {
                let mut ids = first.tile_ids();
                ids.extend(second.tile_ids());
                ids
            }
        }
    }

    /// Default 3-pane layout: thread list (left) | thread (right-top) + status (right-bottom).
    pub fn default_three_pane(list_id: TileId, thread_id: TileId, status_id: TileId) -> Self {
        LayoutTree::Split {
            axis: SplitAxis::Horizontal,
            ratio: 0.25,
            first: Box::new(LayoutTree::Leaf(list_id)),
            second: Box::new(LayoutTree::Split {
                axis: SplitAxis::Vertical,
                ratio: 0.85,
                first: Box::new(LayoutTree::Leaf(thread_id)),
                second: Box::new(LayoutTree::Leaf(status_id)),
            }),
        }
    }
}

/// Compute positioned rectangles for each tile in the tree.
pub fn layout_rects(tree: &LayoutTree, viewport: Rect) -> HashMap<TileId, Rect> {
    let mut rects = HashMap::new();
    layout_rects_recursive(tree, viewport, &mut rects);
    rects
}

fn layout_rects_recursive(tree: &LayoutTree, rect: Rect, out: &mut HashMap<TileId, Rect>) {
    match tree {
        LayoutTree::Leaf(id) => {
            out.insert(*id, rect);
        }
        LayoutTree::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            let ratio = ratio.clamp(0.1, 0.9);
            match axis {
                SplitAxis::Horizontal => {
                    let split_x = (rect.w as f32 * ratio) as u16;
                    let first_rect = Rect::new(rect.x, rect.y, split_x, rect.h);
                    let second_rect = Rect::new(
                        rect.x + split_x,
                        rect.y,
                        rect.w.saturating_sub(split_x),
                        rect.h,
                    );
                    layout_rects_recursive(first, first_rect, out);
                    layout_rects_recursive(second, second_rect, out);
                }
                SplitAxis::Vertical => {
                    let split_y = (rect.h as f32 * ratio) as u16;
                    let first_rect = Rect::new(rect.x, rect.y, rect.w, split_y);
                    let second_rect = Rect::new(
                        rect.x,
                        rect.y + split_y,
                        rect.w,
                        rect.h.saturating_sub(split_y),
                    );
                    layout_rects_recursive(first, first_rect, out);
                    layout_rects_recursive(second, second_rect, out);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_default_workspace_has_expected_tiles() {
        let tree = LayoutTree::default_three_pane(TileId::new(1), TileId::new(2), TileId::new(3));
        let ids = tree.tile_ids();
        assert_eq!(ids.len(), 3);
        assert!(ids.contains(&TileId::new(1)));
        assert!(ids.contains(&TileId::new(2)));
        assert!(ids.contains(&TileId::new(3)));
    }

    #[test]
    fn layout_split_rects_sum_to_viewport() {
        let tree = LayoutTree::default_three_pane(TileId::new(1), TileId::new(2), TileId::new(3));
        let vp = Rect::new(0, 0, 200, 60);
        let rects = layout_rects(&tree, vp);

        assert_eq!(rects.len(), 3);

        // All rects should fit within viewport
        for rect in rects.values() {
            assert!(rect.x + rect.w <= vp.w);
            assert!(rect.y + rect.h <= vp.h);
        }

        // Total area should be close to viewport area
        let total: u32 = rects.values().map(|r| r.area()).sum();
        assert_eq!(total, vp.area());
    }

    #[test]
    fn layout_handles_tiny_viewport() {
        let tree = LayoutTree::Split {
            axis: SplitAxis::Horizontal,
            ratio: 0.5,
            first: Box::new(LayoutTree::Leaf(TileId::new(1))),
            second: Box::new(LayoutTree::Leaf(TileId::new(2))),
        };
        let vp = Rect::new(0, 0, 10, 1);
        let rects = layout_rects(&tree, vp);
        assert_eq!(rects.len(), 2);
    }

    #[test]
    fn layout_handles_zero_viewport() {
        let tree = LayoutTree::Leaf(TileId::new(1));
        let vp = Rect::new(0, 0, 0, 0);
        let rects = layout_rects(&tree, vp);
        assert_eq!(rects.len(), 1);
        assert!(rects[&TileId::new(1)].is_empty());
    }
}
