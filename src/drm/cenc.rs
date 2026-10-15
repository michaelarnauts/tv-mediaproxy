//! Decrypts `cenc` (AES-CTR) and `cbcs` (AES-CBC pattern) MP4 segments and
//! strips their protection boxes. ClearKey keys are AES-128.

use std::collections::HashMap;

use super::mp4_atom::{Mp4Atom, Mp4Parser};

// ---------------------------------------------------------------------------
// Sample auxiliary data
// ---------------------------------------------------------------------------

/// Per-sample encryption info parsed from a `senc` box.
#[derive(Debug, Clone)]
pub struct SampleInfo {
    /// Initialization vector (8 or 16 bytes).
    pub iv: Vec<u8>,
    /// Sub-sample list: (clear_bytes, encrypted_bytes).
    pub sub_samples: Vec<(u16, u32)>,
}

// ---------------------------------------------------------------------------
// Per-track info (collected during moof processing)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct TrackInfo {
    data_offset: i32,
    sample_sizes: Vec<u32>,
    sample_info: Vec<SampleInfo>,
    key: Option<Vec<u8>>,
    default_sample_size: u32,
    crypt_byte_block: u8,
    skip_byte_block: u8,
    constant_iv: Option<Vec<u8>>,
}

/// Boxes in a `traf` that only describe encryption: sample auxiliary info and
/// the `seig` sample group. They are removed along with the encryption.
fn is_traf_encryption_box(atom: &Mp4Atom) -> bool {
    match &atom.atom_type {
        b"senc" | b"saiz" | b"saio" => true,
        b"sbgp" | b"sgpd" => atom.data.get(4..8) == Some(b"seig".as_slice()),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// MP4Decrypter
// ---------------------------------------------------------------------------

pub struct Mp4Decrypter {
    key_map: HashMap<Vec<u8>, Vec<u8>>,
    // State accumulated during moof / stsd processing
    /// `cbcs` rather than `cenc`.
    cbcs: bool,
    default_iv_size: usize,
    crypt_byte_block: u8,
    skip_byte_block: u8,
    constant_iv: Option<Vec<u8>>,
    default_sample_size: u32,
    trun_sample_sizes: Vec<u32>,
    total_encryption_overhead: usize,
    track_infos: Vec<TrackInfo>,
    track_encryption_settings: HashMap<u32, TrackSettings>,
    extracted_kids: HashMap<u32, Vec<u8>>,
    current_track_id: u32,
}

#[derive(Debug, Clone)]
struct TrackSettings {
    crypt_byte_block: u8,
    skip_byte_block: u8,
    constant_iv: Option<Vec<u8>>,
    iv_size: usize,
}

impl Mp4Decrypter {
    pub fn new(key_map: HashMap<Vec<u8>, Vec<u8>>) -> Self {
        Self {
            key_map,
            cbcs: false,
            default_iv_size: 8,
            crypt_byte_block: 1,
            skip_byte_block: 9,
            constant_iv: None,
            default_sample_size: 0,
            trun_sample_sizes: Vec::new(),
            total_encryption_overhead: 0,
            track_infos: Vec::new(),
            track_encryption_settings: HashMap::new(),
            extracted_kids: HashMap::new(),
            current_track_id: 0,
        }
    }

    // -----------------------------------------------------------------------
    // Public entry points
    // -----------------------------------------------------------------------

    /// Decrypt a media segment. The init segment supplies the track and
    /// encryption parameters.
    pub fn decrypt_segment(&mut self, init: &[u8], segment: &[u8]) -> Vec<u8> {
        let init_atoms = Mp4Parser::new(init.to_vec()).list_atoms();
        let segment_atoms = Mp4Parser::new(segment.to_vec()).list_atoms();
        let all = || init_atoms.iter().chain(segment_atoms.iter());

        // Process in canonical order
        let mut processed: HashMap<[u8; 4], Mp4Atom> = HashMap::new();
        for type_tag in [*b"moov", *b"moof", *b"sidx", *b"mdat"] {
            if let Some(atom) = all().find(|a| a.atom_type == type_tag) {
                let result = self.process_atom(atom);
                processed.insert(type_tag, result);
            }
        }

        let mut out = Vec::new();
        for atom in &segment_atoms {
            if let Some(p) = processed.get(&atom.atom_type) {
                out.extend_from_slice(&p.pack());
            } else {
                out.extend_from_slice(&atom.pack());
            }
        }
        out
    }

    /// Process only the init segment (remove encryption boxes from moov).
    pub fn process_init_only(&mut self, init: &[u8]) -> Vec<u8> {
        let mut parser = Mp4Parser::new(init.to_vec());
        let atoms = parser.list_atoms();

        let moov_processed = atoms
            .iter()
            .find(|a| a.atom_type == *b"moov")
            .map(|a| self.process_moov(a));

        let mut out = Vec::new();
        for atom in &atoms {
            if atom.atom_type == *b"moov" {
                if let Some(ref p) = moov_processed {
                    out.extend_from_slice(&p.pack());
                } else {
                    out.extend_from_slice(&atom.pack());
                }
            } else {
                out.extend_from_slice(&atom.pack());
            }
        }
        out
    }

    // -----------------------------------------------------------------------
    // Atom dispatch
    // -----------------------------------------------------------------------

    fn process_atom(&mut self, atom: &Mp4Atom) -> Mp4Atom {
        match &atom.atom_type {
            b"moov" => self.process_moov(atom),
            b"moof" => self.process_moof(atom),
            b"sidx" => self.process_sidx(atom),
            b"mdat" => self.decrypt_mdat(atom),
            _ => atom.clone(),
        }
    }

    // -----------------------------------------------------------------------
    // moov processing
    // -----------------------------------------------------------------------

    fn process_moov(&mut self, moov: &Mp4Atom) -> Mp4Atom {
        let children = Mp4Parser::children_of(&moov.data);
        let mut new_data = Vec::new();
        for child in &children {
            if child.atom_type == *b"trak" {
                let new_trak = self.process_trak(child);
                new_data.extend_from_slice(&new_trak.pack());
            } else if child.atom_type != *b"pssh" {
                new_data.extend_from_slice(&child.pack());
            }
        }
        Mp4Atom::new(*b"moov", new_data)
    }

    fn process_trak(&mut self, trak: &Mp4Atom) -> Mp4Atom {
        let children = Mp4Parser::children_of(&trak.data);

        // First pass: find track ID from tkhd
        for child in &children {
            if child.atom_type == *b"tkhd" {
                let version = child.data[0];
                let id_offset = if version == 0 { 12 } else { 20 };
                if child.data.len() >= id_offset + 4 {
                    self.current_track_id = u32::from_be_bytes(
                        child.data[id_offset..id_offset + 4]
                            .try_into()
                            .unwrap_or([0; 4]),
                    );
                }
                break;
            }
        }

        // Second pass: process atoms
        let mut new_data = Vec::new();
        for child in &children {
            if child.atom_type == *b"mdia" {
                new_data.extend_from_slice(&self.process_mdia(child).pack());
            } else {
                new_data.extend_from_slice(&child.pack());
            }
        }
        Mp4Atom::new(*b"trak", new_data)
    }

    fn process_mdia(&mut self, mdia: &Mp4Atom) -> Mp4Atom {
        let mut new_data = Vec::new();
        for child in Mp4Parser::children_of(&mdia.data) {
            if child.atom_type == *b"minf" {
                new_data.extend_from_slice(&self.process_minf(&child).pack());
            } else {
                new_data.extend_from_slice(&child.pack());
            }
        }
        Mp4Atom::new(*b"mdia", new_data)
    }

    fn process_minf(&mut self, minf: &Mp4Atom) -> Mp4Atom {
        let mut new_data = Vec::new();
        for child in Mp4Parser::children_of(&minf.data) {
            if child.atom_type == *b"stbl" {
                new_data.extend_from_slice(&self.process_stbl(&child).pack());
            } else {
                new_data.extend_from_slice(&child.pack());
            }
        }
        Mp4Atom::new(*b"minf", new_data)
    }

    fn process_stbl(&mut self, stbl: &Mp4Atom) -> Mp4Atom {
        let mut new_data = Vec::new();
        for child in Mp4Parser::children_of(&stbl.data) {
            if child.atom_type == *b"stsd" {
                new_data.extend_from_slice(&self.process_stsd(&child).pack());
            } else {
                new_data.extend_from_slice(&child.pack());
            }
        }
        Mp4Atom::new(*b"stbl", new_data)
    }

    fn process_stsd(&mut self, stsd: &Mp4Atom) -> Mp4Atom {
        if stsd.data.len() < 8 {
            return stsd.clone();
        }
        let entry_count = u32::from_be_bytes(stsd.data[4..8].try_into().unwrap_or([0; 4]));
        let mut new_data = stsd.data[..8].to_vec();

        let mut parser = Mp4Parser::new(stsd.data[8..].to_vec());
        for _ in 0..entry_count {
            if let Some(entry) = parser.read_atom() {
                let processed = self.process_sample_entry(&entry);
                new_data.extend_from_slice(&processed.pack());
            }
        }
        Mp4Atom::new(*b"stsd", new_data)
    }

    fn process_sample_entry(&mut self, entry: &Mp4Atom) -> Mp4Atom {
        // Fixed-field size depends on sample entry type
        let fixed_size: usize = match &entry.atom_type {
            b"mp4a" | b"enca" => 28,
            b"mp4v" | b"encv" | b"avc1" | b"hev1" | b"hvc1" => 78,
            _ => 16,
        };

        if entry.data.len() < fixed_size {
            return entry.clone();
        }

        let mut new_data = entry.data[..fixed_size.min(entry.data.len())].to_vec();
        let mut codec_format: Option<[u8; 4]> = None;

        let mut parser = Mp4Parser::new(entry.data[fixed_size..].to_vec());
        for child in parser.list_atoms() {
            match &child.atom_type {
                b"sinf" => {
                    codec_format = self.extract_codec_format(&child);
                }
                b"schi" | b"tenc" | b"schm" => { /* skip */ }
                _ => new_data.extend_from_slice(&child.pack()),
            }
        }

        let new_type = codec_format.unwrap_or(entry.atom_type);
        Mp4Atom::new(new_type, new_data)
    }

    fn extract_codec_format(&mut self, sinf: &Mp4Atom) -> Option<[u8; 4]> {
        let mut codec_format: Option<[u8; 4]> = None;
        for child in Mp4Parser::children_of(&sinf.data) {
            match &child.atom_type {
                b"frma" if child.data.len() >= 4 => {
                    let mut t = [0u8; 4];
                    t.copy_from_slice(&child.data[..4]);
                    codec_format = Some(t);
                }
                b"schm" => self.parse_schm(&child),
                b"schi" => {
                    for schi_child in Mp4Parser::children_of(&child.data) {
                        if schi_child.atom_type == *b"tenc" {
                            self.parse_tenc(&schi_child);
                        }
                    }
                }
                _ => {}
            }
        }
        codec_format
    }

    fn parse_schm(&mut self, schm: &Mp4Atom) {
        if schm.data.len() >= 8 {
            let scheme_type = &schm.data[4..8];
            self.cbcs = scheme_type == b"cbcs";
        }
    }

    fn parse_tenc(&mut self, tenc: &Mp4Atom) {
        let data = &tenc.data;
        if data.len() < 8 {
            return;
        }
        let version = data[0];
        let mut settings = TrackSettings {
            crypt_byte_block: 1,
            skip_byte_block: 9,
            constant_iv: None,
            iv_size: 8,
        };

        if version > 0 && data.len() >= 6 {
            let pattern_byte = data[5];
            settings.crypt_byte_block = (pattern_byte >> 4) & 0x0F;
            settings.skip_byte_block = pattern_byte & 0x0F;
            self.crypt_byte_block = settings.crypt_byte_block;
            self.skip_byte_block = settings.skip_byte_block;
        }

        // Extract KID (16 bytes at offset 8)
        if data.len() >= 24 {
            let kid = data[8..24].to_vec();
            if self.current_track_id > 0 {
                self.extracted_kids
                    .insert(self.current_track_id, kid.clone());
            }
        }

        // IV size at offset 7
        if data.len() > 7 {
            let iv_size = data[7] as usize;
            if iv_size == 0 || iv_size == 8 || iv_size == 16 {
                settings.iv_size = if iv_size > 0 { iv_size } else { 16 };
                self.default_iv_size = settings.iv_size;

                if iv_size == 0 && data.len() > 24 {
                    let civ_size_offset = 24;
                    let civ_size = data[civ_size_offset] as usize;
                    if civ_size > 0 && data.len() >= civ_size_offset + 1 + civ_size {
                        let civ =
                            data[civ_size_offset + 1..civ_size_offset + 1 + civ_size].to_vec();
                        settings.constant_iv = Some(civ.clone());
                        self.constant_iv = Some(civ);
                    }
                }
            }
        }

        if self.current_track_id > 0 {
            self.track_encryption_settings
                .insert(self.current_track_id, settings);
        }
    }

    // -----------------------------------------------------------------------
    // moof processing
    // -----------------------------------------------------------------------

    fn process_moof(&mut self, moof: &Mp4Atom) -> Mp4Atom {
        let children = Mp4Parser::children_of(&moof.data);
        self.track_infos.clear();

        // First pass: calculate total encryption overhead — every byte removed
        // from the moof shifts the sample data offsets.
        self.total_encryption_overhead = 0;
        for child in &children {
            if child.atom_type == *b"traf" {
                let traf_children = Mp4Parser::children_of(&child.data);
                let overhead: usize = traf_children
                    .iter()
                    .filter(|a| is_traf_encryption_box(a))
                    .map(|a| a.data.len() + 8)
                    .sum();
                self.total_encryption_overhead += overhead;
            } else if child.atom_type == *b"pssh" {
                self.total_encryption_overhead += child.data.len() + 8;
            }
        }

        // Second pass: process atoms. A pssh left in a moof makes a browser
        // treat the stream as encrypted.
        let mut new_data = Vec::new();
        for child in &children {
            if child.atom_type == *b"traf" {
                new_data.extend_from_slice(&self.process_traf(child).pack());
            } else if child.atom_type != *b"pssh" {
                new_data.extend_from_slice(&child.pack());
            }
        }
        Mp4Atom::new(*b"moof", new_data)
    }

    fn process_traf(&mut self, traf: &Mp4Atom) -> Mp4Atom {
        let children = Mp4Parser::children_of(&traf.data);
        let mut new_data = Vec::new();
        let mut tfhd_track_id: u32 = 0;
        let mut has_tfhd = false;
        let mut sample_count = 0u32;
        let mut trun_data_offset = 0i32;
        let mut sample_info: Vec<SampleInfo> = Vec::new();
        let mut track_default_sample_size = 0u32;

        for child in &children {
            match &child.atom_type {
                b"tfhd" => {
                    has_tfhd = true;
                    self.parse_tfhd(child);
                    track_default_sample_size = self.default_sample_size;
                    if child.data.len() >= 8 {
                        tfhd_track_id =
                            u32::from_be_bytes(child.data[4..8].try_into().unwrap_or([0; 4]));
                    }
                    new_data.extend_from_slice(&child.pack());
                }
                b"trun" => {
                    let (sc, doff) = self.process_trun(child);
                    sample_count = sc;
                    trun_data_offset = doff;
                    let modified = self.modify_trun(child);
                    new_data.extend_from_slice(&modified.pack());
                }
                b"senc" => {
                    sample_info = self.parse_senc(child, sample_count);
                    // senc is NOT written to new_data (stripped)
                }
                _ if is_traf_encryption_box(child) => { /* stripped */ }
                _ => new_data.extend_from_slice(&child.pack()),
            }
        }

        let track_key = self.get_key_for_track(tfhd_track_id);
        let track_enc = self.track_encryption_settings.get(&tfhd_track_id).cloned();
        let (crypt, skip, civ) = track_enc
            .as_ref()
            .map(|s| (s.crypt_byte_block, s.skip_byte_block, s.constant_iv.clone()))
            .unwrap_or((
                self.crypt_byte_block,
                self.skip_byte_block,
                self.constant_iv.clone(),
            ));

        self.track_infos.push(TrackInfo {
            data_offset: trun_data_offset,
            sample_sizes: self.trun_sample_sizes.clone(),
            sample_info,
            key: track_key.cloned(),
            default_sample_size: track_default_sample_size,
            crypt_byte_block: crypt,
            skip_byte_block: skip,
            constant_iv: civ,
        });

        // Some CMAF audio files omit tfhd and rely on trex defaults.
        // ffmpeg's mp4 parser requires tfhd to identify the track, so synthesize
        // a minimal one when none was present in the original traf.
        if !has_tfhd && self.current_track_id > 0 {
            let mut tfhd_data = vec![0u8; 8]; // version=0, flags=0, track_id
            tfhd_data[4..8].copy_from_slice(&self.current_track_id.to_be_bytes());
            let tfhd_atom = Mp4Atom::new(*b"tfhd", tfhd_data);
            // tfhd must precede trun inside traf
            let mut with_tfhd = tfhd_atom.pack();
            with_tfhd.extend_from_slice(&new_data);
            new_data = with_tfhd;
        }

        Mp4Atom::new(*b"traf", new_data)
    }

    fn parse_tfhd(&mut self, tfhd: &Mp4Atom) {
        let data = &tfhd.data;
        if data.len() < 8 {
            return;
        }
        let flags = (u32::from_be_bytes(data[0..4].try_into().unwrap_or([0; 4]))) & 0xFFFFFF;
        let mut offset = 8; // skip version_flags(4) + track_id(4)

        if flags & 0x000001 != 0 {
            offset += 8;
        } // base-data-offset
        if flags & 0x000002 != 0 {
            offset += 4;
        } // sample-description-index
        if flags & 0x000008 != 0 {
            offset += 4;
        } // default-sample-duration
        if flags & 0x000010 != 0 && offset + 4 <= data.len() {
            self.default_sample_size =
                u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap_or([0; 4]));
        }
    }

    fn parse_senc(&self, senc: &Mp4Atom, _sample_count: u32) -> Vec<SampleInfo> {
        let data = &senc.data;
        if data.len() < 8 {
            return Vec::new();
        }
        let version_flags = u32::from_be_bytes(data[0..4].try_into().unwrap_or([0; 4]));
        let flags = version_flags & 0xFFFFFF;
        let mut pos = 4;

        // sample count from senc itself
        if data.len() < pos + 4 {
            return Vec::new();
        }
        let sc = u32::from_be_bytes(data[pos..pos + 4].try_into().unwrap_or([0; 4]));
        pos += 4;

        let iv_size = self.default_iv_size;
        let use_constant_iv = self.cbcs && self.constant_iv.is_some();

        let mut samples = Vec::new();
        for _ in 0..sc {
            let iv = if use_constant_iv {
                self.constant_iv.clone().unwrap_or_default()
            } else {
                if pos + iv_size > data.len() {
                    break;
                }
                let iv_bytes = data[pos..pos + iv_size].to_vec();
                pos += iv_size;
                iv_bytes
            };

            let mut sub_samples = Vec::new();
            if flags & 0x000002 != 0 && pos + 2 <= data.len() {
                let ss_count = u16::from_be_bytes(data[pos..pos + 2].try_into().unwrap_or([0; 2]));
                pos += 2;
                for _ in 0..ss_count {
                    if pos + 6 <= data.len() {
                        let clear =
                            u16::from_be_bytes(data[pos..pos + 2].try_into().unwrap_or([0; 2]));
                        let enc =
                            u32::from_be_bytes(data[pos + 2..pos + 6].try_into().unwrap_or([0; 4]));
                        sub_samples.push((clear, enc));
                        pos += 6;
                    }
                }
            }

            samples.push(SampleInfo { iv, sub_samples });
        }
        samples
    }

    fn process_trun(&mut self, trun: &Mp4Atom) -> (u32, i32) {
        let data = &trun.data;
        if data.len() < 8 {
            return (0, 0);
        }
        let flags = u32::from_be_bytes(data[0..4].try_into().unwrap_or([0; 4]));
        let sample_count = u32::from_be_bytes(data[4..8].try_into().unwrap_or([0; 4]));
        let mut parse_offset = 8usize;

        let mut data_offset = 0i32;
        if flags & 0x000001 != 0 {
            if parse_offset + 4 <= data.len() {
                data_offset = i32::from_be_bytes(
                    data[parse_offset..parse_offset + 4]
                        .try_into()
                        .unwrap_or([0; 4]),
                );
            }
            parse_offset += 4;
        }
        if flags & 0x000004 != 0 {
            parse_offset += 4; // first-sample-flags
        }

        self.trun_sample_sizes.clear();
        for _ in 0..sample_count {
            if flags & 0x000100 != 0 {
                parse_offset += 4;
            } // sample-duration
            let size = if flags & 0x000200 != 0 {
                if parse_offset + 4 <= data.len() {
                    let s = u32::from_be_bytes(
                        data[parse_offset..parse_offset + 4]
                            .try_into()
                            .unwrap_or([0; 4]),
                    );
                    parse_offset += 4;
                    s
                } else {
                    parse_offset += 4;
                    0
                }
            } else {
                0
            };
            self.trun_sample_sizes.push(size);
            if flags & 0x000400 != 0 {
                parse_offset += 4;
            } // sample-flags
            if flags & 0x000800 != 0 {
                parse_offset += 4;
            } // composition-time-offset
        }

        (sample_count, data_offset)
    }

    fn modify_trun(&self, trun: &Mp4Atom) -> Mp4Atom {
        let mut data = trun.data.clone();
        if data.len() < 12 {
            return trun.clone();
        }
        let flags = (u32::from_be_bytes(data[0..4].try_into().unwrap_or([0; 4]))) & 0xFFFFFF;
        if flags & 0x000001 != 0 {
            let current = i32::from_be_bytes(data[8..12].try_into().unwrap_or([0; 4]));
            let new_offset = current - self.total_encryption_overhead as i32;
            data[8..12].copy_from_slice(&new_offset.to_be_bytes());
        }
        Mp4Atom::new(*b"trun", data)
    }

    fn process_sidx(&self, sidx: &Mp4Atom) -> Mp4Atom {
        let mut data = sidx.data.clone();
        if data.len() >= 36 {
            let current = u32::from_be_bytes(data[32..36].try_into().unwrap_or([0; 4]));
            let ref_type = current >> 31;
            let referenced_size =
                (current & 0x7FFFFFFF).saturating_sub(self.total_encryption_overhead as u32);
            let new_val = (ref_type << 31) | referenced_size;
            data[32..36].copy_from_slice(&new_val.to_be_bytes());
        }
        Mp4Atom::new(*b"sidx", data)
    }

    // -----------------------------------------------------------------------
    // mdat decryption
    // -----------------------------------------------------------------------

    fn decrypt_mdat(&self, mdat: &Mp4Atom) -> Mp4Atom {
        if self.track_infos.is_empty() {
            return mdat.clone();
        }
        let mdat_data = &mdat.data;
        let mut decrypted = mdat_data.to_vec();

        // Sort tracks by data_offset
        let mut sorted_tracks = self.track_infos.clone();
        sorted_tracks.sort_by_key(|t| t.data_offset);

        let first_data_offset = sorted_tracks[0].data_offset;

        for track in &sorted_tracks {
            let Some(ref key) = track.key else { continue };
            if track.sample_info.is_empty() {
                continue;
            }

            let mdat_pos_start = (track.data_offset - first_data_offset).max(0) as usize;
            let mut pos = mdat_pos_start;

            for (i, info) in track.sample_info.iter().enumerate() {
                let mut size = if i < track.sample_sizes.len() {
                    track.sample_sizes[i] as usize
                } else {
                    0
                };
                if size == 0 {
                    size = if track.default_sample_size > 0 {
                        track.default_sample_size as usize
                    } else {
                        continue;
                    };
                }
                if pos + size > mdat_data.len() {
                    break;
                }

                let sample = &mdat_data[pos..pos + size];
                let dec = if self.cbcs {
                    decrypt_sample_cbcs(
                        sample,
                        info,
                        key,
                        track.crypt_byte_block,
                        track.skip_byte_block,
                        track.constant_iv.as_deref(),
                    )
                } else {
                    decrypt_sample_cenc(sample, info, key)
                };
                decrypted[pos..pos + dec.len().min(size)]
                    .copy_from_slice(&dec[..dec.len().min(size)]);
                pos += size;
            }
        }

        Mp4Atom::new(*b"mdat", decrypted)
    }

    // -----------------------------------------------------------------------
    // Key lookup
    // -----------------------------------------------------------------------

    fn get_key_for_track(&self, track_id: u32) -> Option<&Vec<u8>> {
        if let Some(kid) = self.extracted_kids.get(&track_id) {
            let all_zero = kid.iter().all(|&b| b == 0) && kid.len() == 16;
            if !all_zero {
                // Direct lookup: tenc KID matches a provided key_id byte-for-byte
                if let Some(k) = self.key_map.get(kid.as_slice()) {
                    return Some(k);
                }

                // PlayReady GUID fallback: some content packagers store the KID in
                // the tenc box using little-endian byte order for the first three UUID
                // components (PlayReady GUID format), while the MPD advertises
                // @cenc:default_KID in standard big-endian UUID order.
                //
                // UUID:    AABBCCDD-EEFF-GGHH-II...
                // LE GUID: DDCCBBAA-FFEE-HHGG-II...
                if kid.len() == 16 {
                    let mut swapped = kid.clone();
                    swapped[0..4].reverse(); // bytes 0-3
                    swapped[4..6].reverse(); // bytes 4-5
                    swapped[6..8].reverse(); // bytes 6-7
                                             // bytes 8-15 unchanged
                    if let Some(k) = self.key_map.get(swapped.as_slice()) {
                        return Some(k);
                    }
                }
            }
        }
        // Fallback: single key
        if self.key_map.len() == 1 {
            return self.key_map.values().next();
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Sample decryption (free functions)
// ---------------------------------------------------------------------------

/// AES-CTR (cenc). The keystream runs on across the protected ranges
/// of a sample byte for byte: a range that ends mid-block is continued by the
/// next one, so the ranges are decrypted as one stream.
fn decrypt_sample_cenc(sample: &[u8], info: &SampleInfo, key: &[u8]) -> Vec<u8> {
    let mut iv = [0u8; 16];
    let copy_len = info.iv.len().min(16);
    iv[..copy_len].copy_from_slice(&info.iv[..copy_len]);

    if info.sub_samples.is_empty() {
        return aes_ctr_decrypt(key, &iv, sample);
    }

    let mut ranges = Vec::with_capacity(info.sub_samples.len());
    let mut pos = 0usize;
    for &(clear, enc) in &info.sub_samples {
        let start = (pos + clear as usize).min(sample.len());
        let end = (start + enc as usize).min(sample.len());
        ranges.push(start..end);
        pos = end;
    }

    let protected: Vec<u8> = ranges
        .iter()
        .flat_map(|r| sample[r.clone()].iter().copied())
        .collect();
    let mut plain = aes_ctr_decrypt(key, &iv, &protected).into_iter();
    let mut result = sample.to_vec();
    for range in ranges {
        for (byte, p) in result[range].iter_mut().zip(&mut plain) {
            *byte = p;
        }
    }
    result
}

fn aes_ctr_decrypt(key: &[u8], iv: &[u8; 16], data: &[u8]) -> Vec<u8> {
    use aes::cipher::{KeyIvInit, StreamCipher};

    let mut buf = data.to_vec();
    if let Ok(mut cipher) = ctr::Ctr128BE::<aes::Aes128>::new_from_slices(key, iv) {
        cipher.apply_keystream(&mut buf);
    }
    buf
}

/// AES-CBC with pattern encryption (cbcs).
fn decrypt_sample_cbcs(
    sample: &[u8],
    info: &SampleInfo,
    key: &[u8],
    crypt_byte_block: u8,
    skip_byte_block: u8,
    constant_iv: Option<&[u8]>,
) -> Vec<u8> {
    let mut iv = [0u8; 16];
    if let Some(civ) = constant_iv {
        let copy_len = civ.len().min(16);
        iv[..copy_len].copy_from_slice(&civ[..copy_len]);
    } else {
        let copy_len = info.iv.len().min(16);
        iv[..copy_len].copy_from_slice(&info.iv[..copy_len]);
    }

    if info.sub_samples.is_empty() {
        return decrypt_cbcs_pattern(sample, key, &iv, crypt_byte_block, skip_byte_block);
    }

    let mut result = Vec::with_capacity(sample.len());
    let mut pos = 0usize;

    for &(clear, enc) in &info.sub_samples {
        let end = (pos + clear as usize).min(sample.len());
        result.extend_from_slice(&sample[pos..end]);
        pos += clear as usize;

        let enc_end = (pos + enc as usize).min(sample.len());
        let enc_data = &sample[pos..enc_end];
        result.extend_from_slice(&decrypt_cbcs_pattern(
            enc_data,
            key,
            &iv,
            crypt_byte_block,
            skip_byte_block,
        ));
        pos += enc as usize;
    }
    if pos < sample.len() {
        result.extend_from_slice(&sample[pos..]);
    }
    result
}

/// CBCS pattern decryption: crypt N blocks, skip M blocks, repeat.
fn decrypt_cbcs_pattern(
    data: &[u8],
    key: &[u8],
    iv: &[u8; 16],
    crypt_blocks: u8,
    skip_blocks: u8,
) -> Vec<u8> {
    if data.is_empty() || crypt_blocks == 0 {
        return data.to_vec();
    }

    let block_size = 16usize;
    let crypt_bytes = crypt_blocks as usize * block_size;
    let skip_bytes = skip_blocks as usize * block_size;

    // If skip=0, full block encryption
    if skip_bytes == 0 {
        return aes_cbc_decrypt_full_blocks(key, iv, data);
    }

    // Collect all encrypted blocks in order, tracking their positions
    let mut encrypted_blocks = Vec::new();
    let mut block_positions: Vec<(usize, usize)> = Vec::new(); // (orig_offset, length)
    let mut pos = 0usize;

    while pos < data.len() {
        // Encrypted portion
        if pos + crypt_bytes <= data.len() {
            encrypted_blocks.extend_from_slice(&data[pos..pos + crypt_bytes]);
            block_positions.push((pos, crypt_bytes));
            pos += crypt_bytes;
        } else {
            let remaining = data.len() - pos;
            let complete = (remaining / block_size) * block_size;
            if complete > 0 {
                encrypted_blocks.extend_from_slice(&data[pos..pos + complete]);
                block_positions.push((pos, complete));
            }
            break;
        }

        // Skip portion
        if pos + skip_bytes <= data.len() {
            pos += skip_bytes;
        } else {
            break;
        }
    }

    if encrypted_blocks.is_empty() {
        return data.to_vec();
    }

    // Decrypt all encrypted blocks as a continuous CBC stream
    let decrypted_blocks = aes_cbc_decrypt_full_blocks(key, iv, &encrypted_blocks);

    // Reconstruct output
    let mut result = data.to_vec();
    let mut dec_pos = 0usize;
    for (orig_pos, length) in &block_positions {
        result[*orig_pos..*orig_pos + length]
            .copy_from_slice(&decrypted_blocks[dec_pos..dec_pos + length]);
        dec_pos += length;
    }
    result
}

/// Decrypt complete 16-byte blocks with AES-CBC (leave trailing partial block unchanged).
fn aes_cbc_decrypt_full_blocks(key: &[u8], iv: &[u8; 16], data: &[u8]) -> Vec<u8> {
    use aes::cipher::{block_padding::NoPadding, BlockDecryptMut, KeyIvInit};

    let complete = (data.len() / 16) * 16;
    if complete == 0 {
        return data.to_vec();
    }

    let mut result = data.to_vec();
    if let Ok(cipher) = cbc::Decryptor::<aes::Aes128>::new_from_slices(key, iv) {
        // Decrypts in place.
        let _ = cipher.decrypt_padded_mut::<NoPadding>(&mut result[..complete]);
    }
    result
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Decrypt a media segment. `keys` is `kid:key[,kid:key]` in hex.
pub fn decrypt_segment(init: &[u8], segment: &[u8], keys: &str) -> Result<Vec<u8>, String> {
    Ok(Mp4Decrypter::new(key_map(keys)?).decrypt_segment(init, segment))
}

/// Strip the protection boxes from an init segment.
pub fn decrypt_init(init: &[u8], keys: &str) -> Result<Vec<u8>, String> {
    Ok(Mp4Decrypter::new(key_map(keys)?).process_init_only(init))
}

/// `kid:key[,kid:key]` hex pairs (dashes allowed) as a `KID → key` map.
fn key_map(keys: &str) -> Result<HashMap<Vec<u8>, Vec<u8>>, String> {
    let map: HashMap<_, _> = keys
        .split(',')
        .filter_map(|pair| {
            let (kid, key) = pair.trim().split_once(':')?;
            let kid = hex::decode(kid.replace('-', "")).ok()?;
            let key = hex::decode(key.replace('-', "")).ok()?;
            Some((kid, key))
        })
        .collect();
    if map.is_empty() {
        return Err("No valid kid:key pairs provided".to_string());
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::{KeyIvInit, StreamCipher};

    #[test]
    fn cenc_keystream_continues_across_unaligned_subsamples() {
        let key = [7u8; 16];
        let iv8 = [1, 2, 3, 4, 5, 6, 7, 8];
        let plain: Vec<u8> = (0..100u8).collect();
        let sub_samples = vec![(5u16, 20u32), (3, 41), (2, 29)];

        // Encrypt: one CTR keystream over the protected bytes only.
        let mut iv = [0u8; 16];
        iv[..8].copy_from_slice(&iv8);
        let mut keystream = vec![0u8; 90];
        ctr::Ctr128BE::<aes::Aes128>::new(&key.into(), &iv.into()).apply_keystream(&mut keystream);
        let mut ks = keystream.iter();
        let mut cipher = plain.clone();
        let mut pos = 0;
        for &(clear, enc) in &sub_samples {
            pos += clear as usize;
            for b in &mut cipher[pos..pos + enc as usize] {
                *b ^= ks.next().unwrap();
            }
            pos += enc as usize;
        }

        let info = SampleInfo {
            iv: iv8.to_vec(),
            sub_samples,
        };
        assert_eq!(decrypt_sample_cenc(&cipher, &info, &key), plain);
    }

    fn atom(t: &[u8; 4], data: Vec<u8>) -> Mp4Atom {
        Mp4Atom::new(*t, data)
    }

    #[test]
    fn moof_loses_pssh_and_seig_groups_and_offsets_follow() {
        let tfhd = atom(b"tfhd", [0u32.to_be_bytes(), 1u32.to_be_bytes()].concat());
        // trun: flags = data-offset present, 0 samples, data_offset = 500
        let trun = atom(
            b"trun",
            [1u32.to_be_bytes(), 0u32.to_be_bytes(), 500i32.to_be_bytes()].concat(),
        );
        let sgpd = atom(b"sgpd", [&[1u8, 0, 0, 0][..], b"seig", &[0; 8]].concat());
        let roll = atom(b"sgpd", [&[1u8, 0, 0, 0][..], b"roll", &[0; 8]].concat());
        let traf = atom(
            b"traf",
            [tfhd.pack(), trun.pack(), sgpd.pack(), roll.pack()].concat(),
        );
        let pssh = atom(b"pssh", vec![0; 24]);
        let moof = atom(b"moof", [pssh.pack(), traf.pack()].concat());

        let out = Mp4Decrypter::new(HashMap::new()).process_moof(&moof);

        let children = Mp4Parser::children_of(&out.data);
        assert_eq!(children.len(), 1, "pssh must be gone");
        let traf_children = Mp4Parser::children_of(&children[0].data);
        let types: Vec<_> = traf_children.iter().map(|a| a.atom_type).collect();
        assert_eq!(
            types,
            vec![*b"tfhd", *b"trun", *b"sgpd"],
            "only the seig group goes"
        );
        let removed = (24 + 8) + (sgpd.data.len() + 8) as i32;
        let offset = i32::from_be_bytes(traf_children[1].data[8..12].try_into().unwrap());
        assert_eq!(offset, 500 - removed);
    }
}
