//! maia-httpd application.
//!
//! This module contains a top-level structure [`App`] that represents the whole
//! maia-httpd application and a structure [`AppState`] that contains the
//! application state.

use crate::{
    args::Args,
    fpga::{InterruptHandler, IpCore},
    httpd::{self, RecorderFinishWaiter, RecorderState},
    iio::Ad9361,
    spectrometer::{Spectrometer, SpectrometerConfig},
};
use anyhow::Result;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

/// maia-httpd application.
///
/// This struct represents the maia-sdr application. It owns the different
/// objects of which the application is formed, and runs them concurrently.
#[derive(Debug)]
pub struct App {
    httpd: httpd::Server,
    interrupt_handler: InterruptHandler,
    recorder_finish: RecorderFinishWaiter,
    spectrometer: Option<Spectrometer>,
    native: Option<crate::native_runtime::Runtime>,
    continuous: Option<crate::continuous_live::Runtime>,
}

impl App {
    /// Creates a new application.
    #[tracing::instrument(name = "App::new", level = "debug")]
    pub async fn new(args: &Args) -> Result<App> {
        // Initialize and build application state
        let native_config=args.native_observations.as_deref().map(crate::native_runtime::Config::load).transpose()?;
        let continuous_config=args.continuous_observations.as_deref().map(crate::continuous_live::Config::load).transpose()?;
        let continuous_guard=continuous_config.as_ref().map(crate::continuous_live::Guard::acquire).transpose()?;
        let native_guard=native_config.as_ref().map(crate::native_runtime::Guard::acquire).transpose()?;
        let (ip_core, interrupt_handler) = if continuous_config.is_some(){IpCore::take_continuous().await?}else if native_config.is_some(){IpCore::take_native().await?}else{IpCore::take().await?};
        let ip_core = std::sync::Mutex::new(ip_core);
        let ad9361 = tokio::sync::Mutex::new(Ad9361::new().await?);
        let recorder = RecorderState::new(&ad9361, &ip_core).await?;
        let state = AppState(Arc::new(State {
            continuous_observations: continuous_config.is_some(),
            parts: Mutex::new(None),
            native_observations: native_config.is_some(),
            scan: Default::default(),
            ad9361,
            ip_core,
            geolocation: std::sync::Mutex::new(None),
            recorder,
            spectrometer_config: Default::default(),
        }));
        // Initialize spectrometer sample rate and mode
        state.spectrometer_config().set_samp_rate_mode(
            state.ad9361().lock().await.get_sampling_frequency().await? as f32,
            state.ip_core().lock().unwrap().spectrometer_mode(),
        );

        // Build application objects

        let (waterfall_sender, _) = broadcast::channel(16);
        let spectrometer = if native_config.is_none()&&continuous_config.is_none(){Some(Spectrometer::new(
            state.clone(),
            interrupt_handler.waiter_spectrometer(),
            waterfall_sender.clone(),
        ))}else{None};

        let native=if let Some(config)=native_config{Some(crate::native_runtime::Runtime::new(state.clone(),config,native_guard.unwrap()).await?)}else{None};
        let continuous=if let Some(config)=continuous_config{Some(crate::continuous_live::Runtime::new(state.clone(),config,continuous_guard.unwrap()).await?)}else{None};

        let recorder_finish =
            RecorderFinishWaiter::new(state.clone(), interrupt_handler.waiter_recorder());

        let httpd = httpd::Server::new(
            args.listen,
            args.listen_https,
            args.ssl_cert.as_ref(),
            args.ssl_key.as_ref(),
            args.ca_cert.as_ref(),
            state,
            waterfall_sender,
        )
        .await?;

        Ok(App {
            continuous,
            httpd,
            interrupt_handler,
            recorder_finish,
            spectrometer,
            native,
        })
    }

