//! The fast path: IDs packed into two u64 (up to 15 bytes, byte-wise order
//! kept), compact records, open-addressing tables, and u64 sort keys.
//! Preconditions (checked by the caller): window plus 7 days fits in u32
//! milliseconds, and at most 256 categories. Any ID over 15 bytes returns
//! Err(TooLong) and the caller reruns the general path.

use crate::parse::{parse_price, parse_ts, scan, DAY};
use crate::{chunks, par_map, timer, write_outputs, Inputs, TooLong, Ud, FxMap, GAP, WEEK};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};

pub type Id = [u64; 2];
const NONE: Id = [0, 0];

/// Bytes big-endian into [hi, lo], length in the lowest byte of lo. Never
/// [0, 0] for a non-empty ID.
#[inline(always)]
pub fn pack(b: &[u8]) -> Option<Id> {
    let n = b.len();
    if n > 15 {
        return None;
    }
    // Shift-or instead of a variable-length copy: the copy compiled to a
    // memcpy call that showed up at 18% of the 20M profile.
    let mut hi = 0u64;
    let mut lo = n as u64;
    let mut j = 0;
    while j < n && j < 8 {
        hi |= (b[j] as u64) << (56 - 8 * j);
        j += 1;
    }
    while j < n {
        lo |= (b[j] as u64) << (56 - 8 * (j - 8));
        j += 1;
    }
    Some([hi, lo])
}

#[cfg(test)]
mod tests {
    #[test]
    fn pack_matches_bytes() {
        let mut x: u64 = 0x1234_5678_9abc_def1;
        for _ in 0..20000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let n = (x % 17) as usize;
            let b: Vec<u8> = (0..n).map(|i| (x >> ((i * 3) % 56)) as u8).collect();
            let want = if n > 15 {
                None
            } else {
                let mut a = [0u8; 16];
                a[..n].copy_from_slice(&b);
                a[15] = n as u8;
                Some([u64::from_be_bytes(a[0..8].try_into().unwrap()), u64::from_be_bytes(a[8..16].try_into().unwrap())])
            };
            assert_eq!(super::pack(&b), want);
            if let Some(id) = want {
                let mut out = Vec::new();
                super::unpack(id, &mut out);
                assert_eq!(out, b);
            }
        }
    }
}

pub fn unpack(id: Id, out: &mut Vec<u8>) {
    let mut a = [0u8; 16];
    a[0..8].copy_from_slice(&id[0].to_be_bytes());
    a[8..16].copy_from_slice(&id[1].to_be_bytes());
    out.extend_from_slice(&a[..a[15] as usize]);
}

#[inline(always)]
pub fn mix(id: Id) -> u64 {
    let mut x = id[0].wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ id[1];
    x ^= x >> 32;
    x = x.wrapping_mul(0xD6E8_FEB8_6659_FD93);
    x ^= x >> 32;
    x = x.wrapping_mul(0xD6E8_FEB8_6659_FD93);
    x ^ (x >> 29)
}

const PBITS: u32 = 8;
const P: usize = 1 << PBITS;
#[inline(always)]
fn part(h: u64) -> usize {
    ((h >> 40) as usize) & (P - 1)
}

/// One copy of an event, partitioned by event_id. `ts == INVALID` marks a
/// copy that fails validation (or is late).
#[derive(Clone, Copy)]
struct ERec {
    eid: Id,
    user: Id,
    ingest: i64,
    ts: u32,
    /// price << 8 | category; price 0 for view and cart
    pc: u32,
}
const INVALID: u32 = u32::MAX;

/// A surviving event, partitioned by person. key = h32(person) << 32 | ts.
#[derive(Clone, Copy)]
struct SRec {
    key: u64,
    person: Id,
    pc: u32,
}

/// A valid click, partitioned by person. key = h32(person) << 32 | ts - (ws - 7 d).
#[derive(Clone, Copy)]
struct CRec {
    key: u64,
    person: Id,
    cid: Id,
    camp: u32,
}

/// Open-addressing map from linked ID to person name.
struct LinkTable {
    mask: usize,
    keys: Vec<Id>,
    vals: Vec<Id>,
}

impl LinkTable {
    #[inline(always)]
    fn resolve(&self, k: Id) -> Id {
        let mut i = mix(k) as usize & self.mask;
        loop {
            let kk = self.keys[i];
            if kk == k {
                return self.vals[i];
            }
            if kk == NONE {
                return k;
            }
            i = (i + 1) & self.mask;
        }
    }
}

