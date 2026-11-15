use super::json_error::JsonError;
use crate::{app::AppState, ddc};
use anyhow::Result;
use axum::{Json, extract::State};
use maia_json::{DDCConfig, DDCConfigSummary, PatchDDCConfig, PutDDCConfig, PutDDCDesign};

/// Route the on-FPGA DDC output into the cf-ad9361-lpc capture device, or restore
/// full-rate wideband IQ, by writing the axi_ad9361 routing register 0x790200BC.
///
/// This register lives in the cf-ad9361-lpc core (not the maia-sdr UIO region), so
/// it is written through a one-shot /dev/mem mapping. The boot/default state is
/// disabled, so ordinary libiio clients (SDR++/SDR#/GNU Radio) stream full-rate IQ
/// unless a client (e.g. the OpenRF overlay) explicitly enables routing for its
/// session and disables it again when done.
fn set_ddc_routing(enable: bool) -> Result<()> {
    use std::os::unix::io::AsRawFd;
    const REG_BASE: libc::off_t = 0x7902_0000;
    const REG_OFFSET: usize = 0xBC;
    const MAP_LEN: usize = 0x1000;
    let mem = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/mem")?;
    // SAFETY: map one page at the page-aligned core base and write the 32-bit
    // routing register at offset 0xBC with volatile semantics, then unmap.
    unsafe {
        let map = libc::mmap(
            std::ptr::null_mut(),
            MAP_LEN,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            mem.as_raw_fd(),
            REG_BASE,
        );
        if map == libc::MAP_FAILED {
            anyhow::bail!("mmap /dev/mem for DDC routing register failed");
        }
        let reg = (map as *mut u8).add(REG_OFFSET) as *mut u32;
        std::ptr::write_volatile(reg, u32::from(enable));
        libc::munmap(map, MAP_LEN);
    }
    Ok(())
}

async fn ddc_config(state: &AppState) -> Result<Json<DDCConfig>, JsonError> {
    let samp_rate = state
        .ad9361_samp_rate()
        .await
        .map_err(JsonError::server_error)?;
    Ok(Json(state.ip_core().lock().unwrap().ddc_config(samp_rate)))
}

pub async fn ddc_json(state: &AppState) -> Result<DDCConfigSummary> {
    let samp_rate = state.ad9361_samp_rate().await?;
    Ok(state
        .ip_core()
        .lock()
        .unwrap()
        .ddc_config_summary(samp_rate))
}

pub async fn get_ddc_config(State(state): State<AppState>) -> Result<Json<DDCConfig>, JsonError> {
    ddc_config(&state).await
}

async fn set_ddc_config(state: &AppState, config: PutDDCConfig) -> Result<(), JsonError> {
    let samp_rate = state
        .ad9361_samp_rate()
        .await
        .map_err(JsonError::server_error)?;
    state
        .ip_core()
        .lock()
        .unwrap()
        .set_ddc_config(&config, samp_rate)
        .map_err(JsonError::client_error_alert)
}

pub async fn put_ddc_config(
    State(state): State<AppState>,
    Json(put): Json<PutDDCConfig>,
) -> Result<Json<DDCConfig>, JsonError> {
    set_ddc_config(&state, put).await?;
    ddc_config(&state).await
}

pub async fn patch_ddc_config(
    State(state): State<AppState>,
    Json(patch): Json<PatchDDCConfig>,
) -> Result<Json<DDCConfig>, JsonError> {
    if let Some(frequency) = patch.frequency {
        let samp_rate = state
            .ad9361_samp_rate()
            .await
            .map_err(JsonError::server_error)?;
        state
            .ip_core()
            .lock()
            .unwrap()
            .set_ddc_frequency(frequency, samp_rate)
            .map_err(JsonError::client_error_alert)?;
    }
    if let Some(enabled) = patch.enabled {
        if state.continuous_observations(){state.ip_core().lock().unwrap().set_ddc_enable(enabled);}
        set_ddc_routing(enabled).map_err(JsonError::server_error)?;
    }
    ddc_config(&state).await
}

async fn set_ddc_design(state: &AppState, design: PutDDCDesign) -> Result<(), JsonError> {
    let samp_rate = state
        .ad9361_samp_rate()
        .await
        .map_err(JsonError::server_error)?;
    // The DDC design can take a couple seconds to calculate, so it is run in a
    // blocking thread.
    let config = tokio::task::spawn_blocking(move || ddc::make_design(&design, samp_rate))
        .await
        .map_err(JsonError::server_error)?
        .map_err(JsonError::client_error_alert)?;
    state
        .ip_core()
        .lock()
        .unwrap()
        .set_ddc_config(&config, samp_rate)
        // If the design was successful, this call should succeed. If it doesn't
        // it's a server error, not a client parameters error.
        .map_err(JsonError::server_error)
}

pub async fn put_ddc_design(
    State(state): State<AppState>,
    Json(put): Json<PutDDCDesign>,
) -> Result<Json<DDCConfig>, JsonError> {
    set_ddc_design(&state, put).await?;
    ddc_config(&state).await
}
