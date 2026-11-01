//! Floating geometry and stacking policy, independent of Wayland and Emacs.

use std::collections::{HashMap, HashSet};
use std::hash::Hash;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SizeHints {
    pub min_w: i32,
    pub min_h: i32,
    pub max_w: i32,
    pub max_h: i32,
}

impl SizeHints {
    pub fn fixed(&self) -> bool {
        self.min_w > 0 && self.min_h > 0 && self.min_w == self.max_w && self.min_h == self.max_h
    }

    fn constrain(value: i32, minimum: i32, maximum: i32, available: i32) -> i32 {
        let maximum = if maximum > 0 {
            maximum.min(available)
        } else {
            available
        }
        .max(1);
        value.clamp(minimum.clamp(1, maximum), maximum)
    }

    pub fn size(&self, w: i32, h: i32, area: Rect) -> (i32, i32) {
        (
            Self::constrain(w, self.min_w, self.max_w, area.w),
            Self::constrain(h, self.min_h, self.max_h, area.h),
        )
    }
}

impl Rect {
    pub fn fit(self, area: Rect, hints: SizeHints) -> Self {
        let area = Rect {
            w: area.w.max(1),
            h: area.h.max(1),
            ..area
        };
        let (w, h) = hints.size(self.w, self.h, area);
        Self {
            x: self.x.clamp(area.x, area.x.saturating_add(area.w - w)),
            y: self.y.clamp(area.y, area.y.saturating_add(area.h - h)),
            w,
            h,
        }
    }

    pub fn centered(w: i32, h: i32, parent: Rect, area: Rect, hints: SizeHints) -> Self {
        let (w, h) = hints.size(w, h, area);
        Self {
            x: parent.x.saturating_add((parent.w - w) / 2),
            y: parent.y.saturating_add((parent.h - h) / 2),
            w,
            h,
        }
        .fit(area, hints)
    }

    pub fn moved(self, dx: i32, dy: i32, area: Rect, hints: SizeHints) -> Self {
        Self {
            x: self.x.saturating_add(dx),
            y: self.y.saturating_add(dy),
            ..self
        }
        .fit(area, hints)
    }

