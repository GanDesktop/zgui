//! Media-type matching and encoding for clipboard transfers.

use std::ffi::OsString;
use std::path::PathBuf;

use zgui_platform::{ClipboardData, ClipboardError, ClipboardFormat, ClipboardMime};

/// Offered MIME names when storing plain text on a Wayland selection.
pub(crate) const TEXT_OFFERED: [&str; 5] = [
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "STRING",
    "TEXT",
];

/// Canonical media type for clipboard and drag-and-drop file lists.
pub(crate) const URI_LIST: &str = "text/uri-list";

/// Selects the preferred plain-text MIME name from `offered`.
pub(crate) fn best_text(offered: &[String]) -> Option<String> {
    const PREFERRED: [&str; 5] = [
        "text/plain;charset=utf-8",
        "UTF8_STRING",
        "text/plain",
        "STRING",
        "TEXT",
    ];
    PREFERRED
        .iter()
        .find_map(|wanted| {
            offered
                .iter()
                .find(|has| has.eq_ignore_ascii_case(wanted))
                .cloned()
        })
        .or_else(|| {
            offered
                .iter()
                .find(|has| has.to_ascii_lowercase().starts_with("text/"))
                .cloned()
        })
}

/// Selects the preferred image MIME name from `offered`.
pub(crate) fn best_image(offered: &[String]) -> Option<String> {
    const PREFERRED: [&str; 5] = [
        "image/png",
        "image/webp",
        "image/jpeg",
        "image/bmp",
        "image/gif",
    ];
    PREFERRED
        .iter()
        .find_map(|wanted| {
            offered
                .iter()
                .find(|has| has.eq_ignore_ascii_case(wanted))
                .cloned()
        })
        .or_else(|| {
            offered
                .iter()
                .find(|has| has.to_ascii_lowercase().starts_with("image/"))
                .cloned()
        })
}

/// Returns whether `offered` contains `text/uri-list`.
pub(crate) fn has_files(offered: &[String]) -> bool {
    offered.iter().any(|has| has.eq_ignore_ascii_case(URI_LIST))
}

/// Matches `wanted` against `offered`, preferring an exact name before case-insensitive matches.
pub(crate) fn match_offered<'a>(offered: &'a [String], wanted: &str) -> Option<&'a str> {
    offered
        .iter()
        .find(|has| has.as_str() == wanted)
        .or_else(|| offered.iter().find(|has| has.eq_ignore_ascii_case(wanted)))
        .map(String::as_str)
}

/// Selects the offered MIME name that satisfies a single-format read.
pub(crate) fn pick_for_format(offered: &[String], format: ClipboardFormat) -> Option<String> {
    match format {
        ClipboardFormat::Text => best_text(offered),
        ClipboardFormat::FileList => has_files(offered).then(|| {
            match_offered(offered, URI_LIST)
                .unwrap_or(URI_LIST)
                .to_owned()
        }),
        ClipboardFormat::Image => best_image(offered),
        _ => None,
    }
}

/// Converts raw bytes read for `mime_type` into a single-format `ClipboardData`.
pub(crate) fn decode_for_format(
    format: ClipboardFormat,
    mime_type: &str,
    bytes: Vec<u8>,
) -> ClipboardData {
    match format {
        ClipboardFormat::FileList => ClipboardData::FileList(decode_uri_list(&bytes)),
        ClipboardFormat::Image => ClipboardData::Image {
            media_type: mime_type.to_owned().into(),
            bytes,
        },
        _ => ClipboardData::Text(String::from_utf8_lossy(&bytes).into_owned().into()),
    }
}

/// Encodes `data` into MIME entries for writing to a native selection.
pub(crate) fn data_to_mimes(data: ClipboardData) -> Result<Vec<ClipboardMime>, ClipboardError> {
    match data {
        ClipboardData::Text(text) => Ok(TEXT_OFFERED
            .iter()
            .map(|mime| ClipboardMime::new(*mime, text.as_bytes().to_vec()))
            .collect()),
        ClipboardData::FileList(paths) => {
            Ok(vec![ClipboardMime::new(URI_LIST, encode_uri_list(&paths))])
        }
        ClipboardData::Image { media_type, bytes } => {
            Ok(vec![ClipboardMime::new(media_type.as_str(), bytes)])
        }
        other => Err(ClipboardError::UnsupportedFormat(other.format())),
    }
}

/// Encodes `paths` into an RFC 2483 `text/uri-list` payload.
pub(crate) fn encode_uri_list(paths: &[PathBuf]) -> Vec<u8> {
    let mut out = String::new();
    for path in paths {
        out.push_str("file://");
        for byte in path.as_os_str().as_encoded_bytes() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                    out.push(*byte as char)
                }
                _ => out.push_str(&format!("%{byte:02X}")),
            }
        }
        out.push_str("\r\n");
    }
    out.into_bytes()
}

/// Decodes a `text/uri-list` payload into filesystem paths.
pub(crate) fn decode_uri_list(bytes: &[u8]) -> Vec<PathBuf> {
    #[cfg(unix)]
    use std::os::unix::ffi::OsStringExt as _;

    let text = String::from_utf8_lossy(bytes);
    let mut paths = Vec::new();
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(rest) = line
            .strip_prefix("file://localhost")
            .or_else(|| line.strip_prefix("file://"))
        else {
            continue;
        };
        if !rest.starts_with('/') {
            continue;
        }
        let decoded = percent_decode(rest.as_bytes());
        #[cfg(unix)]
        {
            paths.push(PathBuf::from(OsString::from_vec(decoded)));
        }
        #[cfg(not(unix))]
        {
            let _ = OsString::new();
            paths.push(PathBuf::from(
                String::from_utf8_lossy(&decoded).into_owned(),
            ));
        }
    }
    paths
}

fn percent_decode(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] == b'%'
            && i + 2 < input.len()
            && let (Some(hi), Some(lo)) = (hex_value(input[i + 1]), hex_value(input[i + 2]))
        {
            out.push((hi << 4) | lo);
            i += 3;
            continue;
        }
        out.push(input[i]);
        i += 1;
    }
    out
}

const fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{best_text, decode_uri_list, encode_uri_list, match_offered};
    use std::path::PathBuf;

    #[test]
    fn uri_list_round_trips_special_characters() {
        let paths = vec![
            PathBuf::from("/tmp/file name.txt"),
            PathBuf::from("/home/user/100%done"),
        ];
        let encoded = encode_uri_list(&paths);
        assert_eq!(decode_uri_list(&encoded), paths);
    }

    #[test]
    fn match_offered_prefers_exact_case() {
        let offered = vec!["TEXT/PLAIN".to_owned(), "text/plain".to_owned()];
        assert_eq!(match_offered(&offered, "text/plain"), Some("text/plain"));
        assert_eq!(
            best_text(&["STRING".to_owned(), "text/plain;charset=utf-8".to_owned()]).as_deref(),
            Some("text/plain;charset=utf-8")
        );
    }
}
