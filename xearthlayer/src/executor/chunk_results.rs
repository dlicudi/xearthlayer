//! Chunk download results for tile generation.
//!
//! This module provides types for tracking successful and failed chunk downloads
//! during tile generation. A tile consists of 256 chunks (16×16 grid), and each
//! chunk is downloaded independently with retry logic.

/// Result of downloading chunks for a tile.
///
/// Tracks successful and failed chunks separately to enable partial
/// results with magenta placeholders for failures.
#[derive(Debug, Clone)]
pub struct ChunkResults {
    /// Successfully downloaded chunks: (row, col) -> JPEG data
    pub successes: Vec<ChunkSuccess>,

    /// Failed chunks: (row, col) with error info
    pub failures: Vec<ChunkFailure>,

    /// Number of chunks per side of this result's grid.
    ///
    /// `16` for a natively-sampled tile (256 chunks). When the source zoom is
    /// capped, a tile is built from a smaller `grid_side × grid_side` block that
    /// is later upscaled — e.g. `4` (16 chunks) for a two-level downsample.
    /// Completeness and the assembly canvas size are driven by this.
    grid_side: u32,
}

/// A successfully downloaded chunk.
#[derive(Debug, Clone)]
pub struct ChunkSuccess {
    /// Chunk row within tile (0-15)
    pub row: u8,
    /// Chunk column within tile (0-15)
    pub col: u8,
    /// Raw image data (JPEG)
    pub data: Vec<u8>,
}

/// A chunk that failed to download after all retries.
#[derive(Debug, Clone)]
pub struct ChunkFailure {
    /// Chunk row within tile (0-15)
    pub row: u8,
    /// Chunk column within tile (0-15)
    pub col: u8,
    /// Number of attempts made
    pub attempts: u32,
    /// Last error message
    pub last_error: String,
}

impl ChunkResults {
    /// Creates a new empty ChunkResults for a native 16×16 (256-chunk) tile.
    pub fn new() -> Self {
        Self::with_grid_side(crate::coord::CHUNKS_PER_TILE_SIDE)
    }

    /// Creates a new empty ChunkResults for a `grid_side × grid_side` grid.
    ///
    /// Use `16` for native sampling; a smaller value for a capped/upscaled tile.
    pub fn with_grid_side(grid_side: u32) -> Self {
        Self {
            successes: Vec::with_capacity((grid_side * grid_side) as usize),
            failures: Vec::new(),
            grid_side,
        }
    }

    /// Returns the number of chunks per side of this result's grid.
    #[inline]
    pub fn grid_side(&self) -> u32 {
        self.grid_side
    }

    /// Returns the number of chunks a complete grid contains (`grid_side²`).
    #[inline]
    pub fn expected_chunks(&self) -> usize {
        (self.grid_side * self.grid_side) as usize
    }

    /// Adds a successful chunk download.
    pub fn add_success(&mut self, row: u8, col: u8, data: Vec<u8>) {
        self.successes.push(ChunkSuccess { row, col, data });
    }

    /// Adds a failed chunk download.
    pub fn add_failure(&mut self, row: u8, col: u8, attempts: u32, last_error: String) {
        self.failures.push(ChunkFailure {
            row,
            col,
            attempts,
            last_error,
        });
    }

    /// Returns the number of successful downloads.
    #[inline]
    pub fn success_count(&self) -> usize {
        self.successes.len()
    }

    /// Returns the number of failed downloads.
    #[inline]
    pub fn failure_count(&self) -> usize {
        self.failures.len()
    }

    /// Returns the total number of chunks processed (should be `expected_chunks`).
    #[inline]
    pub fn total_count(&self) -> usize {
        self.successes.len() + self.failures.len()
    }

    /// Returns true if every chunk in the grid downloaded successfully.
    ///
    /// For a native tile this means all 256 chunks; for a capped grid it means
    /// all `grid_side²` chunks. A complete capped grid is a fully-successful
    /// (upscaled) tile and is therefore cacheable — see issue #180.
    #[inline]
    pub fn is_complete(&self) -> bool {
        self.successes.len() == self.expected_chunks() && self.failures.is_empty()
    }

