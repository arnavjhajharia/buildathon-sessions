//! A minimal Parquet writer: one row group, one PLAIN, uncompressed v1 data
//! page per column. Enough for flat tables of INT32 DATE, INT64 and strings.

pub enum Values {
    /// INT32 with the DATE logical type: days since the epoch.
    Date(Vec<i32>),
    I64(Vec<i64>),
    /// PLAIN BYTE_ARRAY bytes (4-byte length + bytes per value) and count.
    Str { plain: Vec<u8>, count: usize },
    /// Like Str, nullable: one flag per row, values only for present rows.
    OptStr { plain: Vec<u8>, present: Vec<bool> },
}

pub struct Column {
    pub name: &'static str,
    pub values: Values,
}

pub fn push_str(plain: &mut Vec<u8>, s: &[u8]) {
    plain.extend_from_slice(&(s.len() as u32).to_le_bytes());
    plain.extend_from_slice(s);
}

struct Thrift {
    buf: Vec<u8>,
    last: Vec<i16>,
    cur: i16,
}

const T_I32: u8 = 5;
const T_I64: u8 = 6;
const T_BIN: u8 = 8;
const T_LIST: u8 = 9;
const T_STRUCT: u8 = 12;

impl Thrift {
    fn new() -> Self {
        Thrift { buf: Vec::new(), last: Vec::new(), cur: 0 }
    }
    fn varint(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.buf.push((v as u8) | 0x80);
            v >>= 7;
        }
        self.buf.push(v as u8);
    }
    fn zz(&mut self, v: i64) {
        self.varint(((v << 1) ^ (v >> 63)) as u64);
    }
    fn field(&mut self, ty: u8, id: i16) {
        let delta = id - self.cur;
        if delta > 0 && delta <= 15 {
            self.buf.push(((delta as u8) << 4) | ty);
        } else {
            self.buf.push(ty);
            self.zz(id as i64);
        }
        self.cur = id;
    }
    fn i32(&mut self, id: i16, v: i32) {
        self.field(T_I32, id);
        self.zz(v as i64);
    }
    fn i64(&mut self, id: i16, v: i64) {
        self.field(T_I64, id);
        self.zz(v);
    }
    fn bin(&mut self, id: i16, v: &[u8]) {
        self.field(T_BIN, id);
        self.varint(v.len() as u64);
        self.buf.extend_from_slice(v);
    }
    fn list(&mut self, id: i16, elem: u8, size: usize) {
        self.field(T_LIST, id);
        if size < 15 {
            self.buf.push(((size as u8) << 4) | elem);
        } else {
            self.buf.push(0xF0 | elem);
            self.varint(size as u64);
        }
    }
    /// Begins a struct, as a field (Some(id)) or as a list element (None).
    fn begin(&mut self, id: Option<i16>) {
        if let Some(id) = id {
            self.field(T_STRUCT, id);
        }
        self.last.push(self.cur);
        self.cur = 0;
    }
    fn end(&mut self) {
        self.buf.push(0);
        self.cur = self.last.pop().unwrap();
    }
}

struct ChunkMeta {
    ptype: i32,
    offset: i64,
    size: i64,
}

