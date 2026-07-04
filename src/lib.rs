use std::sync::Arc;

use compact_str::{CompactString, ToCompactString};

pub mod wayland;
pub mod x11;

// ============================================================================
// Shared State
// ============================================================================

#[derive(Debug, Clone)]
pub enum ClipboardContent {
    Text(CompactString),
    Data {
        mime_type: CompactString,
        bytes: Arc<[u8]>,
        hash: u64,
    },
    Empty,
}
impl PartialEq for ClipboardContent {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Text(a), Self::Text(b)) => a == b,
            (
                Self::Data {
                    mime_type: _,
                    bytes: _,
                    hash: a_hash,
                },
                Self::Data {
                    mime_type: _,
                    bytes: _,
                    hash: b_hash,
                },
            ) => a_hash == b_hash,
            (Self::Empty, Self::Empty) => true,
            _ => false,
        }
    }
}
impl ClipboardContent {
    pub fn len(&self) -> usize {
        match self {
            Self::Text(text) => text.len(),
            Self::Data { bytes, .. } => bytes.len(),
            Self::Empty => 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn primary_mime_type(&self) -> Option<&str> {
        match self {
            Self::Text(_) => Some(TEXT_PLAIN_UTF8_ATOM),
            Self::Data { mime_type, .. } => Some(mime_type),
            Self::Empty => None,
        }
    }

    pub fn bytes_for_mime(&self, mime_type: &str) -> Option<&[u8]> {
        match self {
            Self::Text(text) if is_text_mime_type(mime_type) => Some(text.as_bytes()),
            Self::Data {
                mime_type: own,
                bytes,
                hash,
            } if own == mime_type => Some(bytes),
            _ => None,
        }
    }

    pub fn offered_mime_types(&self) -> Vec<&str> {
        match self {
            Self::Text(_) => TEXT_MIME_TYPES.to_vec(),
            Self::Data { mime_type, .. } => vec![mime_type.as_str()],
            Self::Empty => Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardType {
    Clipboard,
    Primary,
}

#[derive(Debug)]
pub enum SyncEvent {
    X11ToWayland {
        content: ClipboardContent,
        clipboard_type: ClipboardType,
    },
    WaylandToX11 {
        content: ClipboardContent,
        clipboard_type: ClipboardType,
    },
}

// ============================================================================
// Configuration
// ============================================================================

pub const CLIPBOARD_ATOM: &str = "CLIPBOARD";
pub const PRIMARY_ATOM: &str = "PRIMARY";
pub const TARGETS_ATOM: &str = "TARGETS";
pub const MULTIPLE_ATOM: &str = "MULTIPLE";
pub const INCR_ATOM: &str = "INCR";
pub const UTF8_STRING_ATOM: &str = "UTF8_STRING";
pub const TEXT_PLAIN_UTF8_ATOM: &str = "text/plain;charset=utf-8";
pub const TEXT_PLAIN_ATOM: &str = "text/plain";
pub const IMAGE_PNG_MIME: &str = "image/png";
pub const IMAGE_JPEG_MIME: &str = "image/jpeg";
pub const IMAGE_JPG_MIME: &str = "image/jpg";

pub const TEXT_MIME_TYPES: &[&str] = &[TEXT_PLAIN_UTF8_ATOM, TEXT_PLAIN_ATOM, UTF8_STRING_ATOM];

pub const IMAGE_MIME_TYPES: &[&str] = &[IMAGE_PNG_MIME, IMAGE_JPEG_MIME, IMAGE_JPG_MIME];

pub const PREFERRED_MIME_TYPES: &[&str] = &[
    IMAGE_PNG_MIME,
    IMAGE_JPEG_MIME,
    IMAGE_JPG_MIME,
    TEXT_PLAIN_UTF8_ATOM,
    UTF8_STRING_ATOM,
    TEXT_PLAIN_ATOM,
];

pub fn is_text_mime_type(mime_type: &str) -> bool {
    TEXT_MIME_TYPES.contains(&mime_type)
}

pub fn is_image_mime_type(mime_type: &str) -> bool {
    IMAGE_MIME_TYPES.contains(&mime_type)
}

pub fn decode_clipboard_content(
    mime_type: &str,
    bytes: Vec<u8>,
) -> Result<ClipboardContent, String> {
    if is_text_mime_type(mime_type) {
        CompactString::from_utf8(bytes)
            .map(ClipboardContent::Text)
            .map_err(|e| format!("Failed to decode {} as UTF-8: {}", mime_type, e))
    } else if is_image_mime_type(mime_type) {
        let hash = xxhash_rust::xxh3::xxh3_64(&bytes);
        Ok(ClipboardContent::Data {
            mime_type: mime_type.to_compact_string(),
            bytes: Arc::from(bytes),
            hash,
        })
    } else {
        Err(format!("Unsupported clipboard MIME type: {}", mime_type))
    }
}
