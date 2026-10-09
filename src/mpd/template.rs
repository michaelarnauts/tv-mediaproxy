//! DASH segment template identifiers (`$RepresentationID$`, `$Time$`, …).
//!
//! A protected adaptation set's `BaseURL` carries its absolute initialization
//! and media templates (`t=`). A media segment request is matched against the
//! media template to recover the representation, which fills in the
//! initialization template: the init segment that decrypts the segment.

use regex::Regex;

/// The init segment URL for `segment_url`, or `None` if it does not match
/// `media`. Query strings are ignored; identifiers live in the path.
pub fn init_for_segment(init: &str, media: &str, segment_url: &str) -> Option<String> {
    let pattern = pattern(path(media))?;
    let caps = pattern.captures(path(segment_url))?;
    let rep = caps.name("rep").map(|m| m.as_str());
    let bandwidth = caps.name("bw").map(|m| m.as_str());
    Some(fill(init, rep, bandwidth))
}

fn path(url: &str) -> &str {
    url.split_once('?').map_or(url, |(p, _)| p)
}

/// A regex matching the URLs `template` expands to.
fn pattern(template: &str) -> Option<Regex> {
    let mut re = String::from("^");
    let (mut have_rep, mut have_bw) = (false, false);
    for (i, part) in template.split('$').enumerate() {
        if i % 2 == 0 {
            re.push_str(&regex::escape(part));
            continue;
        }
        let name = part.split_once('%').map_or(part, |(n, _)| n);
        match name {
            "" => re.push_str(r"\$"),
            "RepresentationID" if !have_rep => {
                re.push_str("(?P<rep>[^/]+?)");
                have_rep = true;
            }
            "RepresentationID" => re.push_str("[^/]+?"),
            "Bandwidth" if !have_bw => {
                re.push_str(r"(?P<bw>\d+)");
                have_bw = true;
            }
            "Bandwidth" | "Number" | "Time" | "SubNumber" => re.push_str(r"\d+"),
            other => re.push_str(&regex::escape(&format!("${other}$"))),
        }
    }
    re.push('$');
    Regex::new(&re).ok()
}

/// Fill in `$RepresentationID$` and `$Bandwidth$`; other identifiers stay.
pub fn fill(template: &str, rep: Option<&str>, bandwidth: Option<&str>) -> String {
    let mut parts = template.split('$');
    let mut out = parts.next().unwrap_or_default().to_string();
    while let Some(ident) = parts.next() {
        let (name, format) = ident.split_once('%').unwrap_or((ident, ""));
        let value = match name {
            "" => Some("$"),
            "RepresentationID" => rep,
            "Bandwidth" => bandwidth,
            _ => None,
        };
        match value {
            Some(v) => {
                let width = format
                    .strip_prefix('0')
                    .and_then(|f| f.strip_suffix('d'))
                    .and_then(|w| w.parse::<usize>().ok());
                match width {
                    Some(width) => out.push_str(&format!("{v:0>width$}")),
                    None => out.push_str(v),
                }
            }
            None => {
                out.push('$');
                out.push_str(ident);
                out.push('$');
            }
        }
        out.push_str(parts.next().unwrap_or_default());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_init_segment_of_a_media_segment() {
        let init = "https://cdn/a/dash/ch-$RepresentationID$.dash?m=1";
        let media = "https://cdn/a/dash/ch-$RepresentationID$-$Time$.dash?m=1";
        assert_eq!(
            init_for_segment(
                init,
                media,
                "https://cdn/a/dash/ch-video=5000000-1074936.dash?m=1"
            )
            .as_deref(),
            Some("https://cdn/a/dash/ch-video=5000000.dash?m=1")
        );
        assert_eq!(
            init_for_segment(
                init,
                media,
                "https://cdn/a/dash/ch-video=500000(mode=trik)-10.dash"
            )
            .as_deref(),
            Some("https://cdn/a/dash/ch-video=500000(mode=trik).dash?m=1")
        );
        assert_eq!(
            init_for_segment(init, media, "https://cdn/other/x.dash"),
            None
        );
    }

    #[test]
    fn number_and_bandwidth_formats() {
        assert_eq!(
            init_for_segment(
                "https://c/i_$Bandwidth%08d$.mp4",
                "https://c/s_$Bandwidth$_$Number%05d$.m4s",
                "https://c/s_500_00042.m4s"
            )
            .as_deref(),
            Some("https://c/i_00000500.mp4")
        );
    }

    #[test]
    fn fills_identifiers() {
        assert_eq!(
            fill(
                "$RepresentationID$/$Bandwidth%05d$-$$-$Time$.mp4",
                Some("v1"),
                Some("42")
            ),
            "v1/00042-$-$Time$.mp4"
        );
    }
}
