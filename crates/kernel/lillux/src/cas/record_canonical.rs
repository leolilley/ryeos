//! Fixed-storage canonicalization for integer-valued admitted record schemas.
//! Input Serialize implementations and their own allocations remain the caller's
//! responsibility. This mechanism owns exactly raw bytes, span nodes and output
//! bytes; it never constructs a serde_json::Value or per-object/key allocation.
use anyhow::{Result, bail, ensure};
use std::{cmp::Ordering, io::Write};
use sha2::{Digest, Sha256};

const NONE: usize = usize::MAX;
const MAX_DEPTH: usize = 128;
#[derive(Clone, Copy)]
enum Kind { Scalar, String, Array, Object }
#[derive(Clone, Copy)]
struct Node<'raw> { kind: Kind, start: usize, end: usize, first: usize, next: usize, text: Option<&'raw str> }
const EMPTY: Node<'static> = Node { kind: Kind::Scalar, start: 0, end: 0, first: NONE, next: NONE, text: None };

/// Allocation dimensions only, never funding or execution authority.
#[derive(Debug, Clone, Copy)]
pub struct CanonicalRecordStorageQuote {
    raw_bytes: usize,
    node_count: usize,
    output_bytes: usize,
    working_bytes: u64,
    work_steps: u64,
    raw_sha256: [u8; 32],
}
impl CanonicalRecordStorageQuote {
    pub fn raw_bytes(&self) -> usize { self.raw_bytes }
    pub fn node_count(&self) -> usize { self.node_count }
    pub fn output_bytes(&self) -> usize { self.output_bytes }
    pub fn working_bytes(&self) -> u64 { self.working_bytes }
    /// Bound in byte/index traversal units, not CPU instructions or duration.
    /// Caller-provided Serialize and cryptographic work are separate contracts.
    pub fn work_steps(&self) -> u64 { self.work_steps }
}
struct Count { length: usize, maximum: usize, hash: Sha256, nodes: usize, in_string: bool, escaped: bool, scalar: bool }
impl Write for Count {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.maximum.saturating_sub(self.length) {
            return Err(std::io::Error::from(std::io::ErrorKind::WriteZero));
        }
        self.length += bytes.len();
        self.hash.update(bytes);
        // Count lexical value/key starts across writer chunks. serde_json's
        // serializer supplies valid JSON; quoted punctuation/escapes never
        // create nodes. The structural parser later checks the exact count.
        for &byte in bytes {
            if self.in_string {
                if self.escaped { self.escaped = false; }
                else if byte == b'\\' { self.escaped = true; }
                else if byte == b'"' { self.in_string = false; }
            } else {
                match byte {
                    b'[' | b'{' => { self.nodes += 1; self.scalar = false; }
                    b'"' => { self.nodes += 1; self.in_string = true; self.scalar = false; }
                    b',' | b']' | b'}' | b':' | b' ' | b'\r' | b'\n' | b'\t' => self.scalar = false,
                    _ => { if !self.scalar { self.nodes += 1; self.scalar = true; } }
                }
            }
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}
struct Buffer<'a> { bytes: &'a mut [u8], used: usize }
impl Buffer<'_> {
    fn append(&mut self, bytes: &[u8]) -> Result<()> {
        ensure!(bytes.len() <= self.bytes.len().saturating_sub(self.used), "canonical record capacity exceeded");
        self.bytes[self.used..self.used + bytes.len()].copy_from_slice(bytes);
        self.used += bytes.len();
        Ok(())
    }
}
impl Write for Buffer<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.bytes.len().saturating_sub(self.used) {
            return Err(std::io::Error::from(std::io::ErrorKind::WriteZero));
        }
        self.bytes[self.used..self.used + bytes.len()].copy_from_slice(bytes);
        self.used += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}
