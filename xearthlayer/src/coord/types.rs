//! Coordinate type definitions

use std::fmt;

/// Web Mercator valid latitude range
pub const MIN_LAT: f64 = -85.05112878;
pub const MAX_LAT: f64 = 85.05112878;

/// Valid longitude range
pub const MIN_LON: f64 = -180.0;
pub const MAX_LON: f64 = 180.0;

/// Standard zoom levels for X-Plane
pub const MIN_ZOOM: u8 = 0;
pub const MAX_ZOOM: u8 = 18;

/// Number of chunks along each side of a tile (16×16 = 256 chunks per tile).
pub const CHUNKS_PER_TILE_SIDE: u32 = 16;

/// Zoom level offset between tile zoom and chunk zoom (log₂(CHUNKS_PER_TILE_SIDE)).
///
/// A tile at zoom Z is composed of chunks at zoom Z+4, because 2⁴ = 16.
pub const CHUNK_ZOOM_OFFSET: u8 = 4;

/// Tile coordinates in Web Mercator / Slippy Map system.
///
/// Represents a 4096×4096 pixel tile composed of 16×16 chunks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TileCoord {
    /// Y coordinate (north-south), 0 at north
    pub row: u32,
    /// X coordinate (east-west), 0 at west
    pub col: u32,
    /// Zoom level (0-18)
    pub zoom: u8,
}

impl TileCoord {
    /// Returns an iterator over all 256 chunks in this tile.
    ///
    /// Chunks are yielded in row-major order (row 0 columns 0-15, row 1 columns 0-15, etc.).
    #[inline]
    pub fn chunks(&self) -> TileChunksIterator {
        TileChunksIterator {
            tile: *self,
            current: 0,
        }
    }

    /// Converts this tile to geographic coordinates (center of tile).
    ///
    /// Returns (latitude, longitude) in degrees.
    #[inline]
    pub fn to_lat_lon(&self) -> (f64, f64) {
        use std::f64::consts::PI;

        let n = 2.0_f64.powi(self.zoom as i32);

        // Convert tile X coordinate to longitude (add 0.5 for center)
        let lon = (self.col as f64 + 0.5) / n * 360.0 - 180.0;

        // Convert tile Y coordinate to latitude using inverse Web Mercator (add 0.5 for center)
        let y = (self.row as f64 + 0.5) / n;
        let lat_rad = (PI * (1.0 - 2.0 * y)).sinh().atan();
        let lat = lat_rad * 180.0 / PI;

        (lat, lon)
    }

    /// Returns the global chunk coordinates of this tile's origin (top-left chunk).
    ///
    /// DDS filenames use chunk-level global coordinates:
    /// `{chunk_row}_{chunk_col}_{map_type}{chunk_zoom}.dds`
    ///
    /// This is the canonical conversion from tile coordinates to the
    /// chunk-level format used in DDS/TER filenames on disk.
    #[inline]
    pub fn chunk_origin(&self) -> (u32, u32, u8) {
        (
            self.row * CHUNKS_PER_TILE_SIDE,
            self.col * CHUNKS_PER_TILE_SIDE,
            self.zoom + CHUNK_ZOOM_OFFSET,
        )
    }

    /// Returns the DSF tile (1°×1°) that contains this DDS tile.
    ///
    /// X-Plane's scenery is organized into 1°×1° DSF tiles. This method
    /// converts the DDS tile coordinates to the containing DSF tile name.
    ///
    /// # Returns
    ///
    /// A string in X-Plane's DSF naming format, e.g., "+53+009" for
    /// a tile at 53°N, 9°E.
    #[inline]
    pub fn to_dsf_tile_name(&self) -> String {
        let (lat, lon) = self.to_lat_lon();
        let dsf_lat = lat.floor() as i32;
        let dsf_lon = lon.floor() as i32;
        super::format_dsf_name(dsf_lat, dsf_lon)
    }

