//! The little BSON the MongoDB checker needs: commands are encoded from
//! strings, numbers, booleans, binary and nested documents; replies are
//! decoded completely (every BSON type is understood well enough to be
//! skipped), so `hello`, `buildInfo` and SASL answers can be read.

/// A BSON value. Types the checker never looks at decode to [`Bson::Other`].
#[derive(Debug, Clone, PartialEq)]
pub enum Bson {
    Double(f64),
    String(String),
    Document(Document),
    Array(Vec<Bson>),
    /// Subtype and bytes.
    Binary(u8, Vec<u8>),
    ObjectId([u8; 12]),
    Bool(bool),
    /// Milliseconds since the epoch.
    DateTime(i64),
    Null,
    Int32(i32),
    Timestamp(u64),
    Int64(i64),
    /// Regex, decimal128, JavaScript, min/max key and the deprecated types.
    Other(u8),
}

/// A BSON document: keys in order (a command's name must come first).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Document(pub Vec<(String, Bson)>);

impl From<&str> for Bson {
    fn from(v: &str) -> Self {
        Bson::String(v.to_string())
    }
}

impl From<String> for Bson {
    fn from(v: String) -> Self {
        Bson::String(v)
    }
}

impl From<i32> for Bson {
    fn from(v: i32) -> Self {
        Bson::Int32(v)
    }
}

impl From<bool> for Bson {
    fn from(v: bool) -> Self {
        Bson::Bool(v)
    }
}

impl From<f64> for Bson {
    fn from(v: f64) -> Self {
        Bson::Double(v)
    }
}

impl From<Document> for Bson {
    fn from(v: Document) -> Self {
        Bson::Document(v)
    }
}

/// Nesting deeper than this is refused (a hostile reply can't blow the stack).
const MAX_DEPTH: usize = 64;

impl Document {
    pub fn new() -> Self {
        Document(Vec::new())
    }

    /// The document with `key` appended.
    pub fn with(mut self, key: &str, value: impl Into<Bson>) -> Self {
        self.0.push((key.to_string(), value.into()));
        self
    }