    /// Resize from cumulative motion, keeping the opposite corner anchored.
    pub fn resized(
        self,
        dx: i32,
        dy: i32,
        edges: ResizeEdges,
        area: Rect,
        hints: SizeHints,
    ) -> Self {
        let w = self.w.saturating_add(if edges.left {
            dx.saturating_neg()
        } else if edges.right {
            dx
        } else {
            0
        });
        let h = self.h.saturating_add(if edges.top {
            dy.saturating_neg()
        } else if edges.bottom {
            dy
        } else {
            0
        });
        let (w, h) = hints.size(w, h, area);
        Self {
            x: if edges.left {
                self.x.saturating_add(self.w - w)
            } else {
                self.x
            },
            y: if edges.top {
                self.y.saturating_add(self.h - h)
            } else {
                self.y
            },
            w,
            h,
        }
        .fit(area, hints)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResizeEdges {
    pub left: bool,
    pub right: bool,
    pub top: bool,
    pub bottom: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Placement {
    pub explicit: Option<bool>,
    pub automatic: bool,
    /// Kept while tiled and fullscreen so re-floating restores the user's box.
    pub geometry: Option<Rect>,
}

impl Placement {
    pub fn floating(&self) -> bool {
        self.explicit.unwrap_or(self.automatic)
    }

    pub fn geometry(&self, tiled: Rect) -> Rect {
        if self.floating() {
            self.geometry.unwrap_or(tiled)
        } else {
            tiled
        }
    }

    pub fn detach_parent(&mut self, still_fixed_size: bool) {
        // A disappearing parent must not suddenly tile or reposition a dialog.
        // Preserve an explicit user choice to tile it, too.
        self.explicit.get_or_insert(true);
        self.automatic = still_fixed_size;
    }
}

#[derive(Clone, Debug)]
pub struct StackEntry<K> {
    pub id: K,
    pub parent: Option<K>,
    pub floating: bool,
    pub order: u64,
}

/// Tiled surfaces first, then floating trees, keeping children above parents.
/// Raising any descendant raises the whole floating tree, deterministically.
pub fn stacking_order<K: Clone + Eq + Hash>(entries: Vec<StackEntry<K>>) -> Vec<K> {
    let by_id: HashMap<_, _> = entries
        .iter()
        .map(|entry| (entry.id.clone(), entry))
        .collect();
    let mut groups: HashMap<K, Vec<&StackEntry<K>>> = HashMap::new();
    let mut tiled = Vec::new();
    for entry in &entries {
        let mut cursor = Some(entry);
        let mut floating_root = None;
        let mut seen = HashSet::new();
        while let Some(current) = cursor {
            if !seen.insert(current.id.clone()) {
                break;
            }
            if current.floating {
                floating_root = Some(current.id.clone());
            }
            cursor = current
                .parent
                .as_ref()
                .and_then(|parent| by_id.get(parent).copied());
        }
        if let Some(root) = floating_root {
            groups.entry(root).or_default().push(entry);
        } else {
            tiled.push(entry);
        }
    }
    tiled.sort_by_key(|entry| entry.order);
    let mut result: Vec<_> = tiled.into_iter().map(|entry| entry.id.clone()).collect();
    let mut groups: Vec<_> = groups.into_values().collect();
    groups.sort_by_key(|group| group.iter().map(|entry| entry.order).max().unwrap_or(0));
    for mut group in groups {
        group.sort_by_key(|entry| entry.order);
        while !group.is_empty() {
            let next = group
                .iter()
                .position(|entry| {
                    !group
                        .iter()
                        .any(|other| entry.parent.as_ref() == Some(&other.id))
                })
                .unwrap_or(0);
            result.push(group.remove(next).id.clone());
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    const AREA: Rect = Rect {
        x: 0,
        y: 0,
        w: 1920,
        h: 1040,
    };

    #[test]
    fn tile_float_tile_restores_the_latest_tile_and_saved_float() {
        let tile = Rect {
            x: 0,
            y: 0,
            w: 960,
            h: 1040,
        };
        let saved = Rect {
            x: 120,
            y: 90,
            w: 640,
            h: 480,
        };
        let mut placement = Placement::default();
        assert_eq!(placement.geometry(tile), tile);
        placement.explicit = Some(true);
        placement.geometry = Some(saved);
        assert_eq!(placement.geometry(tile), saved);
        placement.explicit = Some(false);
        let changed_tile = Rect { w: 1920, ..tile };
        assert_eq!(placement.geometry(changed_tile), changed_tile);
        placement.explicit = Some(true);
        assert_eq!(placement.geometry(changed_tile), saved);
    }

    #[test]
    fn dialogs_center_over_the_parent_and_stay_inside_the_work_area() {
        let parent = Rect {
            x: 100,
            y: 60,
            w: 1200,
            h: 800,
        };
        assert_eq!(
            Rect::centered(640, 480, parent, AREA, SizeHints::default()),
            Rect {
                x: 380,
                y: 220,
                w: 640,
                h: 480
            }
        );
        let offscreen = Rect {
            x: 1800,
            y: 1000,
            ..parent
        };
        assert_eq!(
            Rect::centered(640, 480, offscreen, AREA, SizeHints::default()),
            Rect {
                x: 1280,
                y: 560,
                w: 640,
                h: 480
            }
        );
    }

    #[test]
    fn pointer_resize_respects_hints_and_anchors_the_opposite_edge() {
        let initial = Rect {
            x: 200,
            y: 150,
            w: 640,
            h: 480,
        };
        let hints = SizeHints {
            min_w: 320,
            min_h: 240,
            max_w: 900,
            max_h: 700,
        };
        let edges = ResizeEdges {
            left: true,
            top: true,
            ..Default::default()
        };
        let resized = initial.resized(600, 500, edges, AREA, hints);
        assert_eq!(
            resized,
            Rect {
                x: 520,
                y: 390,
                w: 320,
                h: 240
            }
        );
        assert_eq!(resized.x + resized.w, initial.x + initial.w);
        assert_eq!(resized.y + resized.h, initial.y + initial.h);
    }

    #[test]
    fn pointer_motion_is_cumulative_and_handles_extreme_deltas() {
        let initial = Rect {
            x: 200,
            y: 150,
            w: 640,
            h: 480,
        };
        assert_eq!(initial.moved(30, 40, AREA, SizeHints::default()).x, 230);
        assert_eq!(initial.moved(60, 80, AREA, SizeHints::default()).x, 260);
        assert_eq!(
            initial.moved(i32::MAX, i32::MIN, AREA, SizeHints::default()),
            Rect {
                x: 1280,
                y: 0,
                ..initial
            }
        );
    }

    #[test]
    fn automatic_dialog_policy_can_be_overridden_explicitly() {
        let mut placement = Placement {
            automatic: true,
            ..Default::default()
        };
        assert!(placement.floating());
        placement.explicit = Some(false);
        assert!(!placement.floating());
        placement.explicit = Some(true);
        placement.automatic = false;
        assert!(placement.floating());
    }

    #[test]
    fn detaching_a_parent_preserves_geometry_and_explicit_user_policy() {
        let saved = Rect {
            x: 100,
            y: 90,
            w: 400,
            h: 300,
        };
        let mut dialog = Placement {
            automatic: true,
            geometry: Some(saved),
            ..Default::default()
        };
        dialog.detach_parent(false);
        assert!(dialog.floating());
        assert_eq!(dialog.geometry, Some(saved));
        dialog.explicit = Some(false);
        dialog.detach_parent(false);
        assert!(!dialog.floating());
    }

    #[test]
    fn floating_children_stay_above_parents_and_focused_trees_are_raised() {
        let entry = |id, parent, floating, order| StackEntry {
            id,
            parent,
            floating,
            order,
        };
        let stack = stacking_order(vec![
            entry(1, None, false, 1),
            entry(2, None, true, 3),
            entry(3, Some(2), true, 10),
            entry(4, None, true, 5),
            entry(5, Some(1), true, 2),
        ]);
        assert_eq!(stack, [1, 5, 4, 2, 3]);
    }
}
