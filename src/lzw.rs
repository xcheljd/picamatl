//! Strict `/LZWDecode` decoder (ISO 32000-1 §7.4.4).
//!
//! Hand-rolled rather than lopdf's `Stream::decompressed_content` because the
//! lopdf path logs a decode error and returns the partial output as `Ok`, and
//! it applies the `/DecodeParms` predictor itself — the transcode needs the
//! raw LZW payload (the predictor carries over to Flate unchanged) and a hard
//! "any doubt is a decline" answer. Everything a conforming viewer might
//! resolve differently is a decline here:
//!
//! - a code above the next free table slot, or the KwKwK code with no
//!   predecessor (the first code after a clear);
//! - a table that would grow past 4,096 entries without a clear code (viewers
//!   disagree: some stop adding entries, poppler resets the table);
//! - output past `limit` (the decompression-bomb guard — checked before every
//!   write, so a bomb never allocates more than `limit` bytes).
//!
//! A missing end-of-data code is accepted (the stream simply ends; leftover
//! bits shorter than one code are padding), and bytes after an end-of-data
//! code are ignored — both match how viewers read such streams.

const CLEAR: u16 = 256;
const EOD: u16 = 257;
const FIRST_FREE: u16 = 258;
const MAX_ENTRIES: usize = 4096;

