//! Camera streams without I/O (ADR-0017 V1, V2, S38): ported from the
//! WebCodecs experiment, which played a camera whose stream breaks strict
//! demuxers. The daemon receives and relays; the browser decodes. Nothing
//! here decodes or encodes a picture.

#![forbid(unsafe_code)]

pub mod aac;
pub mod au;
pub mod audio;
pub mod bits;
pub mod klv;
pub mod mp4;
pub mod nal;
pub mod opus;
pub mod photo;
pub mod record;
pub mod replay;
pub mod rtcp;
pub mod rtp;
pub mod rtp_audio;
pub mod rtp_klv;
pub mod source;
pub mod ts;
pub mod tsmux;
pub mod tsrecord;
pub mod udp;
pub mod wire;