    pub fn get(&self, key: &str) -> Option<&Bson> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn str(&self, key: &str) -> Option<&str> {
        match self.get(key)? {
            Bson::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn bool(&self, key: &str) -> bool {
        matches!(self.get(key), Some(Bson::Bool(true)))
    }

    /// A number of any BSON numeric type.
    pub fn number(&self, key: &str) -> Option<f64> {
        match self.get(key)? {
            Bson::Double(v) => Some(*v),
            Bson::Int32(v) => Some(f64::from(*v)),
            Bson::Int64(v) => Some(*v as f64),
            Bson::Bool(v) => Some(f64::from(u8::from(*v))),
            _ => None,
        }
    }

    pub fn array(&self, key: &str) -> Option<&[Bson]> {
        match self.get(key)? {
            Bson::Array(a) => Some(a),
            _ => None,
        }
    }

    pub fn binary(&self, key: &str) -> Option<&[u8]> {
        match self.get(key)? {
            Bson::Binary(_, b) => Some(b),
            _ => None,
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        write_document(&mut out, self.0.iter().map(|(k, v)| (k.as_str(), v)));
        out
    }

    /// Decode the document at the start of `bytes`; returns it and its length.
    pub fn from_bytes(bytes: &[u8]) -> Result<(Document, usize), String> {
        let mut r = Reader { buf: bytes, at: 0 };
        let doc = r.document(0)?;
        Ok((doc, r.at))
    }
}

fn write_cstring(out: &mut Vec<u8>, s: &str) {
    // Keys never contain NUL here; a stray one would end the key early.
    out.extend(s.bytes().take_while(|b| *b != 0));
    out.push(0);
}

fn write_document<'a>(out: &mut Vec<u8>, items: impl Iterator<Item = (&'a str, &'a Bson)>) {
    let start = out.len();
    out.extend_from_slice(&[0; 4]);
    for (key, value) in items {
        out.push(type_byte(value));
        write_cstring(out, key);
        write_value(out, value);
    }
    out.push(0);
    let len = (out.len() - start) as i32;
    out[start..start + 4].copy_from_slice(&len.to_le_bytes());
}

fn type_byte(v: &Bson) -> u8 {
    match v {
        Bson::Double(_) => 0x01,
        Bson::String(_) => 0x02,
        Bson::Document(_) => 0x03,
        Bson::Array(_) => 0x04,
        Bson::Binary(..) => 0x05,
        Bson::ObjectId(_) => 0x07,
        Bson::Bool(_) => 0x08,
        Bson::DateTime(_) => 0x09,
        Bson::Null | Bson::Other(_) => 0x0A,
        Bson::Int32(_) => 0x10,
        Bson::Timestamp(_) => 0x11,
        Bson::Int64(_) => 0x12,
    }
}

fn write_value(out: &mut Vec<u8>, v: &Bson) {
    match v {
        Bson::Double(f) => out.extend_from_slice(&f.to_le_bytes()),
        Bson::String(s) => {
            out.extend_from_slice(&((s.len() + 1) as i32).to_le_bytes());
            out.extend_from_slice(s.as_bytes());
            out.push(0);
        }
        Bson::Document(d) => write_document(out, d.0.iter().map(|(k, v)| (k.as_str(), v))),
        Bson::Array(items) => {
            let keys: Vec<String> = (0..items.len()).map(|i| i.to_string()).collect();
            write_document(out, keys.iter().map(String::as_str).zip(items));
        }
        Bson::Binary(subtype, bytes) => {
            out.extend_from_slice(&(bytes.len() as i32).to_le_bytes());
            out.push(*subtype);
            out.extend_from_slice(bytes);
        }
        Bson::ObjectId(id) => out.extend_from_slice(id),
        Bson::Bool(b) => out.push(u8::from(*b)),
        Bson::DateTime(v) | Bson::Int64(v) => out.extend_from_slice(&v.to_le_bytes()),
        Bson::Null | Bson::Other(_) => {}
        Bson::Int32(v) => out.extend_from_slice(&v.to_le_bytes()),
        Bson::Timestamp(v) => out.extend_from_slice(&v.to_le_bytes()),
    }
}

struct Reader<'a> {
    buf: &'a [u8],
    at: usize,
}

const BAD: &str = "Not a MongoDB server (malformed BSON)";

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let out = self.buf.get(self.at..self.at.checked_add(n).ok_or(BAD)?).ok_or(BAD)?;
        self.at += n;
        Ok(out)
    }

    fn i32(&mut self) -> Result<i32, String> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().map_err(|_| BAD)?))
    }

    fn i64(&mut self) -> Result<i64, String> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().map_err(|_| BAD)?))
    }

    fn len(&mut self, min: i32) -> Result<usize, String> {
        let n = self.i32()?;
        if n < min {
            return Err(BAD.into());
        }
        Ok(n as usize)
    }

    fn cstring(&mut self) -> Result<String, String> {
        let rest = self.buf.get(self.at..).ok_or(BAD)?;
        let end = rest.iter().position(|b| *b == 0).ok_or(BAD)?;
        let s = String::from_utf8_lossy(&rest[..end]).into_owned();
        self.at += end + 1;
        Ok(s)
    }

    fn string(&mut self) -> Result<String, String> {
        let len = self.len(1)?;
        let bytes = self.take(len)?;
        if bytes[len - 1] != 0 {
            return Err(BAD.into());
        }
        Ok(String::from_utf8_lossy(&bytes[..len - 1]).into_owned())
    }

    fn document(&mut self, depth: usize) -> Result<Document, String> {
        if depth > MAX_DEPTH {
            return Err("Not a MongoDB server (BSON nested too deeply)".into());
        }
        let start = self.at;
        let len = self.len(5)?;
        let end = start.checked_add(len).filter(|e| *e <= self.buf.len()).ok_or(BAD)?;
        let mut doc = Document::new();
        loop {
            let kind = *self.take(1)?.first().ok_or(BAD)?;
            if kind == 0 {
                break;
            }
            let key = self.cstring()?;
            let value = self.value(kind, depth)?;
            doc.0.push((key, value));
            if self.at >= end {
                return Err(BAD.into());
            }
        }
        if self.at != end {
            return Err(BAD.into());
        }
        Ok(doc)
    }

    fn value(&mut self, kind: u8, depth: usize) -> Result<Bson, String> {
        Ok(match kind {
            0x01 => Bson::Double(f64::from_le_bytes(self.take(8)?.try_into().map_err(|_| BAD)?)),
            0x02 => Bson::String(self.string()?),
            0x03 => Bson::Document(self.document(depth + 1)?),
            0x04 => Bson::Array(self.document(depth + 1)?.0.into_iter().map(|(_, v)| v).collect()),
            0x05 => {
                let len = self.len(0)?;
                let subtype = self.take(1)?[0];
                Bson::Binary(subtype, self.take(len)?.to_vec())
            }
            0x0A => Bson::Null,
            0x06 | 0x7F | 0xFF => Bson::Other(kind),
            0x07 => Bson::ObjectId(self.take(12)?.try_into().map_err(|_| BAD)?),
            0x08 => Bson::Bool(self.take(1)?[0] != 0),
            0x09 => Bson::DateTime(self.i64()?),
            0x0B => {
                self.cstring()?;
                self.cstring()?;
                Bson::Other(kind)
            }
            0x0C => {
                self.string()?;
                self.take(12)?;
                Bson::Other(kind)
            }
            0x0D | 0x0E => {
                self.string()?;
                Bson::Other(kind)
            }
            0x0F => {
                let len = self.len(4)?;
                self.take(len - 4)?;
                Bson::Other(kind)
            }
            0x10 => Bson::Int32(self.i32()?),
            0x11 => Bson::Timestamp(self.i64()? as u64),
            0x12 => Bson::Int64(self.i64()?),
            0x13 => {
                self.take(16)?;
                Bson::Other(kind)
            }
            _ => return Err(format!("Not a MongoDB server (unknown BSON type {kind:#04x})")),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_the_bsonspec_examples() {
        let hello = Document::new().with("hello", "world");
        assert_eq!(hello.to_bytes(), b"\x16\x00\x00\x00\x02hello\x00\x06\x00\x00\x00world\x00\x00");
        let awesome = Document::new().with("BSON", Bson::Array(vec!["awesome".into(), 5.05.into(), 1986.into()]));
        let bytes = b"\x31\x00\x00\x00\x04BSON\x00\x26\x00\x00\x00\x020\x00\x08\x00\x00\x00awesome\x00\x011\x00\x33\x33\x33\x33\x33\x33\x14\x40\x102\x00\xc2\x07\x00\x00\x00\x00";
        assert_eq!(awesome.to_bytes(), bytes);
        assert_eq!(Document::from_bytes(bytes).unwrap(), (awesome, bytes.len()));
    }

    #[test]
    fn decodes_every_type_a_server_sends() {
        let doc = Document(vec![
            ("oid".into(), Bson::ObjectId([7; 12])),
            ("when".into(), Bson::DateTime(1_700_000_000_000)),
            ("ts".into(), Bson::Timestamp(42 << 32 | 1)),
            ("n".into(), Bson::Null),
            ("long".into(), Bson::Int64(-5)),
            ("bin".into(), Bson::Binary(4, vec![1, 2, 3])),
            ("sub".into(), Document::new().with("ok", 1.0).into()),
            ("yes".into(), true.into()),
        ]);
        let mut bytes = doc.to_bytes();
        // Append a regex, a decimal128 and a MaxKey by hand before the final NUL.
        bytes.pop();
        bytes.extend_from_slice(b"\x0Bre\x00^a\x00i\x00");
        bytes.extend_from_slice(b"\x13dec\x00");
        bytes.extend_from_slice(&[0; 16]);
        bytes.extend_from_slice(b"\x7Fmax\x00\x00");
        let len = bytes.len() as i32;
        bytes[..4].copy_from_slice(&len.to_le_bytes());
        let (back, used) = Document::from_bytes(&bytes).unwrap();
        assert_eq!(used, bytes.len());
        assert_eq!(&back.0[..8], &doc.0[..]);
        assert_eq!(back.get("re"), Some(&Bson::Other(0x0B)));
        assert_eq!(back.get("max"), Some(&Bson::Other(0x7F)));
        assert_eq!(back.number("long"), Some(-5.0));
        assert!(back.bool("yes") && !back.bool("n"));
        assert_eq!(back.binary("bin"), Some(&[1u8, 2, 3][..]));
    }

    #[test]
    fn malformed_and_hostile_input_is_refused() {
        assert!(Document::from_bytes(b"\x05\x00\x00").is_err(), "truncated");
        assert!(Document::from_bytes(b"\xff\xff\xff\x7f\x00").is_err(), "length past the end");
        assert!(Document::from_bytes(b"\x0c\x00\x00\x00\x02a\x00\xff\xff\xff\x7f\x00").is_err(), "string length past the end");
        assert!(Document::from_bytes(b"\x08\x00\x00\x00\x20a\x00\x00").unwrap_err().contains("unknown BSON type"));
        // 100 nested documents.
        let mut doc = Document::new().with("x", 1);
        for _ in 0..100 {
            doc = Document::new().with("x", doc);
        }
        assert!(Document::from_bytes(&doc.to_bytes()).unwrap_err().contains("deeply"));
    }
}
