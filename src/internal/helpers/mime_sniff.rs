// MIME sniffing ported from Go 1.26.5 net/http/sniff.go.
// Copyright 2011 The Go Authors. All rights reserved.
// SPDX-License-Identifier: BSD-3-Clause
//
// Copyright 2009 The Go Authors.
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions are
// met:
//
//    * Redistributions of source code must retain the above copyright
// notice, this list of conditions and the following disclaimer.
//    * Redistributions in binary form must reproduce the above
// copyright notice, this list of conditions and the following disclaimer
// in the documentation and/or other materials provided with the
// distribution.
//    * Neither the name of Google LLC nor the names of its
// contributors may be used to endorse or promote products derived from
// this software without specific prior written permission.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
// "AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
// LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
// A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT
// OWNER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
// SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT
// LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
// DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
// THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
// (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
// OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

// Keep the signature order and byte rules in sync with Go's DetectContentType.
// MIME sniffing deliberately accepts invalid UTF-8; URL encoding validates it
// independently when deciding between percent encoding and base64.

const HTML: &[&[u8]] = &[
    b"<!DOCTYPE HTML",
    b"<HTML",
    b"<HEAD",
    b"<SCRIPT",
    b"<IFRAME",
    b"<H1",
    b"<DIV",
    b"<FONT",
    b"<TABLE",
    b"<A",
    b"<STYLE",
    b"<TITLE",
    b"<B",
    b"<BODY",
    b"<BR",
    b"<P",
    b"<!--",
];

enum Signature {
    Exact(&'static [u8], &'static str),
    Masked(&'static [u8], &'static [u8], &'static str),
    Mp4,
}

use Signature::{Exact, Masked, Mp4};

const SIGNATURES: &[Signature] = &[
    Exact(b"%PDF-", "application/pdf"),
    Exact(b"%!PS-Adobe-", "application/postscript"),
    Masked(
        b"\xff\xff\0\0",
        b"\xfe\xff\0\0",
        "text/plain; charset=utf-16be",
    ),
    Masked(
        b"\xff\xff\0\0",
        b"\xff\xfe\0\0",
        "text/plain; charset=utf-16le",
    ),
    Masked(
        b"\xff\xff\xff\0",
        b"\xef\xbb\xbf\0",
        "text/plain; charset=utf-8",
    ),
    Exact(b"\0\0\x01\0", "image/x-icon"),
    Exact(b"\0\0\x02\0", "image/x-icon"),
    Exact(b"BM", "image/bmp"),
    Exact(b"GIF87a", "image/gif"),
    Exact(b"GIF89a", "image/gif"),
    Masked(
        b"\xff\xff\xff\xff\0\0\0\0\xff\xff\xff\xff\xff\xff",
        b"RIFF\0\0\0\0WEBPVP",
        "image/webp",
    ),
    Exact(b"\x89PNG\r\n\x1a\n", "image/png"),
    Exact(b"\xff\xd8\xff", "image/jpeg"),
    Masked(
        b"\xff\xff\xff\xff\0\0\0\0\xff\xff\xff\xff",
        b"FORM\0\0\0\0AIFF",
        "audio/aiff",
    ),
    Exact(b"ID3", "audio/mpeg"),
    Exact(b"OggS\0", "application/ogg"),
    Exact(b"MThd\0\0\0\x06", "audio/midi"),
    Masked(
        b"\xff\xff\xff\xff\0\0\0\0\xff\xff\xff\xff",
        b"RIFF\0\0\0\0AVI ",
        "video/avi",
    ),
    Masked(
        b"\xff\xff\xff\xff\0\0\0\0\xff\xff\xff\xff",
        b"RIFF\0\0\0\0WAVE",
        "audio/wave",
    ),
    Mp4,
    Exact(b"\x1a\x45\xdf\xa3", "video/webm"),
    Masked(
        b"\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\xff\xff",
        b"\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0LP",
        "application/vnd.ms-fontobject",
    ),
    Exact(b"\0\x01\0\0", "font/ttf"),
    Exact(b"OTTO", "font/otf"),
    Exact(b"ttcf", "font/collection"),
    Exact(b"wOFF", "font/woff"),
    Exact(b"wOF2", "font/woff2"),
    Exact(b"\x1f\x8b\x08", "application/x-gzip"),
    Exact(b"PK\x03\x04", "application/zip"),
    Exact(b"Rar!\x1a\x07\0", "application/x-rar-compressed"),
    Exact(b"Rar!\x1a\x07\x01\0", "application/x-rar-compressed"),
    Exact(b"\0asm", "application/wasm"),
];

pub(super) fn detect_content_type(contents: &[u8]) -> &'static str {
    let data = &contents[..contents.len().min(512)];
    let first_non_ws = data
        .iter()
        .position(|byte| !matches!(byte, b'\t' | b'\n' | b'\x0c' | b'\r' | b' '))
        .unwrap_or(data.len());
    let trimmed = &data[first_non_ws..];
    for pattern in HTML {
        if trimmed.len() > pattern.len()
            && pattern.iter().zip(trimmed).all(|(pattern, byte)| {
                *pattern
                    == if pattern.is_ascii_uppercase() {
                        byte & 0xdf
                    } else {
                        *byte
                    }
            })
            && matches!(trimmed[pattern.len()], b' ' | b'>')
        {
            return "text/html; charset=utf-8";
        }
    }
    if trimmed.starts_with(b"<?xml") {
        return "text/xml; charset=utf-8";
    }
    for signature in SIGNATURES {
        match signature {
            Exact(pattern, mime) if data.starts_with(pattern) => return mime,
            Masked(mask, pattern, mime)
                if data.len() >= pattern.len()
                    && mask.len() == pattern.len()
                    && data
                        .iter()
                        .zip(mask.iter().zip(*pattern))
                        .all(|(byte, (mask, pattern))| byte & mask == *pattern) =>
            {
                return mime;
            }
            Mp4 if matches_mp4(data) => return "video/mp4",
            _ => {}
        }
    }
    if trimmed
        .iter()
        .any(|byte| matches!(byte, 0x00..=0x08 | 0x0b | 0x0e..=0x1a | 0x1c..=0x1f))
    {
        "application/octet-stream"
    } else {
        "text/plain; charset=utf-8"
    }
}

fn matches_mp4(data: &[u8]) -> bool {
    if data.len() < 12 {
        return false;
    }
    let box_size = u32::from_be_bytes(data[..4].try_into().unwrap()) as usize;
    if box_size > data.len() || !box_size.is_multiple_of(4) || &data[4..8] != b"ftyp" {
        return false;
    }
    (8..box_size)
        .step_by(4)
        .any(|start| start != 12 && &data[start..start + 3] == b"mp4")
}
