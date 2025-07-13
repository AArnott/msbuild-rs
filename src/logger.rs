use anyhow::Result;
use env_logger::Builder;
use log::LevelFilter;

pub fn setup_logging(verbose: bool) -> Result<()> {
    let mut builder = Builder::from_default_env();

    if verbose {
        builder.filter_level(LevelFilter::Debug);
    } else {
        builder.filter_level(LevelFilter::Info);
    }

    builder.init();
    Ok(())
}