/// Decode an LZW payload. `early_change` is `/EarlyChange` (default `true`,
/// i.e. 1): the code width grows one code early. `None` on any malformed
/// input or when the output would exceed `limit` bytes.
pub(crate) fn lzw_decode(data: &[u8], early_change: bool, limit: usize) -> Option<Vec<u8>> {
    let early = u16::from(early_change);
    // String table as prefix links: entry `c` is entry `prefix[c]` followed
    // by byte `suffix[c]`; `first[c]` is its first byte, `len[c]` its length.
    let mut prefix = [0u16; MAX_ENTRIES];
    let mut suffix = [0u8; MAX_ENTRIES];
    let mut first = [0u8; MAX_ENTRIES];
    let mut len = [0u16; MAX_ENTRIES];
    for b in 0..256u16 {
        suffix[b as usize] = b as u8;
        first[b as usize] = b as u8;
        len[b as usize] = 1;
    }

    let mut out: Vec<u8> = Vec::new();
    let mut next = FIRST_FREE;
    let mut width: u32 = 9;
    let mut prev: Option<u16> = None;
    let mut acc: u32 = 0;
    let mut acc_bits: u32 = 0;
    let mut bytes = data.iter();

    loop {
        while acc_bits < width {
            let Some(&b) = bytes.next() else {
                // No complete code left: missing EOD, trailing bits are padding.
                return Some(out);
            };
            acc = (acc << 8) | u32::from(b);
            acc_bits += 8;
        }
        acc_bits -= width;
        let code = ((acc >> acc_bits) & ((1 << width) - 1)) as u16;
        acc &= (1 << acc_bits) - 1;

        if code == CLEAR {
            next = FIRST_FREE;
            width = 9;
            prev = None;
            continue;
        }
        if code == EOD {
            return Some(out);
        }
        match prev {
            None => {
                // First code after a clear (or of the stream): must be a
                // literal byte — no table entry exists yet.
                if code >= 256 {
                    return None;
                }
            }
            Some(p) => {
                if code > next || (code > EOD && code < FIRST_FREE) {
                    return None;
                }
                if usize::from(next) >= MAX_ENTRIES {
                    return None;
                }
                // New entry: previous string + first byte of the current one
                // (for the KwKwK case `code == next`, that is prev's first).
                let f = if code == next {
                    first[p as usize]
                } else {
                    first[code as usize]
                };
                let n = next as usize;
                prefix[n] = p;
                suffix[n] = f;
                first[n] = first[p as usize];
                len[n] = len[p as usize] + 1;
                next += 1;
                if next + early >= (1 << width) && width < 12 {
                    width += 1;
                }
            }
        }
        // Emit the string for `code`, written back to front.
        let l = len[code as usize] as usize;
        if out.len().checked_add(l)? > limit {
            return None;
        }
        let start = out.len();
        out.resize(start + l, 0);
        let mut c = code;
        for i in (start..start + l).rev() {
            out[i] = suffix[c as usize];
            c = prefix[c as usize];
        }
        prev = Some(code);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Test-only reference LZW encoder (MSB-first); `eod` appends the end
    /// code. Clears when the table is full (4,096 entries).
    pub(crate) fn lzw_encode(data: &[u8], early_change: bool, eod: bool) -> Vec<u8> {
        lzw_encode_with(data, early_change, eod, 4096)
    }

    /// `clear_at`: emit a clear code once the encoder's table reaches this
    /// many entries. `u16::MAX` never clears — the encoder just stops adding
    /// entries at 4,096, which the strict decoder must refuse.
    pub(crate) fn lzw_encode_with(
        data: &[u8],
        early_change: bool,
        eod: bool,
        clear_at: u16,
    ) -> Vec<u8> {
        use std::collections::HashMap;
        // The decoder adds entry `258 + k - 1` only when it reads code
        // `k + 1`, so the width switch tracks the decoder's counter
        // (`dnext`), one behind the encoder's own (`enc_next`).
        struct St {
            w: BitWriter,
            width: u32,
            dnext: u16,
            fresh: bool,
            early: u16,
        }
        impl St {
            fn emit(&mut self, code: u16) {
                self.w.put(u32::from(code), self.width);
                if !self.fresh {
                    // Saturates in the never-clear mode, where the decoder
                    // must refuse the stream at the 4,097th entry anyway.
                    self.dnext = (self.dnext + 1).min(4096);
                    if self.dnext + self.early >= (1 << self.width) && self.width < 12 {
                        self.width += 1;
                    }
                }
                self.fresh = false;
            }
            fn clear(&mut self) {
                self.w.put(u32::from(CLEAR), self.width);
                self.width = 9;
                self.dnext = FIRST_FREE;
                self.fresh = true;
            }
        }
        let mut st = St {
            w: BitWriter::default(),
            width: 9,
            dnext: FIRST_FREE,
            fresh: true,
            early: u16::from(early_change),
        };
        let mut enc_next = FIRST_FREE;
        let mut dict: HashMap<(u16, u8), u16> = HashMap::new();
        st.clear();
        let mut cur: Option<u16> = None;
        for &b in data {
            match cur {
                None => cur = Some(u16::from(b)),
                Some(c) => {
                    if let Some(&code) = dict.get(&(c, b)) {
                        cur = Some(code);
                        continue;
                    }
                    st.emit(c);
                    if enc_next < clear_at.min(4096) {
                        dict.insert((c, b), enc_next);
                        enc_next += 1;
                    } else if clear_at != u16::MAX {
                        st.clear();
                        dict.clear();
                        enc_next = FIRST_FREE;
                    }
                    cur = Some(u16::from(b));
                }
            }
        }
        if let Some(c) = cur {
            st.emit(c);
        }
        if eod {
            let width = st.width;
            st.w.put(u32::from(EOD), width);
        }
        st.w.finish()
    }

    #[derive(Default)]
    pub(crate) struct BitWriter {
        out: Vec<u8>,
        acc: u64,
        bits: u32,
    }

    impl BitWriter {
        pub(crate) fn put(&mut self, code: u32, width: u32) {
            self.acc = (self.acc << width) | u64::from(code);
            self.bits += width;
            while self.bits >= 8 {
                self.bits -= 8;
                self.out.push((self.acc >> self.bits) as u8);
            }
        }
        pub(crate) fn finish(mut self) -> Vec<u8> {
            if self.bits > 0 {
                self.out.push((self.acc << (8 - self.bits)) as u8);
            }
            self.out
        }
    }

    const CAP: usize = 1 << 24;

    #[test]
    fn spec_example_decodes() {
        // ISO 32000-1 §7.4.4.2 example: "-----A---B" encodes to
        // 256 45 258 258 65 259 66 257 (9-bit codes).
        let mut w = BitWriter::default();
        for c in [256, 45, 258, 258, 65, 259, 66, 257] {
            w.put(c, 9);
        }
        let enc = w.finish();
        assert_eq!(enc, [0x80, 0x0B, 0x60, 0x50, 0x22, 0x0C, 0x0C, 0x85, 0x01]);
        assert_eq!(lzw_decode(&enc, true, CAP).unwrap(), b"-----A---B");
        assert_eq!(lzw_decode(&enc, false, CAP).unwrap(), b"-----A---B");
    }

    /// Pseudo-random but compressible input: enough distinct strings to
    /// fill the table through every width (9 → 12 bits).
    fn sample(n: usize) -> Vec<u8> {
        let mut x: u32 = 0x1234_5678;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                b"abcdefgh 0123\n"[(x % 14) as usize]
            })
            .collect()
    }

    #[test]
    fn round_trips_through_every_code_width_both_early_change_modes() {
        let data = sample(200_000);
        for early in [true, false] {
            let enc = lzw_encode(&data, early, true);
            assert_eq!(lzw_decode(&enc, early, CAP).unwrap(), data, "early={early}");
            // Decoding with the wrong EarlyChange must not silently agree.
            assert_ne!(lzw_decode(&enc, !early, CAP).as_deref(), Some(&data[..]));
        }
    }

    #[test]
    fn early_change_zero_switches_width_one_code_later() {
        // 254 fresh two-byte strings take next from 258 to 512 exactly:
        // with EarlyChange 1 the width moves to 10 at next == 511, with 0 at
        // next == 512. Exercise the boundary in both modes, with and without
        // the lopdf-independent reference encoder agreeing.
        let mut data = Vec::new();
        for a in 0..=255u8 {
            data.push(a);
            data.push(a.wrapping_mul(7));
        }
        for early in [true, false] {
            let enc = lzw_encode(&data, early, true);
            assert_eq!(lzw_decode(&enc, early, CAP).unwrap(), data);
        }
    }

    #[test]
    fn matches_lopdf_decoder() {
        // Differential check against lopdf's (weezl-backed) LZW decoder.
        for (n, early) in [(10, true), (5_000, true), (300_000, true), (300_000, false)] {
            let data = sample(n);
            let enc = lzw_encode(&data, early, true);
            let mut dict = lopdf::dictionary! { "Filter" => "LZWDecode" };
            if !early {
                dict.set("DecodeParms", lopdf::dictionary! { "EarlyChange" => 0 });
            }
            let stream = lopdf::Stream::new(dict, enc.clone());
            let theirs = stream.decompressed_content().unwrap();
            assert_eq!(lzw_decode(&enc, early, CAP).unwrap(), theirs);
        }
    }

    #[test]
    fn clear_code_mid_stream_resets_table() {
        let data = sample(50_000);
        let enc = lzw_encode_with(&data, true, true, 600);
        assert_eq!(lzw_decode(&enc, true, CAP).unwrap(), data);
    }

    #[test]
    fn missing_eod_is_accepted() {
        let data = sample(30_000);
        let enc = lzw_encode(&data, true, false);
        assert_eq!(lzw_decode(&enc, true, CAP).unwrap(), data);
    }

    #[test]
    fn bytes_after_eod_are_ignored() {
        let data = sample(1_000);
        let mut enc = lzw_encode(&data, true, true);
        enc.extend_from_slice(b"\xff\xff\xff garbage");
        assert_eq!(lzw_decode(&enc, true, CAP).unwrap(), data);
    }

    #[test]
    fn malformed_input_declines() {
        let code_stream = |codes: &[u32]| {
            let mut w = BitWriter::default();
            for &c in codes {
                w.put(c, 9);
            }
            w.finish()
        };
        // First code after clear is a table code.
        assert!(lzw_decode(&code_stream(&[256, 258]), true, CAP).is_none());
        // KwKwK with no predecessor.
        assert!(lzw_decode(&code_stream(&[300]), true, CAP).is_none());
        // Code beyond the next free slot.
        assert!(lzw_decode(&code_stream(&[256, 65, 300]), true, CAP).is_none());
        // Table overflow without a clear code: 4,096+ entries, never cleared.
        let enc = lzw_encode_with(&sample(400_000), true, true, u16::MAX);
        assert!(lzw_decode(&enc, true, usize::MAX).is_none());
        // Random garbage never panics.
        let mut x: u32 = 99;
        for n in 0..2_000usize {
            let junk: Vec<u8> = (0..n % 97)
                .map(|_| {
                    x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                    (x >> 16) as u8
                })
                .collect();
            let _ = lzw_decode(&junk, n % 2 == 0, 1 << 16);
        }
    }

    #[test]
    fn bomb_hits_the_output_cap() {
        // 1 MiB of one byte compresses to a few KB; with a 64 KiB cap the
        // decode is refused rather than allocating the full output.
        let data = vec![b'A'; 1 << 20];
        let enc = lzw_encode(&data, true, true);
        assert!(enc.len() < 16 * 1024, "{}", enc.len());
        assert!(lzw_decode(&enc, true, 64 * 1024).is_none());
        assert_eq!(lzw_decode(&enc, true, 1 << 20).unwrap(), data);
        assert!(lzw_decode(&enc, true, (1 << 20) - 1).is_none());
    }
}
