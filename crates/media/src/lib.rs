pub mod encoder;
pub mod pipeline;
mod quality;
pub use quality::PreparedFormat;
pub mod streaming;

pub use encoder::*;
pub use pipeline::*;
pub use streaming::*;

pub fn initialize() -> Result<String, MediaError> {
    gst::init()?;
    Ok(gst::version_string().to_string())
}
