//! Real8 mode selection and the DDC /2 profile used by manual video.
use super::{FS, Registers};
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::sync::OnceLock;

const DDC4_CONTROL: u32 = 29_360_130;
// FIR1 BRAM values from the OBN5 board-tested DDC4 setup. The second half
// starts with the reverse of the first, with unused words set to 1.
const DDC4_WORDS: [u32; 12] = [
    11289, 262143, 512553, 23825, 206113, 480917, 37433, 125669, 475469, 518359, 33933, 497267,
];

pub(super) struct Profile {
    pub division: u32,
    pub control: u32,
    pub code: u8,
    pub rate: u64,
}

impl Profile {
    pub fn parse(settings: &Value, obn5: bool) -> Result<Self> {
        let choice = |key| settings.get(key).and_then(Value::as_str);
        let division = match settings.get("ddc_decimation").map(Value::as_u64) {
            None | Some(Some(4)) => 4,
            Some(Some(2)) => 2,
            _ => anyhow::bail!("ddc_decimation must be 2 or 4"),
        };
        let (filter, rate) = match choice("profile").unwrap_or(if obn5 { "color" } else { "full" }) {
            "full" => (8, 15_360_000),
            "color" => (12, 11_520_000),
            "luma" => (4, 7_680_000),
            _ => anyhow::bail!("unknown video profile"),
        };
        let standard = match choice("standard").unwrap_or("pal") {
            "pal" => 0,
            "ntsc" => 2,
            _ => anyhow::bail!("unknown video standard"),
        };
        let polarity = match choice("sync").unwrap_or("auto") {
            "auto" => 0,
            "negative" => 1 << 5,
            "positive" => 2 << 5,
            _ => anyhow::bail!("unknown sync polarity"),
        };
        let marks = match settings.get("marks").map(Value::as_bool) {
            None | Some(Some(true)) => 16,
            Some(Some(false)) => 0,
            _ => anyhow::bail!("marks must be boolean"),
        };
        for key in ["profile", "standard", "sync"] {
            ensure!(settings.get(key).is_none() || choice(key).is_some(), "video {key} must be a string");
        }
        ensure!(obn5 || ["profile", "standard", "sync", "marks", "ddc_decimation"].iter()
            .all(|key| settings.get(*key).is_none()), "video profiles require OBN5");
        let control = filter | standard | marks | polarity | if division == 2 { 1 << 8 } else { 0 };
        let code = ((control & 31) | ((control >> 3) & 32)) as u8;
        Ok(Self { division, control, code, rate })
    }

    pub fn actual(&self) -> Value {
        json!({"ddc_decimation":self.division,"ddc_input_fs_hz":FS/u64::from(self.division),
            "real8_fs_hz":self.rate,"code":self.code,"gain_code":0,"video_control":self.control,
            "marks":self.code&16!=0,"standard":if self.code&2==0{"pal"}else{"ntsc"},
            "profile":match self.rate{11_520_000=>"color",7_680_000=>"luma",_=>"full"},
            "sync":match self.control&0x60{0x20=>"negative",0x40=>"positive",_=>"auto"}})
    }
}

struct Fir2Input {
    words: [u32; 256],
    control: u32,
}

fn design_ddc2() -> Result<Fir2Input> {
    let config = crate::ddc::make_design(&maia_json::PutDDCDesign {
        frequency: 0.0, decimation: 2, transition_bandwidth: Some(0.35),
        stopband_attenuation_db: Some(30.0), passband_ripple: None, stopband_one_over_f: None,
    }, FS as f64)?;
    ensure!(config.fir1.decimation == 2 && config.fir2.is_none() && config.fir3.is_none(), "DDC2 FIR layout");
    let taps = &config.fir1.coefficients;
    let half_ops = taps.len().div_ceil(2);
    let ops = half_ops.div_ceil(2);
    ensure!(ops > 0 && ops as f64 * FS as f64 <= crate::ddc::constants::CLOCK_FREQUENCY,
        "DDC2 FIR exceeds available operations");
    let mut words = [1; 256];
    for (address, word) in words.iter_mut().enumerate() {
        let bank = address / 128;
        let slot = address % 128;
        let phase = slot / ops;
        if phase < 2 {
            let tap = (2 * (slot % ops) + bank) * 2 + (1 - phase);
            let coefficient = taps.get(tap).copied().unwrap_or(0) as u32;
            *word = ((coefficient & 0x3ffff) << 1) | 1;
        }
    }
    let control = (DDC4_CONTROL & !((1 << 20) | 0x7f))
        | ((half_ops as u32 & 1) << 20) | (ops as u32 - 1);
    Ok(Fir2Input { words, control })
}

pub(super) fn select_ddc2(regs: &Registers) -> Result<()> {
    static FIR: OnceLock<Result<Fir2Input, String>> = OnceLock::new();
    ensure!(regs.read(0x2c) == 4 && regs.read(0x34) == DDC4_CONTROL, "video DDC2 requires the guarded DDC4 baseline");
    ensure!(regs.read(0x4c) >> 24 >= 5, "video DDC2 requires PL video v5");
    let fir = FIR.get_or_init(|| design_ddc2().map_err(|err| err.to_string()))
        .as_ref().map_err(|err| anyhow::anyhow!("{err}"))?;
    program(regs, 2, fir.control, fir.words.into_iter());
    if regs.read(0x2c) != 2 || regs.read(0x34) != fir.control {
        restore_ddc4(regs);
        anyhow::bail!("video DDC2 readback mismatch");
    }
    Ok(())
}

pub(super) fn restore_ddc4(regs: &Registers) {
    let first = DDC4_WORDS.into_iter().chain(std::iter::repeat_n(1, 116));
    let second = DDC4_WORDS.into_iter().rev().chain(std::iter::repeat_n(1, 116));
    program(regs, 4, DDC4_CONTROL, first.chain(second));
}

fn program(regs: &Registers, division: u32, control: u32, words: impl Iterator<Item=u32>) {
    regs.write(0x34, 0);
    for (addr, word) in words.enumerate() {
        regs.write(0x24, addr as u32);
        regs.write(0x28, word);
    }
    regs.write(0x2c, division);
    regs.write(0x34, control);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packed_video_code_carries_input_rate_and_output_profile() {
        let p = Profile::parse(&json!({"ddc_decimation":2,"standard":"ntsc","sync":"positive"}), true).unwrap();
        assert_eq!((p.division,p.control,p.code,p.rate), (2,0x15e,0x3e,11_520_000));
        assert_eq!(Profile::parse(&json!({}), true).unwrap().code, 28);
        assert_eq!(Profile::parse(&json!({}), false).unwrap().code, 24);
    }
}