    /// Computes the source sampling grid for this tile under an optional zoom cap.
    ///
    /// `max_source_zoom` is expressed in **chunk-zoom (ZL) terms** — the same
    /// units as the DDS filename (`..._ZL16.dds`) and the `generation.max_source_zoom`
    /// config — *not* the tile zoom. A tile's requested chunk zoom is
    /// `self.zoom + CHUNK_ZOOM_OFFSET` (e.g. a `zoom = 14` tile is ZL18).
    ///
    /// - `None` or `>= requested chunk zoom` → identity: the native 16×16 chunk
    ///   grid at `self.zoom`, `upscale_factor == 1` (behaviour unchanged).
    /// - `< requested chunk zoom` → the *same geographic box* fetched at the
    ///   capped chunk zoom as a smaller `grid_side × grid_side` grid, to be
    ///   upscaled to the full 4096×4096 texture by `upscale_factor`.
    ///
    /// The cap is clamped to at most `MAX_SOURCE_DOWNSAMPLE` levels below the
    /// request (a single 256×256 chunk is the most a tile can be built from; you
    /// cannot downsample below one chunk per side). Because `grid_side` always
    /// divides 16 and the origin is a multiple of `grid_side`, the fetched chunks
    /// always fall within a single source tile — no straddling.
    #[inline]
    pub fn source_grid(&self, max_source_zoom: Option<u8>) -> SourceGrid {
        // Work entirely in chunk-zoom (ZL) units, matching the cap and filenames.
        let requested_chunk_zoom = self.zoom + CHUNK_ZOOM_OFFSET;
        let min_chunk_zoom = requested_chunk_zoom.saturating_sub(MAX_SOURCE_DOWNSAMPLE);
        let capped_chunk_zoom = max_source_zoom
            .unwrap_or(requested_chunk_zoom)
            .clamp(min_chunk_zoom, requested_chunk_zoom);
        let delta = requested_chunk_zoom - capped_chunk_zoom;

        SourceGrid {
            source_zoom: self.zoom - delta,
            chunk_zoom: capped_chunk_zoom,
            grid_side: CHUNKS_PER_TILE_SIDE >> delta,
            origin_chunk_row: (self.row * CHUNKS_PER_TILE_SIDE) >> delta,
            origin_chunk_col: (self.col * CHUNKS_PER_TILE_SIDE) >> delta,
            upscale_factor: 1u32 << delta,
        }
    }
}

/// Maximum number of zoom levels a tile may be downsampled when capping the
/// source zoom. A tile is 16×16 chunks (`2^4`), so beyond 4 levels the grid
/// would shrink below a single chunk per side, which the chunk model cannot
/// represent. Caps below `self.zoom - 4` are clamped to this floor.
pub const MAX_SOURCE_DOWNSAMPLE: u8 = 4;

/// Iterator over all chunks in a tile.
///
/// Yields 256 chunks (16×16) in row-major order.
#[derive(Debug, Clone)]
pub struct TileChunksIterator {
    tile: TileCoord,
    current: u16,
}

impl Iterator for TileChunksIterator {
    type Item = ChunkCoord;

