pub mod capabilities;
pub mod quality;
pub mod rtsp;
pub mod session;
pub mod source;
pub mod video;

/// WFD Device Information subelement: source, available, RTSP 7236, 50 Mbps.
pub const SOURCE_WFD_IES: [u8; 9] = [0, 0, 6, 0, 0x10, 0x1c, 0x44, 0, 50];
