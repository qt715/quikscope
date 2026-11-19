/*
 * Copyright (c) 2023.
 *
 * This software is free software;
 *
 * You can redistribute it or modify it under terms of the MIT, Apache License or Zlib license
 */

//! Zune-jpegxl, vendored into quikscope with animation support (see `NOTICE`
//! next to this file's directory).
//!
//! A small lossless modular JPEG XL encoder ported from libjxl's
//! fast-lossless encoder: up to 16 bits of depth, up to 4 channels, no
//! palette.
//!
//! 16-bit data is passed as two native-endian bytes per sample.
#![forbid(unsafe_code)]

pub use encoder::{JxlAnimation, JxlFrame, JxlSimpleEncoder};
pub use errors::JxlEncodeErrors;

mod bit_depth;
mod bit_writer;
mod color_convert;
mod encoder;
mod errors;