    fn next(&mut self) -> Option<Self::Item> {
        let total_chunks = CHUNKS_PER_TILE_SIDE * CHUNKS_PER_TILE_SIDE;
        if self.current >= total_chunks as u16 {
            return None;
        }

        // Calculate chunk position in row-major order
        let chunk_row = (self.current / CHUNKS_PER_TILE_SIDE as u16) as u8;
        let chunk_col = (self.current % CHUNKS_PER_TILE_SIDE as u16) as u8;

        self.current += 1;

        Some(ChunkCoord {
            tile_row: self.tile.row,
            tile_col: self.tile.col,
            chunk_row,
            chunk_col,
            zoom: self.tile.zoom,
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = (256 - self.current) as usize;
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for TileChunksIterator {
    fn len(&self) -> usize {
        (256 - self.current) as usize
    }
}

/// Chunk coordinates within a tile.
///
/// Represents a 256×256 pixel chunk within a 4096×4096 tile.
/// Each tile contains 16×16 = 256 chunks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChunkCoord {
    /// Tile row coordinate
    pub tile_row: u32,
    /// Tile column coordinate
    pub tile_col: u32,
    /// Chunk row within tile (0-15)
    pub chunk_row: u8,
    /// Chunk column within tile (0-15)
    pub chunk_col: u8,
    /// Zoom level
    pub zoom: u8,
}

impl ChunkCoord {
    /// Converts chunk coordinates to global tile coordinates.
    ///
    /// This is used when requesting chunks from satellite imagery providers,
    /// which expect global tile coordinates at the chunk resolution.
    ///
    /// Returns coordinates at zoom+4, because a tile at zoom Z is composed
    /// of 16×16 chunks, where each chunk is a 256×256 tile from zoom Z+4.
    #[inline]
    pub fn to_global_coords(&self) -> (u32, u32, u8) {
        let global_row = self.tile_row * CHUNKS_PER_TILE_SIDE + self.chunk_row as u32;
        let global_col = self.tile_col * CHUNKS_PER_TILE_SIDE + self.chunk_col as u32;
        (global_row, global_col, self.zoom + CHUNK_ZOOM_OFFSET)
    }
}

/// Describes how a tile's imagery is sampled when the source zoom is capped.
///
/// Produced by [`TileCoord::source_grid`]. The downloaded chunk grid is
/// `grid_side × grid_side` chunks at `chunk_zoom`, assembled into a
/// `grid_side * 256` square image and scaled by `upscale_factor` to the final
/// 4096×4096 texture. When the cap does not bite, this is the identity
/// (`grid_side == 16`, `upscale_factor == 1`) and no scaling occurs.
///
/// Invariant: `grid_side * 256 * upscale_factor == 4096`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceGrid {
    /// Tile zoom actually fetched (`== requested zoom` when uncapped).
    pub source_zoom: u8,
    /// Chunk (imagery) zoom of the fetched chunks (`source_zoom + 4`).
    pub chunk_zoom: u8,
    /// Number of chunks per side of the fetched grid (`16` when uncapped).
    pub grid_side: u32,
    /// Global chunk row of the grid's top-left chunk, at `chunk_zoom`.
    pub origin_chunk_row: u32,
    /// Global chunk column of the grid's top-left chunk, at `chunk_zoom`.
    pub origin_chunk_col: u32,
    /// Linear scale factor from the assembled image to 4096×4096 (`1` uncapped).
    pub upscale_factor: u32,
}

impl SourceGrid {
    /// Returns true when this grid samples at native resolution (no upscaling).
    #[inline]
    pub fn is_native(&self) -> bool {
        self.upscale_factor == 1
    }
}

/// Errors that can occur during coordinate conversion.
#[derive(Debug, Clone, PartialEq)]
pub enum CoordError {
    /// Latitude is outside valid range (-85.05112878 to 85.05112878)
    InvalidLatitude(f64),
    /// Longitude is outside valid range (-180.0 to 180.0)
    InvalidLongitude(f64),
    /// Zoom level is outside valid range (0 to 18)
    InvalidZoom(u8),
    /// Quadkey contains invalid characters or is too long
    InvalidQuadkey(String),
}

impl fmt::Display for CoordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CoordError::InvalidLatitude(lat) => {
                write!(
                    f,
                    "Invalid latitude: {} (must be between {} and {})",
                    lat, MIN_LAT, MAX_LAT
                )
            }
            CoordError::InvalidLongitude(lon) => {
                write!(
                    f,
                    "Invalid longitude: {} (must be between {} and {})",
                    lon, MIN_LON, MAX_LON
                )
            }
            CoordError::InvalidZoom(zoom) => {
                write!(
                    f,
                    "Invalid zoom level: {} (must be between {} and {})",
                    zoom, MIN_ZOOM, MAX_ZOOM
                )
            }
            CoordError::InvalidQuadkey(quadkey) => {
                write!(
                    f,
                    "Invalid quadkey: '{}' (must contain only digits 0-3 and length <= {})",
                    quadkey, MAX_ZOOM
                )
            }
        }
    }
}