/// Writes a complete Parquet file. All columns must hold `rows` rows.
pub fn write(path: &std::path::Path, rows: usize, cols: &[Column]) -> std::io::Result<()> {
    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(b"PAR1");
    let mut metas = Vec::new();
    if rows > 0 {
        for c in cols {
            let mut body: Vec<u8> = Vec::new();
            let ptype = match &c.values {
                Values::Date(v) => {
                    body.reserve(v.len() * 4);
                    for x in v {
                        body.extend_from_slice(&x.to_le_bytes());
                    }
                    1
                }
                Values::I64(v) => {
                    body.reserve(v.len() * 8);
                    for x in v {
                        body.extend_from_slice(&x.to_le_bytes());
                    }
                    2
                }
                Values::Str { plain, .. } => {
                    body.extend_from_slice(plain);
                    6
                }
                Values::OptStr { plain, present } => {
                    // Definition levels: RLE/bit-packed hybrid, bit width 1, as
                    // one bit-packed run, prefixed by its byte length.
                    let groups = present.len().div_ceil(8);
                    let mut lv = Thrift::new();
                    lv.varint(((groups as u64) << 1) | 1);
                    let mut bits = vec![0u8; groups];
                    for (i, &p) in present.iter().enumerate() {
                        if p {
                            bits[i >> 3] |= 1 << (i & 7);
                        }
                    }
                    lv.buf.extend_from_slice(&bits);
                    body.extend_from_slice(&(lv.buf.len() as u32).to_le_bytes());
                    body.extend_from_slice(&lv.buf);
                    body.extend_from_slice(plain);
                    6
                }
            };
            let mut h = Thrift::new();
            h.i32(1, 0); // DATA_PAGE
            h.i32(2, body.len() as i32);
            h.i32(3, body.len() as i32);
            h.begin(Some(5));
            h.i32(1, rows as i32);
            h.i32(2, 0); // PLAIN
            h.i32(3, 3); // RLE
            h.i32(4, 3); // RLE
            h.end();
            h.buf.push(0);
            let offset = out.len() as i64;
            out.extend_from_slice(&h.buf);
            out.extend_from_slice(&body);
            metas.push(ChunkMeta { ptype, offset, size: (h.buf.len() + body.len()) as i64 });
        }
    }

    let mut m = Thrift::new();
    m.i32(1, 1);
    m.list(2, T_STRUCT, cols.len() + 1);
    m.begin(None);
    m.bin(4, b"schema");
    m.i32(5, cols.len() as i32);
    m.end();
    for c in cols {
        m.begin(None);
        let (ptype, optional, conv, logical) = match &c.values {
            Values::Date(_) => (1, false, Some(6), Some(6)),
            Values::I64(_) => (2, false, None, None),
            Values::Str { .. } => (6, false, Some(0), Some(1)),
            Values::OptStr { .. } => (6, true, Some(0), Some(1)),
        };
        m.i32(1, ptype);
        m.i32(3, if optional { 1 } else { 0 });
        m.bin(4, c.name.as_bytes());
        if let Some(cv) = conv {
            m.i32(6, cv);
        }
        if let Some(lt) = logical {
            m.begin(Some(10));
            m.begin(Some(lt));
            m.end();
            m.end();
        }
        m.end();
    }
    m.i64(3, rows as i64);
    if rows > 0 {
        m.list(4, T_STRUCT, 1);
        m.begin(None);
        m.list(1, T_STRUCT, cols.len());
        let mut total = 0i64;
        for (c, cm) in cols.iter().zip(&metas) {
            total += cm.size;
            m.begin(None);
            m.i64(2, cm.offset);
            m.begin(Some(3));
            m.i32(1, cm.ptype);
            m.list(2, T_I32, 2);
            m.zz(0); // PLAIN
            m.zz(3); // RLE
            m.list(3, T_BIN, 1);
            m.varint(c.name.len() as u64);
            m.buf.extend_from_slice(c.name.as_bytes());
            m.i32(4, 0); // UNCOMPRESSED
            m.i64(5, rows as i64);
            m.i64(6, cm.size);
            m.i64(7, cm.size);
            m.i64(9, cm.offset);
            m.end();
            m.end();
        }
        m.i64(2, total);
        m.i64(3, rows as i64);
        m.end();
    } else {
        m.list(4, T_STRUCT, 0);
    }
    m.bin(6, b"sessions-engine");
    m.buf.push(0);
    out.extend_from_slice(&m.buf);
    out.extend_from_slice(&(m.buf.len() as u32).to_le_bytes());
    out.extend_from_slice(b"PAR1");
    std::fs::write(path, out)
}
