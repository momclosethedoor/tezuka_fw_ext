//! maia-httpd is part of Maia SDR. It is a web server that controls the Maia
//! SDR FPGA IP core and the AD9361 RFIC. It provides a RESTful API for these
//! elements, and serves the web application `maia-wasm`, which functions as the
//! UI of Maia SDR. Waterfall data is streamed to clients in real time using
//! WebSockets.

#![warn(missing_docs)]

pub mod app;
pub mod continuous;
pub mod scan_parts;
pub mod continuous_runtime;
pub mod continuous_live;
pub mod view;
pub mod args;
pub mod ddc;
pub mod fpga;
pub mod httpd;
pub mod iio;
pub mod observation;
pub mod observation_live;
pub mod native_observation;
pub mod native_runtime;
pub mod resident_schedule;
pub mod scan_frame;
pub mod gap_diagnostics;
pub mod scan_runtime;
pub mod observation_transport;
pub mod observation_accumulator;
pub mod rxbuffer;
pub mod sigmf;
pub mod spectrometer;
pub mod uio;

pub mod system_reader;
pub mod system_runtime;

pub mod delivery_runtime;
pub mod field1_control;
