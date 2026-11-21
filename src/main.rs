//! Shopping Sessions: click-stream CSV logs to two exact daily Parquet tables.
//!
//! Phases (all multi-threaded except link union-find):
//!   1. links: parse, union-find, name each person by its smallest ID
//!   2. events + clicks: parse and validate each line; events are scattered by
//!      event_id hash, clicks by person hash
//!   3. dedup events per event_id partition, keep valid winners, scatter by person
//!   4. per person partition: sort, sessionize, attribute, aggregate
//!   5. write Parquet, one file per worker
mod parquet;
mod parse;
mod sys;

use parse::{parse_price, parse_ts, split, DAY};
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::time::Instant;

const GAP: i64 = 1_800_000;
const WEEK: i64 = 7 * DAY;

// ---------------------------------------------------------------- hashing

#[derive(Default, Clone, Copy)]
pub struct Fx(u64);
const FK: u64 = 0x51_7c_c1_b7_27_22_0a_95;
impl Hasher for Fx {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut h = self.0;
        let mut c = bytes.chunks_exact(8);
        for w in &mut c {
            h = (h.rotate_left(5) ^ u64::from_le_bytes(w.try_into().unwrap())).wrapping_mul(FK);
        }
        let r = c.remainder();
        if !r.is_empty() {
            let mut b = [0u8; 8];
            b[..r.len()].copy_from_slice(r);
            h = (h.rotate_left(5) ^ u64::from_le_bytes(b)).wrapping_mul(FK);
        }
        self.0 = h;
    }
    #[inline]
    fn write_u64(&mut self, v: u64) {
        self.0 = (self.0.rotate_left(5) ^ v).wrapping_mul(FK);
    }
    #[inline]
    fn write_u128(&mut self, v: u128) {
        self.write_u64(v as u64);
        self.write_u64((v >> 64) as u64);
    }
    #[inline]
    fn write_usize(&mut self, v: usize) {
        self.write_u64(v as u64);
    }
    #[inline]
    fn write_u32(&mut self, v: u32) {
        self.write_u64(v as u64);
    }
    #[inline]
    fn write_u8(&mut self, v: u8) {
        self.write_u64(v as u64);
    }
    #[inline]
    fn finish(&self) -> u64 {
        let mut h = self.0;
        h ^= h >> 33;
        h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
        h ^= h >> 33;
        h
    }
}
type FxMap<K, V> = HashMap<K, V, BuildHasherDefault<Fx>>;

// ---------------------------------------------------------------- keys

/// An ID as a sortable, hashable value. `u128` packs IDs of up to 15 bytes
/// (bytes big-endian, length in the last byte), which keeps byte-wise order;
/// `&[u8]` is the general fallback for any length.
pub trait Key: Copy + Ord + Eq + Hash + Send + Sync + 'static {
    fn make(b: &'static [u8]) -> Option<Self>;
    fn zero() -> Self;
    fn put(&self, out: &mut Vec<u8>);
    #[inline]
    fn h(&self) -> u64 {
        let mut s = Fx::default();
        self.hash(&mut s);
        s.finish()
    }
}

impl Key for u128 {
    #[inline]
    fn make(b: &'static [u8]) -> Option<Self> {
        if b.len() > 15 {
            return None;
        }
        let mut buf = [0u8; 16];
        buf[..b.len()].copy_from_slice(b);
        buf[15] = b.len() as u8;
        Some(u128::from_be_bytes(buf))
    }
    fn zero() -> Self {
        0
    }
    fn put(&self, out: &mut Vec<u8>) {
        let a = self.to_be_bytes();
        out.extend_from_slice(&a[..a[15] as usize]);
    }
}

impl Key for &'static [u8] {
    #[inline]
    fn make(b: &'static [u8]) -> Option<Self> {
        Some(b)
    }
    fn zero() -> Self {
        &[]
    }
    fn put(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(self);
    }
}

// ---------------------------------------------------------------- inputs

