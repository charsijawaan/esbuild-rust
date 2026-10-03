//! The native stdin/stdout service and its wire protocol.
//!
//! Source: `cmd/esbuild/stdio_protocol.go`, `cmd/esbuild/service.go:84-164`,
//! and `lib/shared/stdio_protocol.ts` at the revision in `UPSTREAM.md`.
//! The initial version greeting is a plain length-prefixed frame; subsequent
//! frames contain packets. `decode_packet` accepts the frame's payload, while
//! `encode_packet` returns the entire frame, matching the upstream API.

pub mod dispatch;
mod messages;
pub mod options;
pub mod protocol;

pub use dispatch::{VERSION, run_service};

pub use protocol::{
    FrameDecoder, Object, Packet, ProtocolError, Value, decode_packet, encode_frame, encode_packet,
    read_length_prefixed_slice, read_uint32, write_uint32,
};