fn byte_slab(length: usize) -> Box<[u8]> {
    let mut slab = Box::<[u8]>::new_uninit_slice(length);
    for slot in slab.iter_mut() { slot.write(0); }
    // SAFETY: every u8 slot was initialized exactly once above.
    unsafe { slab.assume_init() }
}
fn node_slab<'raw>(length: usize) -> Box<[Node<'raw>]> {
    let mut slab = Box::<[Node<'raw>]>::new_uninit_slice(length);
    for slot in slab.iter_mut() { slot.write(EMPTY); }
    // SAFETY: every Node slot was initialized exactly once above.
    unsafe { slab.assume_init() }
}

/// Fund the bounded counting/hash pass before examining a derived record.
/// Units cover bytes accepted, token scanning and hashing input bytes; custom
/// Serialize implementations remain outside this mechanism's work contract.
pub fn canonical_record_quote_work_bound(maximum: usize) -> Result<u64> {
    u64::try_from(maximum)?.checked_mul(3)
        .ok_or_else(|| anyhow::anyhow!("canonical quote work overflow"))
}
/// Count an immutable derived record without materializing it. The caller funds
/// this Serialize traversal first and must preserve the SAME input through use.
pub fn canonical_record_storage_quote<T: serde::Serialize>(record: &T, maximum: usize) -> Result<CanonicalRecordStorageQuote> {
    ensure!(maximum > 0, "empty canonical record capacity");
    let mut count = Count { length: 0, maximum, hash: Sha256::new(), nodes: 0, in_string: false, escaped: false, scalar: false };
    serde_json::to_writer(&mut count, record)?;
    std::alloc::Layout::array::<Node<'_>>(count.nodes)?;
    std::alloc::Layout::array::<u8>(maximum)?;
    let working = count.nodes.checked_mul(std::mem::size_of::<Node<'_>>())
        .and_then(|n| n.checked_add(count.length))
        .and_then(|n| n.checked_add(maximum))
        .ok_or_else(|| anyhow::anyhow!("canonical record storage overflow"))?;
    // At most ceil(log2(nodes)) merge levels. Each comparison is charged
    // to the key removed from that merge; decoded common-prefix visits are
    // bounded by that key's encoded length. Strings keep validated borrowed
    // UTF-8 spans, so comparisons never rescan the other entire long key.
    // Eight visits per byte/index cover serialization/hash, initialization,
    // parsing/decoding, linked-list splitting/merging and canonical emission.
    let levels = if count.nodes <= 1 { 0 } else { usize::BITS - (count.nodes - 1).leading_zeros() };
    let work = u64::try_from(count.length)?.checked_add(u64::try_from(count.nodes)?)
        .and_then(|n| n.checked_mul(u64::from(levels) + 1))
        .and_then(|n| n.checked_add(u64::try_from(maximum).ok()?))
        .and_then(|n| n.checked_mul(8))
        .ok_or_else(|| anyhow::anyhow!("canonical record traversal work overflow"))?;
    Ok(CanonicalRecordStorageQuote { raw_bytes: count.length, node_count: count.nodes, output_bytes: maximum,
        working_bytes: u64::try_from(working)?, work_steps: work, raw_sha256: count.hash.finalize().into() })
}

/// Use the exact previously funded dimensions. The caller prepays all three
/// payload slabs plus its control/stack/Serialize work before entry. A changed
/// input refuses rather than growing any slab. No floating-point values are
/// accepted by this integer-record contract; unrelated generic JSON uses keep
/// their own canonicalizer. Returned Vec retains the fixed output capacity.
pub fn canonical_record_bytes<T: serde::Serialize>(record: &T, quote: &CanonicalRecordStorageQuote) -> Result<Vec<u8>> {
    let mut raw = byte_slab(quote.raw_bytes);
    let used = {
        let mut writer = Buffer { bytes: &mut raw, used: 0 };
        serde_json::to_writer(&mut writer, record)?;
        writer.used
    };
    ensure!(used == quote.raw_bytes && <[u8; 32]>::from(Sha256::digest(&raw[..used])) == quote.raw_sha256,
        "canonical record changed after storage quote");
    let mut nodes = node_slab(quote.node_count);
    let mut output = byte_slab(quote.output_bytes);
    let output_used = {
        let mut tree = Tree { raw: &raw, nodes: &mut nodes, used: 0, at: 0 };
        let root = tree.parse(0)?;
        tree.space();
        ensure!(tree.at == raw.len() && tree.used == quote.node_count,
            "canonical record span census differs from quoted snapshot");
        let mut writer = Buffer { bytes: &mut output, used: 0 };
        tree.emit(root, &mut writer, 0)?;
        writer.used
    };
    // into_vec transfers this SAME output allocation; truncate never grows it.
    let mut bytes = output.into_vec();
    bytes.truncate(output_used);
    Ok(bytes)
}
struct Tree<'raw, 'nodes> { raw: &'raw [u8], nodes: &'nodes mut [Node<'raw>], used: usize, at: usize }
impl Tree<'_, '_> {
    fn space(&mut self) { while self.at < self.raw.len() && matches!(self.raw[self.at], b' ' | b'\r' | b'\n' | b'\t') { self.at += 1; } }
    fn node(&mut self, kind: Kind, start: usize) -> Result<usize> {
        ensure!(self.used < self.nodes.len(), "canonical span table exhausted");
        let index = self.used;
        self.used += 1;
        self.nodes[index] = Node { kind, start, ..EMPTY };
        Ok(index)
    }
    fn punctuation(&mut self, byte: u8) -> Result<()> {
        self.space();
        ensure!(self.raw.get(self.at) == Some(&byte), "invalid canonical record punctuation");
        self.at += 1;
        Ok(())
    }
    fn string(&mut self) -> Result<usize> {
        self.space();
        self.punctuation(b'"')?;
        let start = self.at;
        while let Some(&byte) = self.raw.get(self.at) {
            match byte {
                b'"' => {
                    let index = self.node(Kind::String, start)?;
                    self.nodes[index].end = self.at;
                    self.at += 1;
                    let text = std::str::from_utf8(&self.raw[start..self.nodes[index].end])?;
                    for character in (Characters { source: text.chars() }) { character?; }
                    self.nodes[index].text = Some(text);
                    return Ok(index);
                }
                b'\\' => { self.at += 1; ensure!(self.at < self.raw.len(), "truncated canonical string"); }
                0..=31 => bail!("unescaped control in canonical string"),
                _ => {}
            }
            self.at += 1;
        }
        bail!("unterminated canonical string")
    }
    fn parse(&mut self, depth: usize) -> Result<usize> {
        ensure!(depth < MAX_DEPTH, "canonical record depth exceeded");
        self.space();
        let start = self.at;
        match self.raw.get(self.at).copied() {
            Some(b'"') => self.string(),
            Some(open @ (b'[' | b'{')) => {
                self.at += 1;
                let object = open == b'{';
                let close = if object { b'}' } else { b']' };
                let index = self.node(if object { Kind::Object } else { Kind::Array }, start)?;
                let mut tail = NONE;
                self.space();
                if self.raw.get(self.at) != Some(&close) {
                    loop {
                        let child = if object {
                            let key = self.string()?;
                            self.punctuation(b':')?;
                            self.nodes[key].first = self.parse(depth + 1)?;
                            key
                        } else { self.parse(depth + 1)? };
                        if tail == NONE { self.nodes[index].first = child; } else { self.nodes[tail].next = child; }
                        tail = child;
                        self.space();
                        if self.raw.get(self.at) == Some(&close) { break; }
                        self.punctuation(b',')?;
                    }
                }
                self.punctuation(close)?;
                self.nodes[index].end = self.at;
                Ok(index)
            }
            Some(_) => {
                while self.at < self.raw.len() && !matches!(self.raw[self.at], b',' | b']' | b'}' | b' ' | b'\r' | b'\n' | b'\t') { self.at += 1; }
                let token = &self.raw[start..self.at];
                let text = std::str::from_utf8(token)?;
                ensure!(matches!(token, b"null" | b"true" | b"false") || valid_integer(text), "canonical record requires integer-valued JSON");
                let index = self.node(Kind::Scalar, start)?;
                self.nodes[index].end = self.at;
                Ok(index)
            }
            None => bail!("missing canonical record value"),
        }
    }
    fn compare_keys(&self, left: usize, right: usize) -> Result<Ordering> {
        let a = self.nodes[left]; let b = self.nodes[right];
        // UTF-8 was validated ONCE when each string span was parsed. Avoid
        // rescanning a long key on every merge comparison with shorter keys.
        let mut a = Characters { source: a.text.ok_or_else(|| anyhow::anyhow!("missing canonical key span"))?.chars() };
        let mut b = Characters { source: b.text.ok_or_else(|| anyhow::anyhow!("missing canonical key span"))?.chars() };
        loop {
            match (a.next().transpose()?, b.next().transpose()?) {
                (Some(a), Some(b)) if a == b => {},
                (Some(a), Some(b)) => return Ok(a.cmp(&b)),
                (None, None) => return Ok(Ordering::Equal),
                (None, Some(_)) => return Ok(Ordering::Less),
                (Some(_), None) => return Ok(Ordering::Greater),
            }
        }
    }
    fn sort_keys(&mut self, head: usize, length: usize) -> Result<usize> {
        if length <= 1 { return Ok(head); }
        let half = length / 2;
        let mut split = head;
        for _ in 1..half { split = self.nodes[split].next; }
        let right = self.nodes[split].next;
        self.nodes[split].next = NONE;
        let mut a = self.sort_keys(head, half)?;
        let mut b = self.sort_keys(right, length - half)?;
        let mut result = NONE;
        let mut tail = NONE;
        while a != NONE || b != NONE {
            let take_a = b == NONE || (a != NONE && self.compare_keys(a, b)? != Ordering::Greater);
            let next = if take_a { let node = a; a = self.nodes[a].next; node } else { let node = b; b = self.nodes[b].next; node };
            if tail == NONE { result = next; } else { self.nodes[tail].next = next; }
            tail = next;
        }
        self.nodes[tail].next = NONE;
        Ok(result)
    }
    fn emit(&mut self, index: usize, out: &mut Buffer<'_>, depth: usize) -> Result<()> {
        ensure!(depth < MAX_DEPTH, "canonical output depth exceeded");
        let node = self.nodes[index];
        match node.kind {
            Kind::Scalar => out.append(&self.raw[node.start..node.end]),
            Kind::String => emit_string(node.text.ok_or_else(|| anyhow::anyhow!("missing canonical string span"))?, out),
            Kind::Array | Kind::Object => {
                let object = matches!(node.kind, Kind::Object);
                let mut child = node.first;
                if object && child != NONE {
                    let mut length = 0; let mut at = child;
                    while at != NONE { length += 1; at = self.nodes[at].next; }
                    child = self.sort_keys(child, length)?;
                    self.nodes[index].first = child;
                }
                out.append(if object { b"{" } else { b"[" })?;
                let mut previous = NONE;
                while child != NONE {
                    if previous != NONE { out.append(b",")?; }
                    if object {
                        if previous != NONE { ensure!(self.compare_keys(previous, child)? != Ordering::Equal, "duplicate canonical record key"); }
                        self.emit(child, out, depth + 1)?;
                        out.append(b":")?;
                        self.emit(self.nodes[child].first, out, depth + 1)?;
                    } else { self.emit(child, out, depth + 1)?; }
                    previous = child;
                    child = self.nodes[child].next;
                }
                out.append(if object { b"}" } else { b"]" })
            }
        }
    }
}
fn valid_integer(text: &str) -> bool {
    let digits = text.strip_prefix('-').unwrap_or(text);
    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
        && !(digits.len() > 1 && digits.starts_with('0')) && text != "-0"
        && (text.parse::<u64>().is_ok() || text.parse::<i64>().is_ok())
}
struct Characters<'a> { source: std::str::Chars<'a> }
impl<'a> Characters<'a> {
    fn hex(&mut self) -> Result<u32> {
        let mut value = 0;
        for _ in 0..4 { value = (value << 4) | self.source.next().and_then(|c| c.to_digit(16)).ok_or_else(|| anyhow::anyhow!("invalid canonical Unicode escape"))?; }
        Ok(value)
    }
    fn escape(&mut self) -> Result<char> {
        Ok(match self.source.next() {
            Some('"') => '"', Some('\\') => '\\', Some('/') => '/', Some('b') => '\x08', Some('f') => '\x0c', Some('n') => '\n', Some('r') => '\r', Some('t') => '\t',
            Some('u') => {
                let high = self.hex()?;
                let scalar = if (0xd800..=0xdbff).contains(&high) {
                    ensure!(self.source.next() == Some('\\') && self.source.next() == Some('u'), "missing canonical low surrogate");
                    let low = self.hex()?;
                    ensure!((0xdc00..=0xdfff).contains(&low), "invalid canonical low surrogate");
                    0x10000 + ((high - 0xd800) << 10) + (low - 0xdc00)
                } else { high };
                char::from_u32(scalar).ok_or_else(|| anyhow::anyhow!("invalid canonical Unicode scalar"))?
            }
            _ => bail!("invalid canonical string escape"),
        })
    }
}
impl Iterator for Characters<'_> {
    type Item = Result<char>;
    fn next(&mut self) -> Option<Self::Item> {
        self.source.next().map(|c| if c == '\\' { self.escape() } else { Ok(c) })
    }
}
fn emit_string(raw: &str, out: &mut Buffer<'_>) -> Result<()> {
    out.append(b"\"")?;
    for ch in (Characters { source: raw.chars() }) {
        let ch = ch?;
        match ch {
            '"' => out.append(b"\\\"")?, '\\' => out.append(b"\\\\")?, '\x08' => out.append(b"\\b")?, '\x0c' => out.append(b"\\f")?, '\n' => out.append(b"\\n")?, '\r' => out.append(b"\\r")?, '\t' => out.append(b"\\t")?,
            c if c.is_ascii() && (c as u32) >= 32 => out.append(&[c as u8])?,
            c => {
                let scalar = c as u32; let mut escape = [0; 12];
                let length = if scalar <= 0xffff { super::write_unicode_escape(scalar, &mut escape[..6]); 6 }
                    else { let offset = scalar - 0x10000; super::write_unicode_escape(0xd800 + (offset >> 10), &mut escape[..6]); super::write_unicode_escape(0xdc00 + (offset & 0x3ff), &mut escape[6..]); 12 };
                out.append(&escape[..length])?;
            }
        }
    }
    out.append(b"\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn admitted_integer_records_match_existing_canonical_format() {
        let records = [
            serde_json::json!({"z": [true, null, -9223372036854775808i64], "a": {"é": "😀", "x": "\\\"\n\u{0001}"}, "n": u64::MAX}),
            serde_json::json!({"a": {"x": [], "b": {}}, "\u{0001}": "é", "😀": "ok", "aa": 3}),
        ];
        for record in records {
            let expected = super::super::canonical_json(&record).unwrap();
            let quote = canonical_record_storage_quote(&record, expected.len()).unwrap();
            let actual = canonical_record_bytes(&record, &quote).unwrap();
            assert_eq!(actual, expected.as_bytes());
            assert_eq!(actual.capacity(), quote.output_bytes());
            assert_eq!(quote.working_bytes(), (quote.node_count() * std::mem::size_of::<Node<'_>>() + quote.raw_bytes() + quote.output_bytes()) as u64);
        }
    }
    #[test]
    fn unicode_expansion_and_changed_snapshot_refuse_without_growing_slabs() {
        let input = "é";
        let quote = canonical_record_storage_quote(&input, 8).unwrap();
        assert_eq!(canonical_record_bytes(&input, &quote).unwrap(), br#""\u00e9""#);
        let insufficient = canonical_record_storage_quote(&input, 7).unwrap();
        assert!(canonical_record_bytes(&input, &insufficient).is_err());
        let original = canonical_record_storage_quote(&"a", 8).unwrap();
        assert!(canonical_record_bytes(&"b", &original).is_err());
        let float = canonical_record_storage_quote(&1.5f64, 8).unwrap();
        assert!(canonical_record_bytes(&1.5f64, &float).is_err());
    }
}
