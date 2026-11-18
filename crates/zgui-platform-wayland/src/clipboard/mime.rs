//! Which media types mean text, and which one to ask for.

use zgui_platform::ClipboardFormat;

/// The media types this backend offers when it takes a selection, best first.
///
/// Five names for one thing, and all five are needed. The first is what every modern toolkit
/// looks for; the last three are what applications written against X11's selection conventions
/// look for, and a great many still are. Offering only the modern name means a paste into an older
/// application silently produces nothing.
pub const OFFERED: [&str; 5] = [
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "STRING",
    "TEXT",
];

/// The media type for a list of files being dragged.
pub const URI_LIST: &str = "text/uri-list";

/// The media type to ask an offer for, given what it says it has.
///
/// In preference order rather than first-match, because an offer usually lists several and the
/// difference between them is the encoding: `STRING` is Latin-1 by X11 convention and
/// `text/plain` is unspecified, so asking for either when a declared UTF-8 name is on the list
/// turns every accented character into a question mark.
pub fn best_text(offered: &[String]) -> Option<String> {
    const PREFERRED: [&str; 5] = [
        "text/plain;charset=utf-8",
        "UTF8_STRING",
        "text/plain",
        "STRING",
        "TEXT",
    ];
    PREFERRED
        .iter()
        .find(|wanted| offered.iter().any(|has| has.eq_ignore_ascii_case(wanted)))
        .map(|found| (*found).to_owned())
        .or_else(|| {
            // Nothing named, but something that is still text: a compositor or an application may
            // offer `text/plain;charset=UTF-8` with different capitalisation, or a subtype nobody
            // enumerated. Anything under the text tree can at least be shown.
            offered
                .iter()
                .find(|has| has.to_ascii_lowercase().starts_with("text/"))
                .cloned()
        })
}

/// Whether an offer carries a list of files.
pub fn has_files(offered: &[String]) -> bool {
    offered.iter().any(|has| has.eq_ignore_ascii_case(URI_LIST))
}

/// The preferred image media type from `offered`, if any.
pub fn best_image(offered: &[String]) -> Option<String> {
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

/// Returns the matching MIME name as offered by the peer, preserving exact case first.
pub fn match_offered<'a>(offered: &'a [String], wanted: &str) -> Option<&'a str> {
    offered
        .iter()
        .find(|has| has.as_str() == wanted)
        .or_else(|| offered.iter().find(|has| has.eq_ignore_ascii_case(wanted)))
        .map(String::as_str)
}

/// Encodes `paths` as a RFC 2483 `text/uri-list` payload.
pub fn encode_uri_list(paths: &[std::path::PathBuf]) -> Vec<u8> {
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

/// Decodes a `text/uri-list` payload into local filesystem paths.
pub fn decode_uri_list(bytes: &[u8]) -> Vec<std::path::PathBuf> {
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
        paths.push(std::path::PathBuf::from(std::ffi::OsString::from_vec(
            decoded,
        )));
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

/// Whether this backend can answer a single-format request for `format`.
pub const fn is_supported(format: ClipboardFormat) -> bool {
    matches!(
        format,
        ClipboardFormat::Text | ClipboardFormat::FileList | ClipboardFormat::Image
    )
}

#[cfg(test)]
mod tests {
    use super::{best_text, has_files, is_supported};
    use zgui_platform::ClipboardFormat;

    fn offered(types: &[&str]) -> Vec<String> {
        types.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn an_offer_with_nothing_on_it_has_nothing_to_ask_for() {
        assert_eq!(best_text(&offered(&[])), None);
        assert_eq!(best_text(&offered(&["image/png"])), None);
    }

    #[test]
    fn a_declared_encoding_wins_over_one_that_has_to_be_guessed() {
        // `STRING` is Latin-1 by convention and `text/plain` says nothing, so taking either while
        // a declared UTF-8 name is on the list turns every accented character into a question mark.
        let both = offered(&["STRING", "text/plain", "text/plain;charset=utf-8"]);
        assert_eq!(
            best_text(&both).as_deref(),
            Some("text/plain;charset=utf-8")
        );
        let older = offered(&["STRING", "TEXT"]);
        assert_eq!(best_text(&older).as_deref(), Some("STRING"));
    }

    #[test]
    fn a_name_in_another_case_is_still_that_name() {
        let shouted = offered(&["TEXT/PLAIN;CHARSET=UTF-8"]);
        assert_eq!(
            best_text(&shouted).as_deref(),
            Some("text/plain;charset=utf-8")
        );
    }

    #[test]
    fn a_text_subtype_nobody_enumerated_is_still_text() {
        let unusual = offered(&["text/markdown"]);
        assert_eq!(best_text(&unusual).as_deref(), Some("text/markdown"));
    }

    #[test]
    fn a_list_of_files_is_recognised_whatever_its_case() {
        assert!(has_files(&offered(&["text/uri-list"])));
        assert!(has_files(&offered(&["TEXT/URI-LIST"])));
        assert!(!has_files(&offered(&["text/plain"])));
    }

    #[test]
    fn supported_single_formats_include_text_files_and_images() {
        assert!(is_supported(ClipboardFormat::Text));
        assert!(is_supported(ClipboardFormat::Image));
        assert!(is_supported(ClipboardFormat::FileList));
        assert!(!is_supported(ClipboardFormat::Html));
    }

    #[test]
    fn uri_list_round_trips_paths_with_spaces() {
        use std::path::PathBuf;

        let paths = vec![
            PathBuf::from("/tmp/hello world.png"),
            PathBuf::from("/home/user/100%.txt"),
        ];
        let encoded = super::encode_uri_list(&paths);
        assert_eq!(super::decode_uri_list(&encoded), paths);
    }
}