struct Inputs {
    events: Vec<&'static [u8]>,
    clicks: Vec<&'static [u8]>,
    links: Vec<&'static [u8]>,
    /// product_id -> category index (indices in byte-wise category order)
    products: FxMap<&'static [u8], u32>,
    categories: Vec<&'static [u8]>,
    unknown_cat: u32,
    /// campaign_id -> index; channels[index]
    campaigns: FxMap<&'static [u8], u32>,
    campaign_ids: Vec<&'static [u8]>,
    channels: Vec<&'static [u8]>,
    ws: i64,
    we: i64,
    threads: usize,
    out: PathBuf,
}

fn csv_files(dir: &Path) -> Vec<&'static [u8]> {
    let mut paths: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().map_or(false, |x| x == "csv") && p.is_file())
            .collect(),
        Err(_) => Vec::new(),
    };
    paths.sort();
    paths.iter().map(|p| sys::map_file(p).expect("map")).collect()
}

/// Lines of a file after its header line.
fn body(data: &'static [u8]) -> &'static [u8] {
    match memchr(b'\n', data) {
        Some(i) => &data[i + 1..],
        None => &[],
    }
}

#[inline]
fn memchr(c: u8, s: &[u8]) -> Option<usize> {
    s.iter().position(|&b| b == c)
}

fn lines(data: &'static [u8]) -> impl Iterator<Item = &'static [u8]> {
    let mut rest = data;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        match memchr(b'\n', rest) {
            Some(i) => {
                let l = &rest[..i];
                rest = &rest[i + 1..];
                Some(l)
            }
            None => {
                let l = rest;
                rest = &[];
                Some(l)
            }
        }
    })
}

fn manifest_value(text: &str, key: &str) -> i64 {
    let at = text.find(&format!("\"{key}\"")).expect("manifest key") + key.len() + 2;
    let rest = &text[at..];
    let colon = rest.find(':').unwrap();
    let rest = &rest[colon + 1..];
    let q1 = rest.find('"').unwrap();
    let rest = &rest[q1 + 1..];
    let q2 = rest.find('"').unwrap();
    parse_ts(rest[..q2].as_bytes()).expect("manifest timestamp")
}

fn load(data: &Path, out: PathBuf, threads: usize) -> Inputs {
    let manifest = std::fs::read_to_string(data.join("manifest.json")).expect("manifest.json");
    let ws = manifest_value(&manifest, "window_start");
    let we = manifest_value(&manifest, "window_end");

    let products_raw: &'static [u8] = Box::leak(std::fs::read(data.join("products.csv")).unwrap_or_default().into_boxed_slice());
    let mut prod_rows: Vec<(&'static [u8], &'static [u8])> = Vec::new();
    for l in lines(body(products_raw)) {
        if let Some([p, c]) = split::<2>(l) {
            prod_rows.push((p, c));
        }
    }
    let mut cats: Vec<&'static [u8]> = prod_rows.iter().map(|r| r.1).collect();
    cats.push(b"unknown");
    cats.sort_unstable();
    cats.dedup();
    let cat_index: FxMap<&[u8], u32> = cats.iter().enumerate().map(|(i, c)| (*c, i as u32)).collect();
    let mut products = FxMap::default();
    for (p, c) in prod_rows {
        products.entry(p).or_insert(cat_index[c]);
    }
    let unknown_cat = cat_index[&b"unknown"[..]];

    let camp_raw: &'static [u8] = Box::leak(std::fs::read(data.join("campaigns.csv")).unwrap_or_default().into_boxed_slice());
    let mut campaigns = FxMap::default();
    let mut campaign_ids = Vec::new();
    let mut channels = Vec::new();
    for l in lines(body(camp_raw)) {
        if let Some([c, ch]) = split::<2>(l) {
            if !campaigns.contains_key(c) {
                campaigns.insert(c, campaign_ids.len() as u32);
                campaign_ids.push(c);
                channels.push(ch);
            }
        }
    }

    Inputs {
        events: csv_files(&data.join("events")),
        clicks: csv_files(&data.join("clicks")),
        links: csv_files(&data.join("links")),
        products,
        categories: cats,
        unknown_cat,
        campaigns,
        campaign_ids,
        channels,
        ws,
        we,
        threads,
        out,
    }
}