impl std::error::Error for CoordError {}

#[cfg(test)]
mod source_grid_tests {
    use super::*;

    // The cap is in chunk-zoom (ZL) units. Real tiles have a *tile* zoom of
    // `ZL - CHUNK_ZOOM_OFFSET`, so a ZL18 tile is `zoom = 14` and a ZL16 tile is
    // `zoom = 12`. These helpers keep the tests in those real units.
    const ZL16_TILE_ZOOM: u8 = 16 - CHUNK_ZOOM_OFFSET; // 12
    const ZL18_TILE_ZOOM: u8 = 18 - CHUNK_ZOOM_OFFSET; // 14

    /// Helper: invariant that the assembled, upscaled image is always 4096².
    fn assert_fills_tile(g: &SourceGrid) {
        assert_eq!(
            g.grid_side * 256 * g.upscale_factor,
            4096,
            "grid {:?} does not fill a 4096 tile",
            g
        );
    }

    #[test]
    fn no_cap_is_identity() {
        let tile = TileCoord {
            row: 100,
            col: 200,
            zoom: ZL18_TILE_ZOOM,
        };
        let g = tile.source_grid(None);
        assert_eq!(g.source_zoom, ZL18_TILE_ZOOM);
        assert_eq!(g.chunk_zoom, 18); // ZL18
        assert_eq!(g.grid_side, 16);
        assert_eq!(g.upscale_factor, 1);
        assert_eq!(g.origin_chunk_row, 100 * 16);
        assert_eq!(g.origin_chunk_col, 200 * 16);
        assert!(g.is_native());
        assert_fills_tile(&g);
    }

    #[test]
    fn cap_at_or_above_request_is_identity() {
        // A ZL16 tile under caps of ZL16/17/18 must stay native.
        let tile = TileCoord {
            row: 1,
            col: 1,
            zoom: ZL16_TILE_ZOOM,
        };
        for cap in [Some(16), Some(17), Some(18)] {
            let g = tile.source_grid(cap);
            assert_eq!(g.grid_side, 16, "cap {:?} should be native", cap);
            assert_eq!(g.source_zoom, ZL16_TILE_ZOOM);
            assert_eq!(g.chunk_zoom, 16);
            assert_eq!(g.upscale_factor, 1);
            assert!(g.is_native());
        }
    }

    /// Regression test for the tile-zoom-vs-chunk-ZL unit bug found in the flight
    /// test: a ZL18 tile under a cap of ZL17 *must* downsample (the cap is in ZL
    /// units, not tile-zoom units). The old code compared 17 against tile zoom 14
    /// and never fired.
    #[test]
    fn zl18_capped_to_17_downsamples_one_level() {
        let tile = TileCoord {
            row: 1000,
            col: 2000,
            zoom: ZL18_TILE_ZOOM,
        };
        let g = tile.source_grid(Some(17));
        assert_eq!(g.source_zoom, 13); // tile zoom 14 - 1
        assert_eq!(g.chunk_zoom, 17); // ZL17
        assert_eq!(g.grid_side, 8);
        assert_eq!(g.upscale_factor, 2);
        assert_eq!(g.origin_chunk_row, (1000 * 16) >> 1);
        assert_eq!(g.origin_chunk_col, (2000 * 16) >> 1);
        assert!(!g.is_native(), "cap ZL17 must bite a ZL18 tile");
        assert_fills_tile(&g);
    }

