//! Explicit HDR mastering information. Never invent display metadata for real content.
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metadata {
    /// R, G, B CIE xy coordinates in units of 1/50000.
    pub primaries: [[u32; 2]; 3],
    pub white_point: [u32; 2],
    /// cd/m² in units of 1/10000, as used by HEVC mastering-display SEI.
    pub min_luminance: u32,
    pub max_luminance: u32,
    pub max_cll: u32,
    pub max_fall: u32,
}
impl Metadata {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.primaries
                .iter()
                .chain(std::iter::once(&self.white_point))
                .all(|xy| xy[0] > 0 && xy[1] > 0 && xy[0] as u64 + xy[1] as u64 <= 50000),
            "invalid HDR chromaticity"
        );
        anyhow::ensure!(
            self.max_luminance > self.min_luminance && self.max_luminance <= 100_000_000,
            "invalid HDR mastering luminance"
        );
        anyhow::ensure!(
            self.max_fall <= self.max_cll && self.max_cll <= 10000,
            "invalid HDR content light level"
        );
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mastering_metadata_rejects_invalid_luminance_and_chromaticity() {
        let mut m = Metadata {
            primaries: [[35400, 14600], [8500, 39850], [6550, 2300]],
            white_point: [15635, 16450],
            min_luminance: 50,
            max_luminance: 10_000_000,
            max_cll: 1000,
            max_fall: 400,
        };
        assert!(m.validate().is_ok());
        m.max_fall = 1001;
        assert!(m.validate().is_err());
        m.max_fall = 400;
        m.white_point = [50000, 50000];
        assert!(m.validate().is_err());
    }
}
