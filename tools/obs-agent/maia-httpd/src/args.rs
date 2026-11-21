//! maia-httpd CLI arguments.
//!
//! This module contains the definition of the CLI arguments for the maia-httpd
//! application.

use clap::Parser;
use std::{net::SocketAddr, path::PathBuf};

/// maia-httpd CLI arguments.
#[derive(Parser, Debug, Clone, Eq, PartialEq, Hash)]
#[clap(author, version, about, long_about = None)]
pub struct Args {
    /// Integrated OBS007A scanner/preview RAM acceptance runtime.
    #[clap(long,conflicts_with_all=["continuous_observations","native_observations"])]
    pub system_observations: Option<PathBuf>,
    /// Interactive OBS row99 owner configuration.
    #[clap(long,conflicts_with_all=["system_observations","continuous_observations","native_observations","system_field_input"])]
    pub system_control: Option<PathBuf>,

    /// Exercise the real scan-bank encoder with a synthetic 8/9-TX fixture.
    #[clap(long, conflicts_with = "system_control")]
    pub check_scan_tx_bank: bool,
    /// Offline check of the in-process native field module, without hardware.
    #[clap(long,requires="system_field_output",conflicts_with="system_observations")]
    pub system_field_input: Option<PathBuf>,
    /// Output directory for an offline owned field.
    #[clap(long)]
    pub system_field_output: Option<PathBuf>,
    /// Static OBN2 r2 continuous source. Requires the matching PL, no fallback.
    #[clap(long,conflicts_with_all=["native_observations","check_native_observations"])]
    pub continuous_observations: Option<PathBuf>,
    /// Validate OBN2 static configuration offline, without opening hardware.
    #[clap(long,conflicts_with_all=["continuous_observations","native_observations","check_native_observations"])]
    pub check_continuous_observations: Option<PathBuf>,
    /// Decode and normalize an OBN2 golden record offline, no hardware access.
    #[clap(long,conflicts_with_all=["continuous_observations","native_observations"])]
    pub check_obn2_record: Option<PathBuf>,
    /// Experimental static raw OBN1 source config; requires a coordinated A/1R1T RAM boot.
    #[clap(long)]
    pub native_observations: Option<PathBuf>,
    /// Validate native config offline, without opening FPGA/IIO or starting capture.
    #[clap(long, conflicts_with = "native_observations")]
    pub check_native_observations: Option<PathBuf>,
    /// Listen address for the HTTP server
    #[clap(long, default_value = "0.0.0.0:8000")]
    pub listen: SocketAddr,
    /// Listen address for the HTTPS server
    #[clap(long, default_value = "0.0.0.0:443")]
    pub listen_https: SocketAddr,
    /// Path to SSL certificate for HTTPS server
    ///
    /// Unless both the SSL certificate and key are specified, the HTTPS server
    /// is disabled.
    #[clap(long)]
    pub ssl_cert: Option<PathBuf>,
    /// Path to SSL key for HTTPS server
    ///
    /// Unless both the SSL certificate and key are specified, the HTTPS server
    /// is disabled.
    #[clap(long)]
    pub ssl_key: Option<PathBuf>,
    /// Path to CA certificate for HTTPS server
    ///
    /// The CA certificate is accessible on /ca.crt of the web API if this
    /// option is provided.
    #[clap(long)]
    pub ca_cert: Option<PathBuf>,
}

#[cfg(feature = "uclibc")]
impl Default for Args {
    fn default() -> Args {
        Args {
            system_observations: None,
            system_control: None,
            check_scan_tx_bank: false,
            system_field_input: None,
            system_field_output: None,
            continuous_observations: None,
            check_continuous_observations: None,
            check_obn2_record: None,
            native_observations: None,
            check_native_observations: None,
            listen: "0.0.0.0:8000".parse().unwrap(),
            listen_https: "0.0.0.0:443".parse().unwrap(),
            ssl_cert: None,
            ssl_key: None,
            ca_cert: None,
        }
    }
}
