//! The long connection's frame (pbbp2.Frame), encoded by hand:
//!
//! ```text
//! message Header { string key = 1; string value = 2; }
//! message Frame {
//!   uint64 SeqID = 1; uint64 LogID = 2; int32 service = 3; int32 method = 4;
//!   repeated Header headers = 5; string payload_encoding = 6; string payload_type = 7;
//!   bytes payload = 8; string LogIDNew = 9;
//! }
//! ```

pub const CONTROL: i32 = 0;
pub const DATA: i32 = 1;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Frame {
    pub seq_id: u64,
    pub log_id: u64,
    pub service: i32,
    pub method: i32,
    pub headers: Vec<(String, String)>,
    pub payload_encoding: String,
    pub payload_type: String,
    pub payload: Vec<u8>,
    pub log_id_new: String,
}

impl Frame {
    pub fn header(&self, key: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = vec![];
        varint_field(&mut out, 1, self.seq_id);
        varint_field(&mut out, 2, self.log_id);
        varint_field(&mut out, 3, self.service as i64 as u64);
        varint_field(&mut out, 4, self.method as i64 as u64);
        for (k, v) in &self.headers {
            let mut h = vec![];
            bytes_field(&mut h, 1, k.as_bytes());
            bytes_field(&mut h, 2, v.as_bytes());
            bytes_field(&mut out, 5, &h);
        }
        if !self.payload_encoding.is_empty() {
            bytes_field(&mut out, 6, self.payload_encoding.as_bytes());
        }
        if !self.payload_type.is_empty() {
            bytes_field(&mut out, 7, self.payload_type.as_bytes());
        }
        if !self.payload.is_empty() {
            bytes_field(&mut out, 8, &self.payload);
        }
        if !self.log_id_new.is_empty() {
            bytes_field(&mut out, 9, self.log_id_new.as_bytes());
        }
        out
    }

    pub fn decode(b: &[u8]) -> Result<Frame, String> {
        let mut f = Frame::default();
        let mut r = Reader { b, i: 0 };
        while !r.done() {
            let (field, wire) = r.key()?;
            match (field, wire) {
                (1, 0) => f.seq_id = r.varint()?,
                (2, 0) => f.log_id = r.varint()?,
                (3, 0) => f.service = r.varint()? as i32,
                (4, 0) => f.method = r.varint()? as i32,
                (5, 2) => {
                    let mut h = Reader { b: r.bytes()?, i: 0 };
                    let (mut k, mut v) = (String::new(), String::new());
                    while !h.done() {
                        match h.key()? {
                            (1, 2) => k = String::from_utf8_lossy(h.bytes()?).into_owned(),
                            (2, 2) => v = String::from_utf8_lossy(h.bytes()?).into_owned(),
                            (_, w) => h.skip(w)?,
                        }
                    }
                    f.headers.push((k, v));
                }
                (6, 2) => f.payload_encoding = String::from_utf8_lossy(r.bytes()?).into_owned(),
                (7, 2) => f.payload_type = String::from_utf8_lossy(r.bytes()?).into_owned(),
                (8, 2) => f.payload = r.bytes()?.to_vec(),
                (9, 2) => f.log_id_new = String::from_utf8_lossy(r.bytes()?).into_owned(),
                (_, w) => r.skip(w)?,
            }
        }
        Ok(f)
    }
}

fn varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn varint_field(out: &mut Vec<u8>, field: u32, v: u64) {
    varint(out, (field as u64) << 3);
    varint(out, v);
}

fn bytes_field(out: &mut Vec<u8>, field: u32, b: &[u8]) {
    varint(out, ((field as u64) << 3) | 2);
    varint(out, b.len() as u64);
    out.extend_from_slice(b);
}

struct Reader<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Reader<'a> {
    fn done(&self) -> bool {
        self.i >= self.b.len()
    }

    fn varint(&mut self) -> Result<u64, String> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = *self.b.get(self.i).ok_or("truncated varint")?;
            self.i += 1;
            v |= ((byte & 0x7f) as u64) << shift;
            if byte & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err("varint too long".into())
    }

    fn key(&mut self) -> Result<(u32, u8), String> {
        let k = self.varint()?;
        Ok(((k >> 3) as u32, (k & 7) as u8))
    }

    fn bytes(&mut self) -> Result<&'a [u8], String> {
        let n = self.varint()? as usize;
        let end = self.i.checked_add(n).filter(|e| *e <= self.b.len()).ok_or("truncated field")?;
        let s = &self.b[self.i..end];
        self.i = end;
        Ok(s)
    }

    fn skip(&mut self, wire: u8) -> Result<(), String> {
        match wire {
            0 => {
                self.varint()?;
            }
            1 => self.i += 8,
            2 => {
                self.bytes()?;
            }
            5 => self.i += 4,
            w => return Err(format!("unsupported wire type {w}")),
        }
        if self.i > self.b.len() {
            return Err("truncated field".into());
        }
        Ok(())
    }
}

/// A stable 32-hex-digit hash (FNV-1a, two lanes) for idempotency keys that must stay short
pub fn short_hash(s: &str) -> String {
    let mut a: u64 = 0xcbf29ce484222325;
    let mut b: u64 = 0x84222325cbf29ce4;
    for byte in s.bytes() {
        a = (a ^ byte as u64).wrapping_mul(0x100000001b3);
        b = (b ^ byte as u64).wrapping_mul(0x100000001b3).rotate_left(5);
    }
    format!("{a:016x}{b:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let f = Frame {
            seq_id: 300,
            log_id: 1,
            service: 12345,
            method: DATA,
            headers: vec![("type".into(), "event".into()), ("message_id".into(), "m1".into())],
            payload: br#"{"a":1}"#.to_vec(),
            log_id_new: "x".into(),
            ..Default::default()
        };
        let d = Frame::decode(&f.encode()).unwrap();
        assert_eq!(d, f);
        assert_eq!(d.header("type"), Some("event"));
        assert!(Frame::decode(&[0x42, 0x05, 1]).is_err());
        // A ping as the official SDK sends it
        let p = Frame { service: 7, method: CONTROL, headers: vec![("type".into(), "ping".into())], ..Default::default() };
        assert_eq!(p.encode()[..8], [0x08, 0x00, 0x10, 0x00, 0x18, 0x07, 0x20, 0x00]);
        assert_eq!(short_hash("abc").len(), 32);
        assert_ne!(short_hash("abc"), short_hash("abd"));
    }
}