    /// Returns the success rate as a percentage (0.0 - 100.0).
    pub fn success_rate(&self) -> f64 {
        let total = self.total_count();
        if total == 0 {
            return 0.0;
        }
        (self.successes.len() as f64 / total as f64) * 100.0
    }

    /// Gets the data for a specific chunk position, if it was successful.
    pub fn get(&self, row: u8, col: u8) -> Option<&[u8]> {
        self.successes
            .iter()
            .find(|c| c.row == row && c.col == col)
            .map(|c| c.data.as_slice())
    }

    /// Returns the total bytes downloaded across all successful chunks.
    ///
    /// This is useful for tracking network throughput in metrics.
    pub fn total_bytes_downloaded(&self) -> u64 {
        self.successes.iter().map(|c| c.data.len() as u64).sum()
    }
}

impl Default for ChunkResults {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chunk_results_new() {
        let results = ChunkResults::new();
        assert_eq!(results.success_count(), 0);
        assert_eq!(results.failure_count(), 0);
        assert!(!results.is_complete());
    }

    #[test]
    fn test_chunk_results_add_success() {
        let mut results = ChunkResults::new();
        results.add_success(0, 0, vec![1, 2, 3]);
        results.add_success(0, 1, vec![4, 5, 6]);

        assert_eq!(results.success_count(), 2);
        assert_eq!(results.get(0, 0), Some(&[1u8, 2, 3][..]));
        assert_eq!(results.get(0, 1), Some(&[4u8, 5, 6][..]));
        assert_eq!(results.get(0, 2), None);
    }

    #[test]
    fn test_chunk_results_add_failure() {
        let mut results = ChunkResults::new();
        results.add_failure(5, 10, 3, "timeout".to_string());

        assert_eq!(results.failure_count(), 1);
        assert_eq!(results.failures[0].row, 5);
        assert_eq!(results.failures[0].col, 10);
        assert_eq!(results.failures[0].attempts, 3);
    }

    #[test]
    fn test_chunk_results_success_rate() {
        let mut results = ChunkResults::new();

        // Add 200 successes
        for i in 0..200 {
            results.add_success((i / 16) as u8, (i % 16) as u8, vec![0]);
        }

        // Add 56 failures
        for i in 200..256 {
            results.add_failure((i / 16) as u8, (i % 16) as u8, 1, "error".to_string());
        }

        assert_eq!(results.total_count(), 256);
        assert!((results.success_rate() - 78.125).abs() < 0.001);
    }

    #[test]
    fn test_chunk_results_is_complete() {
        let mut results = ChunkResults::new();

        // Add all 256 chunks as successes
        for row in 0..16u8 {
            for col in 0..16u8 {
                results.add_success(row, col, vec![0]);
            }
        }

        assert!(results.is_complete());
        assert_eq!(results.success_rate(), 100.0);
    }

    #[test]
    fn test_capped_grid_is_complete_at_grid_side_squared() {
        // A 4×4 (capped, Δ2) grid is complete at 16 chunks, not 256.
        let mut results = ChunkResults::with_grid_side(4);
        assert_eq!(results.grid_side(), 4);
        assert_eq!(results.expected_chunks(), 16);

        for row in 0..4u8 {
            for col in 0..4u8 {
                assert!(!results.is_complete());
                results.add_success(row, col, vec![0]);
            }
        }
        assert!(results.is_complete());
        assert_eq!(results.success_rate(), 100.0);
    }

    #[test]
    fn test_capped_grid_incomplete_with_one_failure() {
        let mut results = ChunkResults::with_grid_side(4);
        for i in 0..15u8 {
            results.add_success(i / 4, i % 4, vec![0]);
        }
        results.add_failure(3, 3, 1, "timeout".to_string());
        assert_eq!(results.total_count(), 16);
        assert!(!results.is_complete());
    }
}