    #[test]
    fn zl18_capped_to_16_downsamples_two_levels() {
        let tile = TileCoord {
            row: 1000,
            col: 2000,
            zoom: ZL18_TILE_ZOOM,
        };
        let g = tile.source_grid(Some(16));
        assert_eq!(g.source_zoom, 12); // ZL16 tile zoom
        assert_eq!(g.chunk_zoom, 16);
        assert_eq!(g.grid_side, 4); // 16 chunks vs 256 native -> 16x fewer
        assert_eq!(g.upscale_factor, 4);
        assert_eq!(g.origin_chunk_row, (1000 * 16) >> 2);
        assert_eq!(g.origin_chunk_col, (2000 * 16) >> 2);
        assert_fills_tile(&g);
    }

    #[test]
    fn max_downsample_is_single_chunk() {
        let tile = TileCoord {
            row: 5,
            col: 7,
            zoom: ZL18_TILE_ZOOM,
        };
        // Δ4 (ZL18 -> ZL14): one 256² chunk upscaled 16x to fill the tile.
        let g = tile.source_grid(Some(14));
        assert_eq!(g.chunk_zoom, 14);
        assert_eq!(g.grid_side, 1);
        assert_eq!(g.upscale_factor, 16);
        assert_fills_tile(&g);
    }

    #[test]
    fn cap_below_floor_is_clamped_to_max_downsample() {
        let tile = TileCoord {
            row: 5,
            col: 7,
            zoom: ZL18_TILE_ZOOM,
        };
        // Asking for ZL10 (Δ8 below ZL18) clamps to Δ4 — never a zero-side grid.
        let g = tile.source_grid(Some(10));
        assert_eq!(g.chunk_zoom, 18 - MAX_SOURCE_DOWNSAMPLE); // ZL14 floor
        assert_eq!(g.grid_side, 1);
        assert_eq!(g.upscale_factor, 16);
        assert!(g.grid_side >= 1, "grid_side must never underflow to 0");
        assert_fills_tile(&g);
    }

    #[test]
    fn zl16_native_is_unaffected_by_cap_17() {
        // The 95% case in the installed scenery: ZL16 tiles with a cap of ZL17
        // must download natively, untouched.
        let tile = TileCoord {
            row: 12754,
            col: 5279,
            zoom: ZL16_TILE_ZOOM,
        };
        let g = tile.source_grid(Some(17));
        assert_eq!(g.grid_side, 16);
        assert_eq!(g.chunk_zoom, 16);
        assert!(g.is_native());
    }

    #[test]
    fn fetched_grid_lies_within_a_single_source_tile() {
        // For every Δ and a spread of tile coords, the grid_side block must not
        // straddle a source-tile (16-chunk) boundary. Caps are in chunk-ZL units.
        for tile_zoom in [ZL16_TILE_ZOOM, ZL18_TILE_ZOOM] {
            let requested_zl = tile_zoom + CHUNK_ZOOM_OFFSET;
            for &cap in &[requested_zl - 1, requested_zl - 2] {
                for row in [0u32, 1, 3, 7, 15, 16, 33, 1000] {
                    for col in [0u32, 1, 4, 15, 16, 31, 2000] {
                        let g = TileCoord {
                            row,
                            col,
                            zoom: tile_zoom,
                        }
                        .source_grid(Some(cap));
                        let first_tile_row = g.origin_chunk_row / 16;
                        let last_tile_row = (g.origin_chunk_row + g.grid_side - 1) / 16;
                        let first_tile_col = g.origin_chunk_col / 16;
                        let last_tile_col = (g.origin_chunk_col + g.grid_side - 1) / 16;
                        assert_eq!(
                            first_tile_row, last_tile_row,
                            "row straddle at {row},{col} tile_zoom {tile_zoom} cap {cap}: {g:?}"
                        );
                        assert_eq!(
                            first_tile_col, last_tile_col,
                            "col straddle at {row},{col} tile_zoom {tile_zoom} cap {cap}: {g:?}"
                        );
                    }
                }
            }
        }
    }
}
