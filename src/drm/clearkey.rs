//! ClearKey keys.

use std::collections::HashMap;

/// Parse `kid:key[,kid:key]` hex pairs (dashes allowed) into a `KID → key` map.
pub fn parse_keys(value: &str) -> HashMap<Vec<u8>, Vec<u8>> {
    value
        .split(',')
        .filter_map(|pair| {
            let (kid, key) = pair.trim().split_once(':')?;
            let kid = hex::decode(kid.replace('-', "")).ok()?;
            let key = hex::decode(key.replace('-', "")).ok()?;
            Some((kid, key))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pairs() {
        let keys = parse_keys("0011:aabb, 22-33:ccdd,broken");
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[&vec![0x00, 0x11]], vec![0xaa, 0xbb]);
        assert_eq!(keys[&vec![0x22, 0x33]], vec![0xcc, 0xdd]);
    }
}
