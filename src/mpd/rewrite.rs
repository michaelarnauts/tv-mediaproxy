//! Rewrite an upstream MPD so a DASH player can play it through this proxy.
//!
//! The manifest stays a DASH manifest: periods, timelines, segment templates
//! and the whole time-shift window are passed on untouched, so the player can
//! seek back as far as the origin allows. Only addressing and protection
//! change:
//!
//! - every `BaseURL` is resolved against the (post-redirect) manifest URL and
//!   removed; an absolute one is put on each element that owns a segment
//!   template instead (players resolve a template from where it is defined,
//!   VLC ignores a `BaseURL` below that),
//! - for protected content that `BaseURL` points into this proxy:
//!   `{prefix}/seg/{key}/t={templates}/{origin base}`, so the relative segment
//!   URLs the player builds carry the keys and the templates that lead from a
//!   media segment to its init segment (see `template`),
//! - clear content keeps pointing at the origin,
//! - `ContentProtection`, `Location` and `PatchLocation` are dropped,
//! - `startNumber` is dropped from templates addressed by `$Time$`: it plays
//!   no part in their URLs, and ffmpeg's DASH demuxer (Jellyfin) repeats
//!   segments of a live timeline that carries one.
//!
//! The XML is held in a minimal lossless tree so everything this module does
//! not touch is written back byte-for-byte equivalent.

use base64::Engine;
use quick_xml::events::{BytesEnd, BytesStart, BytesText, Event};
use quick_xml::{Reader, Writer};
use url::Url;

pub struct RewriteParams<'a> {
    /// Upstream manifest URL after redirects; the root of URL resolution.
    pub manifest_url: &'a str,
    /// Public base URL of this proxy including the password, no trailing slash.
    pub prefix: &'a str,
    /// `kid:key[,kid:key]` for protected content. Without it, protected
    /// representations keep pointing at the origin.
    pub key: Option<&'a str>,
}

// ---------------------------------------------------------------------------
// Minimal lossless XML tree
// ---------------------------------------------------------------------------

enum Node {
    Elem(Elem),
    Other(Event<'static>),
}

struct Elem {
    start: BytesStart<'static>,
    children: Vec<Node>,
}

impl Elem {
    fn local(&self) -> &[u8] {
        self.start.local_name().into_inner()
    }

    fn attr(&self, name: &[u8]) -> Option<String> {
        self.start
            .attributes()
            .flatten()
            .find(|a| a.key.local_name().as_ref() == name)
            .and_then(|a| a.unescape_value().ok().map(|v| v.into_owned()))
    }

    fn remove_attr(&mut self, name: &str) {
        let tag = String::from_utf8_lossy(self.start.name().as_ref()).into_owned();
        let mut start = BytesStart::new(tag);
        for a in self.start.attributes().flatten() {
            if a.key.as_ref() != name.as_bytes() {
                start.push_attribute(a);
            }
        }
        self.start = start;
    }

    fn child_elems(&self) -> impl Iterator<Item = &Elem> {
        self.children.iter().filter_map(|n| match n {
            Node::Elem(e) => Some(e),
            Node::Other(_) => None,
        })
    }

    fn child_elems_mut(&mut self) -> impl Iterator<Item = &mut Elem> {
        self.children.iter_mut().filter_map(|n| match n {
            Node::Elem(e) => Some(e),
            Node::Other(_) => None,
        })
    }

    fn child(&self, name: &[u8]) -> Option<&Elem> {
        self.child_elems().find(|e| e.local() == name)
    }

    fn child_mut(&mut self, name: &[u8]) -> Option<&mut Elem> {
        self.child_elems_mut().find(|e| e.local() == name)
    }

    fn text(&self) -> String {
        let mut out = String::new();
        for n in &self.children {
            match n {
                Node::Other(Event::Text(t)) => {
                    out.push_str(&t.unescape().unwrap_or_default());
                }
                Node::Other(Event::CData(t)) => {
                    out.push_str(&String::from_utf8_lossy(t));
                }
                _ => {}
            }
        }
        out.trim().to_string()
    }

