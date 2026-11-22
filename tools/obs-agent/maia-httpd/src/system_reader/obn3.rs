//! OBN3 revision 4 record geometry and header validation.
use super::continuous::{BINS, BYTES, Header, TICK_HZ};

pub const ID: u32 = 0x334e424f;
pub const COMPACT_BYTES: usize = 128 + 6 * BINS;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Native,
    Compact,
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
        }
    }

    pub fn prefix_bytes(self) -> usize {
        match self {
            Self::Native => 512,
            Self::Compact => 128,
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
            Self::Compact => {
                header.copy_from_slice(&prefix[..64]);
                if prefix[64..].iter().any(|&byte| byte != 0) {
                    return Err("OBN3 compact reserved padding".into());
                }
            }
        }
        let u32_at = |i| u32::from_le_bytes(header[i..i + 4].try_into().unwrap());
        let u64_at = |i| u64::from_le_bytes(header[i..i + 8].try_into().unwrap());
        let expected_format = match self {
            Self::Native => 2,
            Self::Compact => 3,
        };
        if u32_at(0) != ID || u32_at(4) != ((64 << 16) | 4)
            || u32_at(32) != BINS as u32 || u32_at(40) != TICK_HZ as u32
            || u32_at(48) != expected_format || u32_at(52) != self.active_bytes() as u32
        {
            return Err("OBN3 ID/revision/format/active length".into());
        }
        let k = u32_at(36);
        let flags = u32_at(44);
        if !(1..=1023).contains(&k) || flags & !0x3f != 0 || flags & 1 == 0
            || flags & 0x22 == 0x22
        {
            return Err("OBN3 actualK/source/flags".into());
        }
        let first_seq = u64_at(8);
        Ok(Header {
            first_seq,
            last_seq: first_seq.checked_add(u64::from(k) - 1).ok_or("OBN3 sequence overflow")?,
            epoch: u64_at(16),
            first_tick: u64_at(24),
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
}
