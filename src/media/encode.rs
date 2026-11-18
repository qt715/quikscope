//! Encoder selection and the libavcodec/libavformat/libavfilter pipeline
//! shared by recording sessions and exports.

/// Sends FFmpeg's log through the `log` crate (warnings and errors only,
/// unless `QUIKSCOPE_LOG` enables debug output for `quikscope::media`).
pub(crate) fn route_logs() {
    todo!("MediaCore")
}