    fn remove_children(&mut self, names: &[&[u8]]) {
        self.children.retain(|n| match n {
            Node::Elem(e) => !names.contains(&e.local()),
            Node::Other(_) => true,
        });
    }
}

fn parse(xml: &[u8]) -> Result<Vec<Node>, String> {
    let mut reader = Reader::from_reader(xml);
    let mut buf = Vec::new();
    let mut top: Vec<Node> = Vec::new();
    let mut stack: Vec<Elem> = Vec::new();

    loop {
        let event = reader
            .read_event_into(&mut buf)
            .map_err(|e| format!("XML error at {}: {e}", reader.buffer_position()))?
            .into_owned();
        let node = match event {
            Event::Eof => break,
            Event::Start(start) => {
                stack.push(Elem {
                    start,
                    children: Vec::new(),
                });
                buf.clear();
                continue;
            }
            Event::End(_) => match stack.pop() {
                Some(elem) => Node::Elem(elem),
                None => return Err("unbalanced end tag".into()),
            },
            Event::Empty(start) => Node::Elem(Elem {
                start,
                children: Vec::new(),
            }),
            other => Node::Other(other),
        };
        match stack.last_mut() {
            Some(parent) => parent.children.push(node),
            None => top.push(node),
        }
        buf.clear();
    }
    if !stack.is_empty() {
        return Err("unclosed element".into());
    }
    Ok(top)
}

fn write_nodes(writer: &mut Writer<Vec<u8>>, nodes: &[Node]) -> std::io::Result<()> {
    for node in nodes {
        match node {
            Node::Other(event) => writer.write_event(event.borrow())?,
            Node::Elem(elem) if elem.children.is_empty() => {
                writer.write_event(Event::Empty(elem.start.borrow()))?
            }
            Node::Elem(elem) => {
                writer.write_event(Event::Start(elem.start.borrow()))?;
                write_nodes(writer, &elem.children)?;
                let name = String::from_utf8_lossy(elem.start.name().as_ref()).into_owned();
                writer.write_event(Event::End(BytesEnd::new(name)))?;
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Rewriting
// ---------------------------------------------------------------------------

/// The `media` / `initialization` templates in effect at some level.
#[derive(Clone, Default)]
struct Template {
    media: Option<String>,
    init: Option<String>,
}

pub fn rewrite_mpd(xml: &[u8], params: &RewriteParams) -> Result<Vec<u8>, String> {
    let mut nodes = parse(xml)?;
    let manifest = Url::parse(params.manifest_url).map_err(|e| format!("bad manifest URL: {e}"))?;

    let mpd = nodes
        .iter_mut()
        .find_map(|n| match n {
            Node::Elem(e) if e.local() == b"MPD" => Some(e),
            _ => None,
        })
        .ok_or("no MPD root element")?;

    let mpd_base = join_base(&manifest, mpd);
    mpd.remove_children(&[b"BaseURL", b"Location", b"PatchLocation"]);

    for period in mpd.child_elems_mut().filter(|e| e.local() == b"Period") {
        let (period_base, period_protected, period_template) =
            rewrite_level(period, &mpd_base, false, &Template::default(), params);

        for set in period
            .child_elems_mut()
            .filter(|e| e.local() == b"AdaptationSet")
        {
            let (set_base, set_protected, set_template) = rewrite_level(
                set,
                &period_base,
                period_protected,
                &period_template,
                params,
            );

            for rep in set
                .child_elems_mut()
                .filter(|e| e.local() == b"Representation")
            {
                let (base, _, template) =
                    rewrite_level(rep, &set_base, set_protected, &set_template, params);
                if template.media.is_none() {
                    // SegmentBase / SegmentList: not rewritten. Keep the origin
                    // address so clear content still plays.
                    tracing::warn!(
                        "Representation {:?} has no SegmentTemplate; left unproxied",
                        rep.attr(b"id")
                    );
                    set_base_url(rep, base.as_str());
                }
            }
        }
    }

    let mut writer = Writer::new(Vec::with_capacity(xml.len() + 4096));
    write_nodes(&mut writer, &nodes).map_err(|e| e.to_string())?;
    Ok(writer.into_inner())
}

/// Rewrite one Period, AdaptationSet or Representation: resolve and drop its
/// `BaseURL`, drop its protection, and give it a new `BaseURL` when it owns a
/// segment template or moves the base of an inherited one. Returns, for its
/// children, its base, whether it is protected and the template in effect.
fn rewrite_level(
    elem: &mut Elem,
    parent_base: &Url,
    parent_protected: bool,
    inherited: &Template,
    params: &RewriteParams,
) -> (Url, bool, Template) {
    let base = join_base(parent_base, elem);
    let moves_base = elem.child(b"BaseURL").is_some();
    // A template defined here also serves protected content further down.
    let protected = parent_protected || is_protected(elem);
    let own_protection = parent_protected || elem.child(b"ContentProtection").is_some();
    elem.remove_children(&[b"BaseURL", b"ContentProtection"]);

    let owns = elem
        .child(b"SegmentTemplate")
        .is_some_and(|t| t.attr(b"media").is_some());
    let template = merge_template(elem, inherited);
    if template.media.is_some() && (owns || moves_base) {
        let url = template_base(&base, protected, &template, params);
        set_base_url(elem, &url);
    }
    (base, own_protection, template)
}

/// Resolve the element's first `BaseURL` child against `parent`.
fn join_base(parent: &Url, elem: &Elem) -> Url {
    match elem.child(b"BaseURL").map(Elem::text) {
        Some(text) if !text.is_empty() => parent.join(&text).unwrap_or_else(|_| parent.clone()),
        _ => parent.clone(),
    }
}

/// Whether the element or anything below it is protected.
fn is_protected(elem: &Elem) -> bool {
    elem.child(b"ContentProtection").is_some()
        || elem
            .child_elems()
            .filter(|e| matches!(e.local(), b"AdaptationSet" | b"Representation"))
            .any(is_protected)
}

/// Merge the element's own `SegmentTemplate` into the inherited one, and drop
/// its `startNumber` when segments are addressed by time.
fn merge_template(elem: &mut Elem, inherited: &Template) -> Template {
    let mut merged = inherited.clone();
    if let Some(template) = elem.child_mut(b"SegmentTemplate") {
        merged.media = template.attr(b"media").or(merged.media);
        merged.init = template.attr(b"initialization").or(merged.init);
        let by_time = merged
            .media
            .as_deref()
            .is_some_and(|m| m.contains("$Time") && !m.contains("$Number"));
        if by_time {
            template.remove_attr("startNumber");
        }
    }
    merged
}

/// The `BaseURL` for an element whose template is in effect: into this proxy
/// when its content has to be decrypted, the origin otherwise.
fn template_base(
    base: &Url,
    protected: bool,
    template: &Template,
    params: &RewriteParams,
) -> String {
    if !protected {
        return base.to_string();
    }
    let (Some(key), Some(media), Some(init)) = (params.key, &template.media, &template.init) else {
        tracing::warn!("Protected content left encrypted: no key or no initialization template");
        return base.to_string();
    };
    if Url::parse(media).is_ok() {
        tracing::warn!("Protected content left encrypted: absolute media template");
        return base.to_string();
    }
    let absolute = |t: &str| base.join(t).map(String::from).unwrap_or_default();
    let templates = format!("{}\n{}", absolute(init), absolute(media));
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(templates);
    format!("{}/seg/{key}/t={encoded}/{base}", params.prefix)
}

fn set_base_url(elem: &mut Elem, url: &str) {
    let node = Elem {
        start: BytesStart::new("BaseURL"),
        children: vec![Node::Other(Event::Text(BytesText::new(url).into_owned()))],
    };
    elem.children.insert(0, Node::Elem(node));
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "00112233445566778899aabbccddeeff:ffeeddccbbaa99887766554433221100";

    fn rewrite(xml: &str) -> String {
        let params = RewriteParams {
            manifest_url: "https://cdn.example/live/ch1/index.mpd?token=a",
            prefix: "http://proxy:8888/pw",
            key: Some(KEY),
        };
        String::from_utf8(rewrite_mpd(xml.as_bytes(), &params).unwrap()).unwrap()
    }

    fn t(init: &str, media: &str) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!("{init}\n{media}"))
    }

    #[test]
    fn protected_template_owner_gets_a_proxy_base_url() {
        let out = rewrite(
            r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" xmlns:cenc="urn:mpeg:cenc:2013" type="dynamic">
 <BaseURL>dash/</BaseURL>
 <Period id="1">
  <AdaptationSet mimeType="video/mp4">
   <ContentProtection schemeIdUri="urn:mpeg:dash:mp4protection:2011" value="cenc" cenc:default_KID="00112233-4455-6677-8899-aabbccddeeff"/>
   <ContentProtection schemeIdUri="urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed"><cenc:pssh>AAAA</cenc:pssh></ContentProtection>
   <SegmentTemplate timescale="90000" initialization="ch1-$RepresentationID$.dash" media="ch1-$RepresentationID$-$Time$.dash"><SegmentTimeline><S t="0" d="90000" r="2"/></SegmentTimeline></SegmentTemplate>
   <Representation id="video=1" bandwidth="1"/>
  </AdaptationSet>
 </Period>
</MPD>"#,
        );
        assert!(!out.contains("ContentProtection"), "{out}");
        assert!(
            out.contains(r#"media="ch1-$RepresentationID$-$Time$.dash""#),
            "templates stay untouched: {out}"
        );
        assert!(out.contains(r#"<S t="0" d="90000" r="2"/>"#), "{out}");
        let t = t(
            "https://cdn.example/live/ch1/dash/ch1-$RepresentationID$.dash",
            "https://cdn.example/live/ch1/dash/ch1-$RepresentationID$-$Time$.dash",
        );
        assert!(
            out.contains(&format!(
                r#"<AdaptationSet mimeType="video/mp4"><BaseURL>http://proxy:8888/pw/seg/{KEY}/t={t}/https://cdn.example/live/ch1/dash/</BaseURL>"#
            )),
            "{out}"
        );
        assert!(
            out.contains(r#"<Representation id="video=1" bandwidth="1"/>"#),
            "{out}"
        );
        assert_eq!(out.matches("<BaseURL>").count(), 1, "{out}");
    }

    #[test]
    fn clear_template_owner_points_at_origin_and_relative_parents_resolve() {
        let out = rewrite(
            r#"<MPD><Period><BaseURL>../../other/</BaseURL><AdaptationSet><SegmentTemplate initialization="i.mp4?m=1" media="s_$Number$.mp4?m=1"/><Representation id="a"/></AdaptationSet></Period></MPD>"#,
        );
        assert!(
            out.contains("<AdaptationSet><BaseURL>https://cdn.example/other/</BaseURL>"),
            "{out}"
        );
        assert!(out.contains(r#"media="s_$Number$.mp4?m=1""#), "{out}");
    }

    #[test]
    fn representation_base_url_moves_the_template_base() {
        let out = rewrite(
            r#"<MPD><Period><AdaptationSet><ContentProtection/><SegmentTemplate media="s_$Time$.m4s" initialization="i.mp4"/><Representation id="a"><BaseURL>a/</BaseURL></Representation></AdaptationSet></Period></MPD>"#,
        );
        let t = t(
            "https://cdn.example/live/ch1/a/i.mp4",
            "https://cdn.example/live/ch1/a/s_$Time$.m4s",
        );
        assert!(
            out.contains(&format!(
                r#"<Representation id="a"><BaseURL>http://proxy:8888/pw/seg/{KEY}/t={t}/https://cdn.example/live/ch1/a/</BaseURL></Representation>"#
            )),
            "{out}"
        );
    }

    #[test]
    fn start_number_dropped_only_for_time_addressing() {
        let out = rewrite(
            r#"<MPD><Period><AdaptationSet><SegmentTemplate startNumber="933100881" media="a-$Time$.m4s" initialization="i.mp4"/><Representation id="a"/></AdaptationSet><AdaptationSet><SegmentTemplate startNumber="7" media="b-$Number$.m4s" initialization="i.mp4"/><Representation id="b"/></AdaptationSet></Period></MPD>"#,
        );
        assert!(!out.contains("933100881"), "{out}");
        assert!(out.contains(r#"startNumber="7""#), "{out}");
    }
}
