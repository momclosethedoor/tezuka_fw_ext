use anyhow::Result;
#[cfg(not(feature = "uclibc"))]
use clap::Parser;
use maia_httpd::{app::App, args::Args};
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env())
        .init();

    // workaround for https://github.com/rust-lang/rust/issues/112488
    #[cfg(feature = "uclibc")]
    let args = Args::default();
    #[cfg(not(feature = "uclibc"))]
    let args = Args::parse();

    if let Some(path)=&args.check_continuous_observations{
        let config=maia_httpd::continuous_live::Config::load(path)?;
        println!("OBN2-r2 static config valid; no hardware access: {config:?}");return Ok(());
    }
    if let Some(path)=&args.check_obn2_record{
        let bytes=std::fs::read(path)?;let h=maia_httpd::continuous::header(&bytes).map_err(anyhow::Error::msg)?;let mut power=vec![0.;4096];
        maia_httpd::continuous::normalize(&bytes,h,1./750668f64.powi(2),&mut power).map_err(anyhow::Error::msg)?;
        let hash=power.iter().flat_map(|p|p.to_le_bytes()).fold(0xcbf29ce484222325u64,|hash,b|(hash^b as u64).wrapping_mul(0x100000001b3));
        println!("OBN2-r2 no hardware access bins=4096 actualK={} ordinal={} fnv64={hash:016x}",h.k,h.ordinal);return Ok(());
    }

    if let Some(path)=&args.check_native_observations{
        let config=maia_httpd::native_runtime::Config::load(path)?;
        println!("OBN1-r1 config syntax/profile valid; no hardware access or boot/RF assertion verified: {config:#?}");
        return Ok(());
    }
    if let Some(path)=&args.system_field_input { return maia_httpd::system_runtime::check_field(path,args.system_field_output.as_ref().unwrap()); }
    if let Some(path)=&args.system_control { return maia_httpd::control_runtime::run(path,args.listen).await; }
    if let Some(path)=&args.system_observations { return maia_httpd::delivery_runtime::run(path,args.listen).await; }
    App::new(&args).await?.run().await
}
