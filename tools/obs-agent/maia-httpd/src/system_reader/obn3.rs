//! OBN3 revision 4 wire decoding. The full allocation stride remains 32768.
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

    pub fn decode(self, slot: &[u8], header: Header, power: &mut [f32]) -> Result<(), String> {
        if slot.len() != self.active_bytes() || power.len() != BINS {
            return Err("OBN3 decode length".into());
        }
        let count = if header.peak() { 1 } else { u64::from(header.k) };
        let scale = 1.0 / (750668.0_f64 * 750668.0 * count as f64);
        let mut factors = [0.0; 5];
        for (exponent, factor) in factors.iter_mut().enumerate() {
            *factor = scale * (1u32 << (2 * exponent)) as f64;
        }
        match self {
            Self::Compact => decode_bins::<6,13>(&slot[128..],power,(count as u32)<<3,&factors),
            Self::Native => decode_bins::<8,15>(slot,power,(count as u32)<<3,&factors),
        }
    }
}

fn decode_bins<const WIDTH: usize, const MANTISSA_HIGH_BITS: u32>(
    bytes: &[u8], power: &mut [f32], maximum_high: u32, factors: &[f64;5],
) -> Result<(), String> {
    for (index, (bin, word)) in power.iter_mut().zip(bytes.chunks_exact(WIDTH)).enumerate() {
        let low=u32::from_le_bytes(word[..4].try_into().unwrap());
        let high=if WIDTH==6 {u32::from(u16::from_le_bytes(word[4..6].try_into().unwrap()))}
            else {u32::from_le_bytes(word[4..8].try_into().unwrap())};
        if WIDTH==8 && (high & 0x00fc0000 != 0 || index >= 64 && high >> 24 != 0) {
            return Err("OBN3 native reserved bin bits".into());
        }
        let exponent=((high>>MANTISSA_HIGH_BITS)&7) as usize;
        let mantissa_high=high&((1<<MANTISSA_HIGH_BITS)-1);
        if mantissa_high>maximum_high || mantissa_high==maximum_high && low!=0 {
            return Err("OBN3 bin exceeds accumulator bound".into());
        }
        let factor=factors.get(exponent).ok_or("OBN3 invalid bin exponent")?;
        // Both limbs and their sum are exact in f64. A9 converts u32 directly.
        *bin=((f64::from(mantissa_high)*4_294_967_296.0+f64::from(low))*factor) as f32;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(format: Format, k: u32, flags: u32, word: u64) -> Vec<u8> {
        let mut h = [0; 64];
        let format_code = if format == Format::Compact { 3u32 } else { 2u32 };
        for (offset, value) in [
            (0, ID), (4, (64 << 16) | 4), (8, 9), (16, 22), (24, 42),
            (32, BINS as u32), (36, k), (40, TICK_HZ as u32), (44, flags),
            (48, format_code), (52, format.active_bytes() as u32), (56, 1024),
        ] {
            h[offset..offset+4].copy_from_slice(&value.to_le_bytes());
        }
        let mut bytes = vec![0; format.active_bytes()];
        match format {
            Format::Compact => {
                bytes[..64].copy_from_slice(&h);
                for bin in bytes[128..].chunks_exact_mut(6) {
                    bin.copy_from_slice(&word.to_le_bytes()[..6]);
                }
            }
            Format::Native => {
                for (index, bin) in bytes.chunks_exact_mut(8).enumerate() {
                    bin.copy_from_slice(&word.to_le_bytes());
                    bin[7] = h.get(index).copied().unwrap_or(0);
                }
            }
        }
        bytes
    }

    #[test]
    fn both_encodings_produce_identical_sum_and_peak_power_at_the_k_bounds() {
        for k in [1, 2, 700, 1023] {
            for peak in [false, true] {
                let count = if peak { 1 } else { k };
                let mantissa = u64::from(count) << 35;
                let mut results = Vec::new();
                for (format, shift) in [(Format::Compact, 45), (Format::Native, 47)] {
                    let data = slot(format, k, 1 | (u32::from(peak) << 2), mantissa | (4 << shift));
                    let header = format.header(&data).unwrap();
                    assert_eq!((header.epoch, header.ordinal, header.last_seq), (22, 1024, 8 + u64::from(k)));
                    let mut power = vec![0.0; BINS];
                    format.decode(&data, header, &mut power).unwrap();
                    results.push(power);
                }
                assert_eq!(results[0], results[1]);
                let expected = ((1u64 << 43) as f64 / 750668.0_f64.powi(2)) as f32;
                assert!(results[0].iter().all(|&value| value == expected));
            }
        }
    }

    #[test]
    fn rejects_invalid_headers_and_power_words() {
        let format = Format::Compact;
        for (k, flags) in [(0, 1), (1024, 1), (2, 0), (2, 0x23), (2, 0x41)] {
            assert!(format.header(&slot(format, k, flags, 0)).is_err());
        }
        let mut data = slot(format, 2, 1, 0);
        data[64] = 1;
        assert!(format.header(&data).is_err());
        data[64] = 0;
        data[52] ^= 1;
        assert!(format.header(&data).is_err());
        for word in [5 << 45, (2 << 35) + 1] {
            let data = slot(format, 2, 1, word);
            let header = format.header(&data).unwrap();
            assert!(format.decode(&data, header, &mut [0.0; BINS]).is_err());
        }
        let data = slot(format, 2, 1, 0);
        let header = format.header(&data).unwrap();
        assert!(format.decode(&data[..data.len()-1], header, &mut [0.0; BINS]).is_err());
    }

    #[test]
    fn decodes_the_supplied_rtl_slot_when_available() {
        let Some(path) = std::env::var_os("OBS_OBN3_RTL_SLOT") else { return };
        let data = std::fs::read(path).unwrap();
        let format = Format::Compact;
        let header = format.header(&data).unwrap();
        let mut power = [0.0; BINS];
        format.decode(&data[..COMPACT_BYTES], header, &mut power).unwrap();
        assert!(power.iter().any(|&value| value > 0.0));
    }
}