/// Byte ranges of whole lines, about `target` bytes each, header skipped.
fn chunks(files: &[&'static [u8]], target: usize) -> Vec<&'static [u8]> {
    let mut v = Vec::new();
    for &f in files {
        let b = body(f);
        let mut s = 0;
        while s < b.len() {
            let mut e = (s + target).min(b.len());
            if e < b.len() {
                e = match memchr(b'\n', &b[e..]) {
                    Some(i) => e + i + 1,
                    None => b.len(),
                };
            }
            v.push(&b[s..e]);
            s = e;
        }
    }
    v
}

fn par_map<T: Send, F: Fn(usize) -> T + Sync>(t: usize, f: F) -> Vec<T> {
    std::thread::scope(|s| {
        let hs: Vec<_> = (0..t).map(|w| {
            let f = &f;
            s.spawn(move || f(w))
        }).collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    })
}

// ---------------------------------------------------------------- records

const VALID: u64 = 1 << 63;

#[derive(Clone, Copy)]
struct ERec<K> {
    eid: K,
    user: K,
    ingest: i64,
    ts: i64,
    /// VALID | price << 24 | category; price 0 for view and cart
    pc: u64,
}

#[derive(Clone, Copy)]
struct SRec<K> {
    person: K,
    ts: i64,
    pc: u64,
}

#[derive(Clone, Copy)]
struct CRec<K> {
    person: K,
    ts: i64,
    cid: K,
    camp: u32,
}

const PBITS: u32 = 8;
const P: usize = 1 << PBITS;
#[inline]
fn part(h: u64) -> usize {
    ((h >> 40) as usize) & (P - 1)
}

struct TooLong;

fn timer() -> impl FnMut(&str) {
    let on = std::env::var_os("PROFILE").is_some();
    let t0 = Instant::now();
    let mut last = t0;
    move |what: &str| {
        if on {
            let now = Instant::now();
            eprintln!("{:>8.1} ms  {:>8.1} ms  {}", (now - last).as_secs_f64() * 1e3, (now - t0).as_secs_f64() * 1e3, what);
            last = now;
        }
    }
}

// ---------------------------------------------------------------- pipeline

fn run<K: Key>(inp: &Inputs) -> Result<(), TooLong> {
    let mut tick = timer();
    let t = inp.threads;
    let too_long = AtomicBool::new(false);

    // ---- links
    let link_chunks = chunks(&inp.links, 1 << 20);
    let next = AtomicUsize::new(0);
    let pairs: Vec<Vec<(K, K)>> = par_map(t, |_| {
        let mut v = Vec::new();
        loop {
            let i = next.fetch_add(1, Relaxed);
            let Some(&c) = link_chunks.get(i) else { break };
            for l in lines(c) {
                if let Some([a, b]) = split::<2>(l) {
                    if a.is_empty() || b.is_empty() {
                        continue;
                    }
                    match (K::make(a), K::make(b)) {
                        (Some(a), Some(b)) => v.push((a, b)),
                        _ => too_long.store(true, Relaxed),
                    }
                }
            }
        }
        v
    });
    if too_long.load(Relaxed) {
        return Err(TooLong);
    }
    let mut ids: FxMap<K, u32> = FxMap::default();
    let mut keys: Vec<K> = Vec::new();
    let mut parent: Vec<u32> = Vec::new();
    fn find(parent: &mut [u32], mut x: u32) -> u32 {
        while parent[x as usize] != x {
            let p = parent[x as usize];
            parent[x as usize] = parent[p as usize];
            x = p;
        }
        x
    }
    for v in &pairs {
        for &(a, b) in v {
            let mut idx = |k: K| -> u32 {
                *ids.entry(k).or_insert_with(|| {
                    keys.push(k);
                    parent.push(keys.len() as u32 - 1);
                    keys.len() as u32 - 1
                })
            };
            let ia = idx(a);
            let ib = idx(b);
            let ra = find(&mut parent, ia);
            let rb = find(&mut parent, ib);
            if ra != rb {
                parent[ra.max(rb) as usize] = ra.min(rb);
            }
        }
    }
    let mut name: Vec<K> = keys.clone();
    for i in 0..keys.len() as u32 {
        let r = find(&mut parent, i) as usize;
        if keys[i as usize] < name[r] {
            name[r] = keys[i as usize];
        }
    }
    let mut linkmap: FxMap<K, K> = FxMap::default();
    for i in 0..keys.len() as u32 {
        let r = find(&mut parent, i) as usize;
        if name[r] != keys[i as usize] {
            linkmap.insert(keys[i as usize], name[r]);
        }
    }
    drop(ids);
    let resolve = |u: K| -> K { linkmap.get(&u).copied().unwrap_or(u) };
    tick("links");

    // ---- events and clicks: parse and validate
    let (ws, we) = (inp.ws, inp.we);
    let cws = ws - WEEK;
    let ev_chunks = chunks(&inp.events, 4 << 20);
    let ck_chunks = chunks(&inp.clicks, 4 << 20);
    let ne = ev_chunks.len();
    let next = AtomicUsize::new(0);
    let parsed: Vec<(Vec<Vec<ERec<K>>>, Vec<Vec<CRec<K>>>)> = par_map(t, |_| {
        let mut ev: Vec<Vec<ERec<K>>> = (0..P).map(|_| Vec::new()).collect();
        let mut ck: Vec<Vec<CRec<K>>> = (0..P).map(|_| Vec::new()).collect();
        loop {
            let i = next.fetch_add(1, Relaxed);
            if i < ne {
                for l in lines(ev_chunks[i]) {
                    let Some([eid, user, ts, ingest, ty, product, price]) = split::<7>(l) else { continue };
                    if eid.is_empty() {
                        continue;
                    }
                    let Some(eid) = K::make(eid) else {
                        too_long.store(true, Relaxed);
                        return (ev, ck);
                    };
                    let ingest = parse_ts(ingest).unwrap_or(i64::MAX);
                    let mut rec = ERec { eid, user: K::zero(), ingest, ts: 0, pc: 0 };
                    'valid: {
                        if user.is_empty() || product.is_empty() {
                            break 'valid;
                        }
                        let Some(t) = parse_ts(ts) else { break 'valid };
                        if t < ws || t >= we || ingest - t > DAY {
                            break 'valid;
                        }
                        let pc = match ty {
                            b"view" | b"cart" => 0,
                            b"purchase" => {
                                let Some(p) = parse_price(price) else { break 'valid };
                                let cat = inp.products.get(product).copied().unwrap_or(inp.unknown_cat);
                                ((p as u64) << 24) | cat as u64
                            }
                            _ => break 'valid,
                        };
                        let Some(u) = K::make(user) else {
                            too_long.store(true, Relaxed);
                            return (ev, ck);
                        };
                        rec.user = u;
                        rec.ts = t;
                        rec.pc = VALID | pc;
                    }
                    ev[part(eid.h())].push(rec);
                }
            } else if let Some(&c) = ck_chunks.get(i - ne) {
                for l in lines(c) {
                    let Some([cid, user, camp, ts]) = split::<4>(l) else { continue };
                    if cid.is_empty() || user.is_empty() {
                        continue;
                    }
                    let Some(t) = parse_ts(ts) else { continue };
                    if t < cws || t >= we {
                        continue;
                    }
                    let Some(&camp) = inp.campaigns.get(camp) else { continue };
                    let (Some(cid), Some(user)) = (K::make(cid), K::make(user)) else {
                        too_long.store(true, Relaxed);
                        return (ev, ck);
                    };
                    let person = resolve(user);
                    ck[part(person.h())].push(CRec { person, ts: t, cid, camp });
                }
            } else {
                break;
            }
        }
        (ev, ck)
    });
    if too_long.load(Relaxed) {
        return Err(TooLong);
    }
    tick("parse events+clicks");

    // ---- dedup events by event_id; scatter valid winners by person
    let next = AtomicUsize::new(0);
    let sess_in: Vec<Vec<Vec<SRec<K>>>> = par_map(t, |_| {
        let mut out: Vec<Vec<SRec<K>>> = (0..P).map(|_| Vec::new()).collect();
        let mut map: FxMap<K, u32> = FxMap::default();
        let mut win: Vec<ERec<K>> = Vec::new();
        loop {
            let p = next.fetch_add(1, Relaxed);
            if p >= P {
                break;
            }
            map.clear();
            win.clear();
            for w in &parsed {
                for r in &w.0[p] {
                    match map.entry(r.eid) {
                        std::collections::hash_map::Entry::Vacant(e) => {
                            e.insert(win.len() as u32);
                            win.push(*r);
                        }
                        std::collections::hash_map::Entry::Occupied(e) => {
                            let b = &mut win[*e.get() as usize];
                            if r.ingest < b.ingest {
                                *b = *r;
                            }
                        }
                    }
                }
            }
            for r in &win {
                if r.pc & VALID != 0 {
                    let person = resolve(r.user);
                    out[part(person.h())].push(SRec { person, ts: r.ts, pc: r.pc });
                }
            }
        }
        out
    });
    tick("dedup events");

    // ---- sessionize and attribute per person partition
    let ncamp = inp.campaign_ids.len();
    let organic = ncamp as u32;
    let ws_day = ws.div_euclid(DAY);
    let ndays = ((we - 1).div_euclid(DAY) - ws_day + 1).max(1) as usize;
    let ncat = inp.categories.len();
    let next = AtomicUsize::new(0);
    struct Ud {
        names: Vec<u8>,
        days: Vec<i32>,
        sessions: Vec<i64>,
        events: Vec<i64>,
        revenue: Vec<i64>,
        top: Vec<u8>,
        present: Vec<bool>,
    }
    let results: Vec<(Ud, Vec<[i64; 4]>)> = par_map(t, |_| {
        let mut ud = Ud { names: Vec::new(), days: Vec::new(), sessions: Vec::new(), events: Vec::new(), revenue: Vec::new(), top: Vec::new(), present: Vec::new() };
        // [campaign][day] -> sessions, purchasing, revenue, assisted
        let mut agg: Vec<[i64; 4]> = vec![[0; 4]; (ncamp + 1) * ndays];
        let mut stamp: Vec<u64> = vec![0; ncamp + 1];
        let mut serial: u64 = 0;
        let mut ev: Vec<SRec<K>> = Vec::new();
        let mut ck: Vec<CRec<K>> = Vec::new();
        let mut catsum: Vec<i64> = vec![0; ncat];
        let mut touched: Vec<u32> = Vec::new();
        let mut namebuf: Vec<u8> = Vec::new();
        loop {
            let p = next.fetch_add(1, Relaxed);
            if p >= P {
                break;
            }
            ev.clear();
            ck.clear();
            for w in &sess_in {
                ev.extend_from_slice(&w[p]);
            }
            for w in &parsed {
                ck.extend_from_slice(&w.1[p]);
            }
            ev.sort_unstable_by(|a, b| (a.person, a.ts).cmp(&(b.person, b.ts)));
            ck.sort_unstable_by(|a, b| (a.person, a.ts, a.cid).cmp(&(b.person, b.ts, b.cid)));
            let mut ci = 0usize;
            let mut i = 0usize;
            while i < ev.len() {
                let person = ev[i].person;
                let mut j = i;
                while j < ev.len() && ev[j].person == person {
                    j += 1;
                }
                while ci < ck.len() && ck[ci].person < person {
                    ci += 1;
                }
                let cstart = ci;
                while ci < ck.len() && ck[ci].person == person {
                    ci += 1;
                }
                let clicks = &ck[cstart..ci];
                let (mut lo, mut hi) = (0usize, 0usize);
                // per (person, day) accumulators
                let mut cur_day: i64 = i64::MIN;
                let (mut d_sess, mut d_ev, mut d_rev) = (0i64, 0i64, 0i64);
                let flush = |day: i64, s: i64, e: i64, r: i64, catsum: &mut Vec<i64>, touched: &mut Vec<u32>, ud: &mut Ud, namebuf: &mut Vec<u8>| {
                    namebuf.clear();
                    person.put(namebuf);
                    parquet::push_str(&mut ud.names, namebuf);
                    ud.days.push(day as i32);
                    ud.sessions.push(s);
                    ud.events.push(e);
                    ud.revenue.push(r);
                    if touched.is_empty() {
                        ud.present.push(false);
                    } else {
                        let mut best = touched[0];
                        for &c in touched.iter() {
                            let (sc, sb) = (catsum[c as usize], catsum[best as usize]);
                            if sc > sb || (sc == sb && c < best) {
                                best = c;
                            }
                        }
                        ud.present.push(true);
                        parquet::push_str(&mut ud.top, inp.categories[best as usize]);
                        for &c in touched.iter() {
                            catsum[c as usize] = 0;
                        }
                        touched.clear();
                    }
                };
                let mut k = i;
                while k < j {
                    let start = ev[k].ts;
                    let day = start.div_euclid(DAY);
                    if day != cur_day {
                        if cur_day != i64::MIN {
                            flush(cur_day, d_sess, d_ev, d_rev, &mut catsum, &mut touched, &mut ud, &mut namebuf);
                        }
                        cur_day = day;
                        d_sess = 0;
                        d_ev = 0;
                        d_rev = 0;
                    }
                    let mut last = start;
                    let mut n = 0i64;
                    let mut rev = 0i64;
                    while k < j && ev[k].ts - last <= GAP {
                        let pc = ev[k].pc;
                        let price = ((pc >> 24) & 0xFF_FFFF) as i64;
                        if price > 0 {
                            rev += price;
                            let c = (pc & 0xFF_FFFF) as usize;
                            if catsum[c] == 0 {
                                touched.push(c as u32);
                            }
                            catsum[c] += price;
                        }
                        n += 1;
                        last = ev[k].ts;
                        k += 1;
                    }
                    d_sess += 1;
                    d_ev += n;
                    d_rev += rev;
                    // attribution
                    while hi < clicks.len() && clicks[hi].ts <= start {
                        hi += 1;
                    }
                    while lo < hi && clicks[lo].ts < start - WEEK {
                        lo += 1;
                    }
                    let di = (day - ws_day) as usize;
                    let credit = if lo < hi {
                        let best = clicks[hi - 1].ts;
                        let mut b = hi - 1;
                        while b > lo && clicks[b - 1].ts == best {
                            b -= 1;
                        }
                        serial += 1;
                        for c in &clicks[lo..hi] {
                            let ca = c.camp as usize;
                            if stamp[ca] != serial {
                                stamp[ca] = serial;
                                agg[ca * ndays + di][3] += 1;
                            }
                        }
                        clicks[b].camp
                    } else {
                        organic
                    };
                    let a = &mut agg[credit as usize * ndays + di];
                    a[0] += 1;
                    a[1] += (rev > 0) as i64;
                    a[2] += rev;
                }
                flush(cur_day, d_sess, d_ev, d_rev, &mut catsum, &mut touched, &mut ud, &mut namebuf);
                i = j;
            }
        }
        (ud, agg)
    });
    tick("sessionize+attribute");

    // ---- write
    let ud_dir = inp.out.join("user_daily");
    let cd_dir = inp.out.join("campaign_daily");
    std::fs::create_dir_all(&ud_dir).unwrap();
    std::fs::create_dir_all(&cd_dir).unwrap();
    let mut agg: Vec<[i64; 4]> = vec![[0; 4]; (ncamp + 1) * ndays];
    let mut uds = Vec::new();
    for (ud, a) in results {
        for (x, y) in agg.iter_mut().zip(&a) {
            for q in 0..4 {
                x[q] += y[q];
            }
        }
        uds.push(ud);
    }
    let any_rows = uds.iter().any(|u| !u.days.is_empty());
    let uds_ref = &uds;
    let ud_dir_ref = &ud_dir;
    std::thread::scope(|s| {
        for (w, ud) in uds_ref.iter().enumerate() {
            if ud.days.is_empty() && (any_rows || w > 0) {
                continue;
            }
            s.spawn(move || {
                let rows = ud.days.len();
                let cols = [
                    parquet::Column { name: "user_id", values: parquet::Values::Str { plain: ud.names.clone(), count: rows } },
                    parquet::Column { name: "day", values: parquet::Values::Date(ud.days.clone()) },
                    parquet::Column { name: "sessions", values: parquet::Values::I64(ud.sessions.clone()) },
                    parquet::Column { name: "events", values: parquet::Values::I64(ud.events.clone()) },
                    parquet::Column { name: "revenue_cents", values: parquet::Values::I64(ud.revenue.clone()) },
                    parquet::Column { name: "top_category", values: parquet::Values::OptStr { plain: ud.top.clone(), present: ud.present.clone() } },
                ];
                parquet::write(&ud_dir_ref.join(format!("part-{w:03}.parquet")), rows, &cols).unwrap();
            });
        }
        // campaign_daily
        let mut ids = Vec::new();
        let mut chans = Vec::new();
        let mut days = Vec::new();
        let mut cols4: [Vec<i64>; 4] = Default::default();
        let mut rows = 0;
        for c in 0..=ncamp {
            for d in 0..ndays {
                let a = agg[c * ndays + d];
                if a[0] == 0 && a[3] == 0 {
                    continue;
                }
                let (id, ch): (&[u8], &[u8]) = if c == ncamp { (b"organic", b"organic") } else { (inp.campaign_ids[c], inp.channels[c]) };
                parquet::push_str(&mut ids, id);
                parquet::push_str(&mut chans, ch);
                days.push((ws_day + d as i64) as i32);
                for q in 0..4 {
                    cols4[q].push(a[q]);
                }
                rows += 1;
            }
        }
        let [s0, s1, s2, s3] = cols4;
        let cols = [
            parquet::Column { name: "campaign_id", values: parquet::Values::Str { plain: ids, count: rows } },
            parquet::Column { name: "channel", values: parquet::Values::Str { plain: chans, count: rows } },
            parquet::Column { name: "day", values: parquet::Values::Date(days) },
            parquet::Column { name: "sessions", values: parquet::Values::I64(s0) },
            parquet::Column { name: "purchasing_sessions", values: parquet::Values::I64(s1) },
            parquet::Column { name: "revenue_cents", values: parquet::Values::I64(s2) },
            parquet::Column { name: "assisted_sessions", values: parquet::Values::I64(s3) },
        ];
        parquet::write(&cd_dir.join("part-000.parquet"), rows, &cols).unwrap();
    });
    tick("write");
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // Fixture runners pass DATA_DIR and OUTPUT_DIR as the last two arguments too.
    let (data, out) = if args.len() >= 3 {
        (PathBuf::from(&args[args.len() - 2]), PathBuf::from(&args[args.len() - 1]))
    } else {
        (
            PathBuf::from(std::env::var("DATA_DIR").unwrap_or_else(|_| "/data".into())),
            PathBuf::from(std::env::var("OUTPUT_DIR").unwrap_or_else(|_| "/output".into())),
        )
    };
    let threads = std::env::var("NCPU")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(8, |n| n.get()));
    let inp = load(&data, out, threads);
    if run::<u128>(&inp).is_err() {
        if std::env::var_os("PROFILE").is_some() {
            eprintln!("an ID is longer than 15 bytes: general path");
        }
        if run::<&'static [u8]>(&inp).is_err() {
            unreachable!();
        }
    }
    // Skip freeing memory: the kernel does it faster at exit.
    std::process::exit(0);
}