    /// Runs the application.
    ///
    /// This only returns if one of the objects that form the application fails.
    #[tracing::instrument(name = "App::run", level = "debug", skip_all)]
    pub async fn run(self) -> Result<()> {
        if let Some(continuous)=self.continuous{
            return tokio::select!{ret=self.httpd.run()=>ret,ret=self.interrupt_handler.run_polling()=>ret,ret=self.recorder_finish.run()=>ret,ret=continuous.run()=>ret};
        }
        if let Some(native)=self.native{
            // Native uses completion counters; neither legacy IRQ blocking read
            // nor recorder-finish waiter is needed (mutating native HTTP is locked).
            return tokio::select!{ret=self.httpd.run()=>ret,ret=native.run()=>ret};
        }
        tokio::select! {
            ret = self.httpd.run() => ret,
            ret = self.interrupt_handler.run() => ret,
            ret = self.recorder_finish.run() => ret,
            ret = self.spectrometer.unwrap().run() => ret,
        }
    }
}

/// Application state.
///
/// This struct contains the application state that needs to be shared between
/// different modules, such as different Axum handlers in the HTTP server. The
/// struct behaves as an `Arc<...>`. It is cheaply clonable and clones represent
/// a reference to a shared object.
#[derive(Debug, Clone)]
pub struct AppState(Arc<State>);

#[derive(Debug)]
struct State {
    continuous_observations: bool,
    parts: Mutex<Option<Arc<Mutex<crate::continuous::PartsHub>>>>,
    scan: crate::scan_runtime::Hub,
    native_observations: bool,
    ad9361: tokio::sync::Mutex<Ad9361>,
    ip_core: Mutex<IpCore>,
    geolocation: Mutex<Option<maia_json::Geolocation>>,
    recorder: RecorderState,
    spectrometer_config: SpectrometerConfig,
}

impl AppState {
    /// The OBN2 static source has its own RF policy and recorder lifecycle.
    pub fn continuous_observations(&self)->bool{self.0.continuous_observations}
    /// Attach the continuous hub only after validating the hardware contract.
    pub fn set_parts(&self,hub:Arc<Mutex<crate::continuous::PartsHub>>){*self.0.parts.lock().unwrap()=Some(hub);}
    /// Continuous shared transport, independent of the legacy whole-frame hub.
    pub fn parts(&self)->Option<Arc<Mutex<crate::continuous::PartsHub>>>{self.0.parts.lock().unwrap().clone()}
    /// Bounded resident publication and command state in the existing application.
    pub fn scan(&self)->&crate::scan_runtime::Hub{&self.0.scan}
    /// The experimental native source freezes RF mutations and disables legacy waterfall.
    pub fn native_observations(&self)->bool{self.0.native_observations}
    /// Gives access to the [`Ad9361`] object of the application.
    pub fn ad9361(&self) -> &tokio::sync::Mutex<Ad9361> {
        &self.0.ad9361
    }

    /// Gives access to the [`IpCore`] object of the application.
    pub fn ip_core(&self) -> &Mutex<IpCore> {
        &self.0.ip_core
    }

    /// Gives access to the current geolocation of the device.
    ///
    /// The geolocation is `None` if it has never been set or if it has been
    /// cleared, or a valid [`Geolocation`](maia_json::Geolocation) otherwise.
    pub fn geolocation(&self) -> &Mutex<Option<maia_json::Geolocation>> {
        &self.0.geolocation
    }

    /// Gives access to the [`RecorderState`] object of the application.
    pub fn recorder(&self) -> &RecorderState {
        &self.0.recorder
    }

    /// Gives access to the [`SpectrometerConfig`] object of the application.
    pub fn spectrometer_config(&self) -> &SpectrometerConfig {
        &self.0.spectrometer_config
    }

    /// Returns the AD9361 sampling frequency.
    pub async fn ad9361_samp_rate(&self) -> Result<f64> {
        Ok(self.ad9361().lock().await.get_sampling_frequency().await? as f64)
    }
}
