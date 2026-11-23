//! OBN3 revision 4 record geometry and header validation.
use super::continuous::{BINS, BYTES, Header, TICK_HZ};

pub const ID: u32 = 0x334e424f;
pub const OBN5_ID: u32 = 0x354e424f;
pub const OBN5_VIEW_STRIDE: usize = 4096;
pub const COMPACT_PREFIX_BYTES: usize = 128;
pub const M45E3_WORD_BYTES: usize = 6;
pub const COMPACT_BYTES: usize = COMPACT_PREFIX_BYTES + M45E3_WORD_BYTES * BINS;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Native,
    Compact,
    Obn5(usize),
}

impl Format {
    pub fn from_config(config: u32) -> Result<Self, String> {
        match config {
            0 | 1 => Ok(Self::Native),
            2 | 3 => Ok(Self::Compact),
            _ => Err("OBN3 effective configuration".into()),
        }
    }

    pub fn active_bytes(self) -> usize {
        match self {
            Self::Native => BYTES,
            Self::Compact => COMPACT_BYTES,
            Self::Obn5(bins) => COMPACT_PREFIX_BYTES + M45E3_WORD_BYTES * bins,
        }
    }

    pub fn obn5_config(config: u32) -> Result<Self, String> {
        match config {
            0 | 1 => Ok(Self::Obn5(4096)),
            2 | 3 => Ok(Self::Obn5(2048)),
            4 | 5 => Ok(Self::Obn5(1024)),
            _ => Err("OBN5 FFT configuration".into()),
        }
    }

    pub fn bins(self) -> usize {
        match self {
            Self::Obn5(bins) => bins,
            _ => BINS,
        }
    }

    pub fn view_stride(self) -> usize {
        match self {
            Self::Obn5(_) => OBN5_VIEW_STRIDE,
            _ => super::view::VIEW_BYTES,
        }
    }

    pub fn prefix_bytes(self) -> usize {
        match self {
            Self::Native => 512,
            Self::Compact | Self::Obn5(_) => COMPACT_PREFIX_BYTES,
        }
    }

    pub fn header(self, slot: &[u8]) -> Result<Header, String> {
        let prefix = slot.get(..self.prefix_bytes()).ok_or("OBN3 header truncated")?;
        let mut header = [0; 64];
        match self {
            Self::Native => {
                for (dst, word) in header.iter_mut().zip(prefix.chunks_exact(8)) {
                    *dst = word[7];
                }
            }
            Self::Compact | Self::Obn5(_) => {
                header.copy_from_slice(&prefix[..64]);
                if prefix[64..].iter().any(|&byte| byte != 0) {
                    return Err("compact reserved padding".into());
                }
            }
        }
        let u32_at = |i| u32::from_le_bytes(header[i..i + 4].try_into().unwrap());
        let u64_at = |i| u64::from_le_bytes(header[i..i + 8].try_into().unwrap());
        let expected_format = match self {
            Self::Native => 2,
            Self::Compact | Self::Obn5(_) => 3,
        };
        let (id, revision, allowed_flags) = match self {
            Self::Obn5(_) => (OBN5_ID, 1, 0xff),
            _ => (ID, 4, 0x3f),
        };
        if u32_at(0) != id || u32_at(4) != ((64 << 16) | revision)
            || u32_at(32) != self.bins() as u32 || u32_at(40) != TICK_HZ as u32
            || u32_at(48) != expected_format || u32_at(52) != self.active_bytes() as u32
        {
            return Err("spectrum ID/revision/format/active length".into());
        }
        let k = u32_at(36);
        let flags = u32_at(44);
        if !(1..=1023).contains(&k) || flags & !allowed_flags != 0 || flags & 1 == 0
            || flags & 0x22 == 0x22
        {
            return Err("spectrum actualK/source/flags".into());
        }
        let first_seq = u64_at(8);
        Ok(Header {
            first_seq,
            // OBN5 groups can span missing IQ windows. Only first_seq is known.
            last_seq: if matches!(self, Self::Obn5(_)) { first_seq }
                else { first_seq.checked_add(u64::from(k) - 1).ok_or("OBN3 sequence overflow")? },
            epoch: u64_at(16),
            first_tick: u64_at(24),
            bins: self.bins(),
            k,
            flags,
            ordinal: u64_at(56),
        })
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_compact_headers() {
        let mut data=[0;128];
        for (offset,value) in [(0,ID),(4,(64<<16)|4),(32,BINS as u32),(36,2),
            (40,TICK_HZ as u32),(44,1),(48,3),(52,COMPACT_BYTES as u32)] {
            data[offset..offset+4].copy_from_slice(&value.to_le_bytes());
        }
        assert_eq!(Format::Compact.header(&data).unwrap().k,2);
        for (offset,value) in [(36,0),(36,1024),(44,0),(44,0x23),(44,0x41),(52,32768)] {
            let mut bad=data;
            bad[offset..offset+4].copy_from_slice(&u32::to_le_bytes(value));
            assert!(Format::Compact.header(&bad).is_err());
        }
        data[64]=1;
        assert!(Format::Compact.header(&data).is_err());
    }

    #[test]
    fn obn5_geometry_and_split_view_mode_bits() {
        for (config,bins) in [(0,4096),(2,2048),(4,1024)] {
            let format=Format::obn5_config(config).unwrap();
            let mut data=[0;COMPACT_PREFIX_BYTES];
            for (offset,value) in [(0,OBN5_ID),(4,(64<<16)|1),(32,bins),(36,1023),
                (40,TICK_HZ as u32),(48,3),(52,128+6*bins)] {
                data[offset..offset+4].copy_from_slice(&value.to_le_bytes());
            }
            for (mode,divisor) in [(0,0),(1,8),(2,4),(3,2),(4,1),(5,0),(6,0),(7,0)] {
                let flags=1u32|64|((mode&3)<<3)|((mode&4)<<5);
                data[44..48].copy_from_slice(&flags.to_le_bytes());
                let h=format.header(&data).unwrap();
                assert_eq!(h.bins,bins as usize);
                assert_eq!(h.k,1023);
                assert_eq!(super::super::view::view_len(h),bins.checked_div(divisor).unwrap_or(0) as usize);
            }
            assert!(Format::Compact.header(&data).is_err());
        }
        assert!(Format::obn5_config(6).is_err());
    }
}