pub fn run_fast(inp: &Inputs) -> Result<(), TooLong> {
    let mut tick = timer();
    let t = inp.threads;
    let too_long = AtomicBool::new(false);

    // ---- links
    let link_chunks = chunks(&inp.links, 1 << 20);
    let next = AtomicUsize::new(0);
    let pairs: Vec<Vec<(Id, Id)>> = par_map(t, |_| {
        let mut v = Vec::new();
        loop {
            let i = next.fetch_add(1, Relaxed);
            let Some(&c) = link_chunks.get(i) else { break };
            scan::<2>(c, |[a, b]| {
                if a.is_empty() || b.is_empty() {
                    return;
                }
                match (pack(a), pack(b)) {
                    (Some(a), Some(b)) => v.push((a, b)),
                    _ => too_long.store(true, Relaxed),
                }
            });
        }
        v
    });
    if too_long.load(Relaxed) {
        return Err(TooLong);
    }
    let mut ids: FxMap<u128, u32> = FxMap::default();
    let mut keys: Vec<Id> = Vec::new();
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
            let mut idx = |k: Id| -> u32 {
                *ids.entry(((k[0] as u128) << 64) | k[1] as u128).or_insert_with(|| {
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
    drop(ids);
    drop(pairs);
    let mut name: Vec<Id> = keys.clone();
    for i in 0..keys.len() as u32 {
        let r = find(&mut parent, i) as usize;
        if keys[i as usize] < name[r] {
            name[r] = keys[i as usize];
        }
    }
    let mut n_entries: usize = 0;
    for i in 0..keys.len() as u32 {
        let r = find(&mut parent, i) as usize;
        if name[r] != keys[i as usize] {
            n_entries += 1;
        }
    }
    let cap = (n_entries * 2).next_power_of_two().max(16);
    let mut lt = LinkTable { mask: cap - 1, keys: vec![NONE; cap], vals: vec![NONE; cap] };
    for i in 0..keys.len() as u32 {
        let r = find(&mut parent, i) as usize;
        let k = keys[i as usize];
        if name[r] != k {
            let mut j = mix(k) as usize & lt.mask;
            while lt.keys[j] != NONE {
                j = (j + 1) & lt.mask;
            }
            lt.keys[j] = k;
            lt.vals[j] = name[r];
        }
    }
    drop(keys);
    drop(name);
    drop(parent);
    let lt = &lt;
    tick("links");

    // ---- events and clicks: parse and validate
    let (ws, we) = (inp.ws, inp.we);
    let cws = ws - WEEK;
    let ev_chunks = chunks(&inp.events, 2 << 20);
    let ck_chunks = chunks(&inp.clicks, 2 << 20);
    let ne = ev_chunks.len();
    let next = AtomicUsize::new(0);
    let ev_bytes: usize = inp.events.iter().map(|f| f.len()).sum();
    let ck_bytes: usize = inp.clicks.iter().map(|f| f.len()).sum();
    // Lines are about 70 bytes (events) and 45 (clicks); reserve a little more.
    let ev_cap = ev_bytes / 60 / t / P + 16;
    let ck_cap = ck_bytes / 40 / t / P + 16;
    let parsed: Vec<(Vec<Vec<ERec>>, Vec<Vec<CRec>>)> = par_map(t, |_| {
        let mut ev: Vec<Vec<ERec>> = (0..P).map(|_| Vec::with_capacity(ev_cap)).collect();
        let mut ck: Vec<Vec<CRec>> = (0..P).map(|_| Vec::with_capacity(ck_cap)).collect();
        loop {
            let i = next.fetch_add(1, Relaxed);
            if i < ne {
                let mut bad = false;
                scan::<7>(ev_chunks[i], |[eid, user, ts, ingest, ty, product, price]| {
                    if eid.is_empty() {
                        return;
                    }
                    let Some(eid) = pack(eid) else {
                        bad = true;
                        return;
                    };
                    let ingest = parse_ts(ingest).unwrap_or(i64::MAX);
                    let mut rec = ERec { eid, user: NONE, ingest, ts: INVALID, pc: 0 };
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
                                (p << 8) | cat
                            }
                            _ => break 'valid,
                        };
                        let Some(u) = pack(user) else {
                            bad = true;
                            break 'valid;
                        };
                        rec.user = u;
                        rec.ts = (t - ws) as u32;
                        rec.pc = pc;
                    }
                    ev[part(mix(eid))].push(rec);
                });
                if bad {
                    too_long.store(true, Relaxed);
                    break;
                }
            } else if let Some(&c) = ck_chunks.get(i - ne) {
                let mut bad = false;
                scan::<4>(c, |[cid, user, camp, ts]| {
                    if cid.is_empty() || user.is_empty() {
                        return;
                    }
                    let Some(t) = parse_ts(ts) else { return };
                    if t < cws || t >= we {
                        return;
                    }
                    let Some(&camp) = inp.campaigns.get(camp) else { return };
                    let (Some(cid), Some(user)) = (pack(cid), pack(user)) else {
                        bad = true;
                        return;
                    };
                    let person = lt.resolve(user);
                    let h = mix(person);
                    ck[part(h)].push(CRec { key: (h << 32) | (t - cws) as u64, person, cid, camp });
                });
                if bad {
                    too_long.store(true, Relaxed);
                    break;
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
    // Pre-size the outputs (each worker sees about 1/t of the survivors, spread
    // over P partitions) so they rarely grow: growth remaps pages under the
    // process-wide mmap lock and stalls the other threads' page faults.
    let n_ev: usize = parsed.iter().map(|w| w.0.iter().map(|v| v.len()).sum::<usize>()).sum();
    let out_cap = n_ev / t / P + n_ev / t / P / 8 + 16;
    let sess_in: Vec<Vec<Vec<SRec>>> = par_map(t, |_| {
        let mut out: Vec<Vec<SRec>> = (0..P).map(|_| Vec::with_capacity(out_cap)).collect();
        let mut table: Vec<u32> = Vec::new();
        let mut buf: Vec<ERec> = Vec::new();
        loop {
            let p = next.fetch_add(1, Relaxed);
            if p >= P {
                break;
            }
            buf.clear();
            for w in &parsed {
                buf.extend_from_slice(&w.0[p]);
            }
            let cap = (buf.len() * 2).next_power_of_two().max(16);
            let mask = cap - 1;
            table.clear();
            table.resize(cap, 0);
            for (i, r) in buf.iter().enumerate() {
                let mut j = mix(r.eid) as usize & mask;
                loop {
                    let s = table[j];
                    if s == 0 {
                        table[j] = i as u32 + 1;
                        break;
                    }
                    let o = &buf[s as usize - 1];
                    if o.eid == r.eid {
                        if r.ingest < o.ingest {
                            table[j] = i as u32 + 1;
                        }
                        break;
                    }
                    j = (j + 1) & mask;
                }
            }
            for &s in &table {
                if s == 0 {
                    continue;
                }
                let r = &buf[s as usize - 1];
                if r.ts == INVALID {
                    continue;
                }
                let person = lt.resolve(r.user);
                let h = mix(person);
                out[part(h)].push(SRec { key: (h << 32) | r.ts as u64, person, pc: r.pc });
            }
        }
        out
    });
    let mut parsed = parsed;
    for w in parsed.iter_mut() {
        w.0 = Vec::new();
    }
    let parsed = parsed;
    tick("dedup events");

    // ---- sessionize and attribute per person partition, largest first
    let ncamp = inp.campaign_ids.len();
    let organic = ncamp as u32;
    let ws_day = ws.div_euclid(DAY);
    let ndays = ((we - 1).div_euclid(DAY) - ws_day + 1).max(1) as usize;
    let ncat = inp.categories.len();
    let mut order: Vec<(usize, usize)> = (0..P)
        .map(|p| (sess_in.iter().map(|w| w[p].len()).sum::<usize>() + parsed.iter().map(|w| w.1[p].len()).sum::<usize>(), p))
        .collect();
    order.sort_unstable_by(|a, b| b.cmp(a));
    let next = AtomicUsize::new(0);
    let results: Vec<(Ud, Vec<[i64; 4]>)> = par_map(t, |_| {
        let mut ud = Ud::new();
        let mut agg: Vec<[i64; 4]> = vec![[0; 4]; (ncamp + 1) * ndays];
        let mut stamp: Vec<u64> = vec![0; ncamp + 1];
        let mut serial: u64 = 0;
        let mut ev: Vec<SRec> = Vec::new();
        let mut ck: Vec<CRec> = Vec::new();
        let mut catsum: Vec<i64> = vec![0; ncat];
        let mut touched: Vec<u32> = Vec::new();
        let mut namebuf: Vec<u8> = Vec::new();
        loop {
            let o = next.fetch_add(1, Relaxed);
            if o >= P {
                break;
            }
            let p = order[o].1;
            ev.clear();
            ck.clear();
            for w in &sess_in {
                ev.extend_from_slice(&w[p]);
            }
            for w in &parsed {
                ck.extend_from_slice(&w.1[p]);
            }
            ev.sort_unstable_by_key(|r| r.key);
            ck.sort_unstable_by(|a, b| (a.key, a.cid).cmp(&(b.key, b.cid)));
            // Repair runs where two persons share a 32-bit hash: order them
            // by (person, time) so each person's rows are contiguous.
            fix_collisions(&mut ev, |r| r.key, |r| r.person, |a, b| (a.person, a.key).cmp(&(b.person, b.key)));
            fix_collisions(&mut ck, |r| r.key, |r| r.person, |a, b| (a.person, a.key, a.cid).cmp(&(b.person, b.key, b.cid)));

            let mut ci = 0usize;
            let mut i = 0usize;
            while i < ev.len() {
                let person = ev[i].person;
                let ph = ev[i].key >> 32;
                let mut j = i;
                while j < ev.len() && ev[j].person == person {
                    j += 1;
                }
                while ci < ck.len() && ((ck[ci].key >> 32), ck[ci].person) < (ph, person) {
                    ci += 1;
                }
                let cstart = ci;
                while ci < ck.len() && ck[ci].person == person {
                    ci += 1;
                }
                let clicks = &ck[cstart..ci];
                let (mut lo, mut hi) = (0usize, 0usize);
                let mut cur_day: i64 = i64::MIN;
                let (mut d_sess, mut d_ev, mut d_rev) = (0i64, 0i64, 0i64);
                let flush = |day: i64, s: i64, e: i64, r: i64, catsum: &mut Vec<i64>, touched: &mut Vec<u32>, ud: &mut Ud, namebuf: &mut Vec<u8>| {
                    namebuf.clear();
                    unpack(person, namebuf);
                    crate::parquet::push_str(&mut ud.names, namebuf);
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
                        crate::parquet::push_str(&mut ud.top, inp.categories[best as usize]);
                        for &c in touched.iter() {
                            catsum[c as usize] = 0;
                        }
                        touched.clear();
                    }
                };
                let mut k = i;
                while k < j {
                    let start_off = ev[k].key as u32 as i64;
                    let start = ws + start_off;
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
                    let mut last = start_off;
                    let mut n = 0i64;
                    let mut rev = 0i64;
                    while k < j {
                        let t = ev[k].key as u32 as i64;
                        if t - last > GAP {
                            break;
                        }
                        let pc = ev[k].pc;
                        let price = (pc >> 8) as i64;
                        if price > 0 {
                            rev += price;
                            let c = (pc & 0xFF) as usize;
                            if catsum[c] == 0 {
                                touched.push(c as u32);
                            }
                            catsum[c] += price;
                        }
                        n += 1;
                        last = t;
                        k += 1;
                    }
                    d_sess += 1;
                    d_ev += n;
                    d_rev += rev;
                    // attribution; click times are offsets from ws - 7 d
                    let s_c = start_off + WEEK;
                    while hi < clicks.len() && (clicks[hi].key as u32 as i64) <= s_c {
                        hi += 1;
                    }
                    while lo < hi && (clicks[lo].key as u32 as i64) < s_c - WEEK {
                        lo += 1;
                    }
                    let di = (day - ws_day) as usize;
                    let credit = if lo < hi {
                        let best = clicks[hi - 1].key as u32;
                        let mut b = hi - 1;
                        while b > lo && clicks[b - 1].key as u32 == best {
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
    write_outputs(inp, results, ncamp, ndays, ws_day);
    tick("write");
    Ok(())
}

/// In a slice sorted by `key`, find runs with equal high 32 bits (the person
/// hash) that hold more than one person, and sort those runs with `full`.
fn fix_collisions<T>(v: &mut [T], key: impl Fn(&T) -> u64, person: impl Fn(&T) -> Id, full: impl Fn(&T, &T) -> std::cmp::Ordering) {
    let mut i = 0;
    while i < v.len() {
        let h = key(&v[i]) >> 32;
        let p0 = person(&v[i]);
        let mut j = i + 1;
        let mut mixed = false;
        while j < v.len() && key(&v[j]) >> 32 == h {
            mixed |= person(&v[j]) != p0;
            j += 1;
        }
        if mixed {
            v[i..j].sort_unstable_by(&full);
        }
        i = j;
    }
}
