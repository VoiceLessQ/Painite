use std::collections::HashSet;

/// Chunks with a job currently running, per stage class.
#[derive(Default, Debug)]
pub struct Grid {
    /// FEATURES jobs: exclusion by chessboard distance.
    features: HashSet<(i32, i32)>,
    /// Center-only stages: exclusion by exact chunk.
    center: HashSet<(u8, i32, i32)>,
}

/// Write radius 1 on both sides: zones overlap iff distance <= 2.
pub const FEATURE_EXCLUSION: i32 = 2;

impl Grid {
    pub fn features_free(&self, cx: i32, cz: i32) -> bool {
        // Active set stays small (bounded by max_features), so a scan beats
        // a spatial index here.
        self.features
            .iter()
            .all(|&(ax, az)| (ax - cx).abs().max((az - cz).abs()) > FEATURE_EXCLUSION)
    }

    pub fn center_free(&self, stage: u8, cx: i32, cz: i32) -> bool {
        !self.center.contains(&(stage, cx, cz))
    }

    pub fn insert_features(&mut self, cx: i32, cz: i32) -> bool {
        self.features.insert((cx, cz))
    }

    pub fn remove_features(&mut self, cx: i32, cz: i32) -> bool {
        self.features.remove(&(cx, cz))
    }

    pub fn insert_center(&mut self, stage: u8, cx: i32, cz: i32) -> bool {
        self.center.insert((stage, cx, cz))
    }

    pub fn remove_center(&mut self, stage: u8, cx: i32, cz: i32) -> bool {
        self.center.remove(&(stage, cx, cz))
    }

    pub fn active_features(&self) -> usize {
        self.features.len()
    }

    pub fn active_total(&self) -> usize {
        self.features.len() + self.center.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn features_exclusion_is_chessboard_radius_two() {
        let mut g = Grid::default();
        assert!(g.insert_features(0, 0));
        for (x, z) in [(1, 0), (2, 2), (-2, 1), (0, -2)] {
            assert!(!g.features_free(x, z), "({x},{z}) overlaps");
        }
        for (x, z) in [(3, 0), (-3, 3), (0, 3), (3, -3)] {
            assert!(g.features_free(x, z), "({x},{z}) is clear");
        }
        assert!(g.remove_features(0, 0));
        assert!(g.features_free(1, 0));
    }

    #[test]
    fn center_stages_do_not_block_each_other() {
        let mut g = Grid::default();
        assert!(g.insert_center(0, 5, 5));
        assert!(!g.center_free(0, 5, 5));
        assert!(g.center_free(1, 5, 5));
        assert!(g.center_free(0, 6, 5));
        assert!(g.features_free(5, 5));
    }
}
