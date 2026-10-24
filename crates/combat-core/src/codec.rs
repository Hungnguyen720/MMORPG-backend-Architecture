//! Hand-packed codec for the two high-rate messages: client movement and
//! server snapshots. Everything else is rare enough for bincode.
//!
//! Positions are quantized to 2 cm on a 16-bit grid centred on the zone
//! origin. Snapshots are deltas against the last snapshot the client
//! acknowledged: unchanged entities cost nothing, a walking one about five
//! bytes.

use crate::{AbilityId, AuraId, EntityId};

/// Quantization step for positions, in metres.
pub const POS_STEP: f32 = 0.02;
/// Largest zone side the 16-bit position grid covers.
pub const MAX_ZONE_SIZE: f32 = 65535.0 * POS_STEP;
const POS_HALF: f32 = MAX_ZONE_SIZE / 2.0;
/// Auras shown per entity in snapshots.
pub const MAX_SNAP_AURAS: usize = 8;

pub fn quantize(v: f32) -> u16 {
    ((v + POS_HALF) / POS_STEP).round().clamp(0.0, 65535.0) as u16
}

pub fn dequantize(q: u16) -> f32 {
    q as f32 * POS_STEP - POS_HALF
}

pub fn quantize_pos(p: [f32; 3]) -> [u16; 3] {
    [quantize(p[0]), quantize(p[1]), quantize(p[2])]
}

pub fn dequantize_pos(q: [u16; 3]) -> [f32; 3] {
    [dequantize(q[0]), dequantize(q[1]), dequantize(q[2])]
}

pub fn quantize_facing(f: f32) -> u8 {
    let t = f.rem_euclid(std::f32::consts::TAU) / std::f32::consts::TAU;
    ((t * 256.0).round() as u32 & 0xff) as u8
}

pub fn dequantize_facing(q: u8) -> f32 {
    q as f32 / 256.0 * std::f32::consts::TAU
}

// ---------------------------------------------------------------------------
// Bit streams
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct BitWriter {
    out: Vec<u8>,
    acc: u64,
    bits: u32,
}

impl BitWriter {
    pub fn with_capacity(bytes: usize) -> Self {
        Self {
            out: Vec::with_capacity(bytes),
            acc: 0,
            bits: 0,
        }
    }

    pub fn write(&mut self, value: u32, bits: u32) {
        debug_assert!(bits <= 32);
        let mask = if bits == 32 { u32::MAX } else { (1 << bits) - 1 };
        self.acc |= ((value & mask) as u64) << self.bits;
        self.bits += bits;
        while self.bits >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.bits -= 8;
        }
    }

    pub fn bit(&mut self, b: bool) {
        self.write(b as u32, 1);
    }

    /// Variable length: 7 data bits per group plus a continue bit.
    pub fn var(&mut self, mut v: u32) {
        while v >= 0x80 {
            self.write((v & 0x7f) | 0x80, 8);
            v >>= 7;
        }
        self.write(v, 8);
    }

    pub fn finish(mut self) -> Vec<u8> {
        if self.bits > 0 {
            self.out.push(self.acc as u8);
        }
        self.out
    }
}

pub struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    acc: u64,
    bits: u32,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            acc: 0,
            bits: 0,
        }
    }

    pub fn read(&mut self, bits: u32) -> Option<u32> {
        while self.bits < bits {
            let b = *self.data.get(self.pos)?;
            self.pos += 1;
            self.acc |= (b as u64) << self.bits;
            self.bits += 8;
        }
        let mask = if bits == 32 {
            u32::MAX as u64
        } else {
            (1u64 << bits) - 1
        };
        let v = (self.acc & mask) as u32;
        self.acc >>= bits;
        self.bits -= bits;
        Some(v)
    }

    pub fn bit(&mut self) -> Option<bool> {
        self.read(1).map(|b| b == 1)
    }

    pub fn var(&mut self) -> Option<u32> {
        let mut v = 0u32;
        for shift in (0..35).step_by(7) {
            let b = self.read(8)?;
            v |= (b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Some(v);
            }
        }
        None
    }
}

fn write_signed(w: &mut BitWriter, v: i32, bits: u32) {
    w.write(v as u32, bits);
}

fn read_signed(r: &mut BitReader, bits: u32) -> Option<i32> {
    let raw = r.read(bits)?;
    let shift = 32 - bits;
    Some(((raw << shift) as i32) >> shift)
}

// ---------------------------------------------------------------------------
// Client → server movement (channel STATE, 20 Hz)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MoveInput {
    pub seq: u32,
    /// Newest snapshot id the client has received, for delta baselines.
    pub ack: u32,
    pub pos: [f32; 3],
    pub facing: f32,
    pub moving: bool,
    /// Current target (0 = none). Lets the server keep it in view.
    pub target: u32,
}

impl MoveInput {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = BitWriter::with_capacity(16);
        w.write(self.seq, 32);
        w.write(self.ack, 32);
        for q in quantize_pos(self.pos) {
            w.write(q as u32, 16);
        }
        w.write(quantize_facing(self.facing) as u32, 8);
        w.bit(self.moving);
        w.var(self.target);
        w.finish()
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let mut r = BitReader::new(bytes);
        let seq = r.read(32)?;
        let ack = r.read(32)?;
        let q = [r.read(16)? as u16, r.read(16)? as u16, r.read(16)? as u16];
        let facing = dequantize_facing(r.read(8)? as u8);
        let moving = r.bit()?;
        let target = r.var()?;
        Some(Self {
            seq,
            ack,
            pos: dequantize_pos(q),
            facing,
            moving,
            target,
        })
    }
}

// ---------------------------------------------------------------------------
// Server → client snapshots (channel STATE, 20 Hz)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum EntityKind {
    #[default]
    Player = 0,
    Mob = 1,
}

pub mod flags {
    pub const DEAD: u8 = 1;
    pub const IN_COMBAT: u8 = 2;
    pub const EVADING: u8 = 4;
    pub const MOVING: u8 = 8;
    /// Mob that never fights back (training dummy).
    pub const PASSIVE: u8 = 16;
    pub const ALL_BITS: u32 = 5;
}

/// Everything a client knows about one entity. Copy, so per-client snapshot
/// history is cheap to keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EntState {
    pub kind: EntityKind,
    /// Class index for players, mob index for mobs.
    pub def: u16,
    pub level: u8,
    pub pos: [u16; 3],
    pub facing: u8,
    pub hp: u32,
    pub max_hp: u32,
    pub mana: u32,
    pub max_mana: u32,
    pub flags: u8,
    /// (ability, server time the bar started).
    pub cast: Option<(AbilityId, u32)>,
    pub aura_count: u8,
    /// (aura, server time it expires).
    pub auras: [(AuraId, u32); MAX_SNAP_AURAS],
}

impl EntState {
    pub fn auras(&self) -> &[(AuraId, u32)] {
        &self.auras[..self.aura_count as usize]
    }

    pub fn set_auras(&mut self, list: impl Iterator<Item = (AuraId, u32)>) {
        self.aura_count = 0;
        for a in list.take(MAX_SNAP_AURAS) {
            self.auras[self.aura_count as usize] = a;
            self.aura_count += 1;
        }
    }
}

mod field {
    pub const POS_XZ: u32 = 1 << 0;
    pub const POS_Y: u32 = 1 << 1;
    pub const FACING: u32 = 1 << 2;
    pub const HP: u32 = 1 << 3;
    pub const MAX_HP: u32 = 1 << 4;
    pub const MANA: u32 = 1 << 5;
    pub const FLAGS: u32 = 1 << 6;
    pub const CAST: u32 = 1 << 7;
    pub const AURAS: u32 = 1 << 8;
    pub const LEVEL: u32 = 1 << 9;
    pub const ALL: u32 = (1 << 10) - 1;
    pub const BITS: u32 = 10;
}

fn changed(a: &EntState, b: &EntState) -> u32 {
    let mut m = 0;
    if a.pos[0] != b.pos[0] || a.pos[2] != b.pos[2] {
        m |= field::POS_XZ;
    }
    if a.pos[1] != b.pos[1] {
        m |= field::POS_Y;
    }
    if a.facing != b.facing {
        m |= field::FACING;
    }
    if a.hp != b.hp {
        m |= field::HP;
    }
    if a.max_hp != b.max_hp {
        m |= field::MAX_HP;
    }
    if a.mana != b.mana || a.max_mana != b.max_mana {
        m |= field::MANA;
    }
    if a.flags != b.flags {
        m |= field::FLAGS;
    }
    if a.cast != b.cast {
        m |= field::CAST;
    }
    if a.auras() != b.auras() {
        m |= field::AURAS;
    }
    if a.level != b.level {
        m |= field::LEVEL;
    }
    m
}

/// One entity in a snapshot being built: its current state, whether it is
/// due an update this tick (tiered rates), and its name for first sight.
pub struct SnapEntity<'a> {
    pub id: EntityId,
    pub state: EntState,
    pub due: bool,
    pub name: &'a str,
}

/// Encode a snapshot for one client. `view` must be sorted by id and
/// `baseline` (the snapshot the client acknowledged) likewise. Returns the
/// packet and the state the client will hold once it decodes it, which is
/// what the server keeps as a future baseline.
pub fn encode_snapshot(
    snap_id: u32,
    server_time: u32,
    baseline: Option<(u32, &[(EntityId, EntState)])>,
    view: &[SnapEntity],
) -> (Vec<u8>, Vec<(EntityId, EntState)>) {
    let mut w = BitWriter::with_capacity(64 + view.len() * 6);
    let (base_id, base) = baseline.unwrap_or((0, &[]));
    w.write(snap_id, 32);
    w.write(base_id, 32);
    w.write(server_time, 32);

    let mut known = Vec::with_capacity(view.len());
    let mut records = BitWriter::with_capacity(view.len() * 6);
    let mut count = 0u32;
    let mut prev_id = 0u32;
    let mut bi = 0;
    let mut removed = Vec::new();
    for e in view {
        while bi < base.len() && base[bi].0 < e.id {
            removed.push(base[bi].0);
            bi += 1;
        }
        let old = (bi < base.len() && base[bi].0 == e.id).then(|| {
            bi += 1;
            base[bi - 1].1
        });
        match old {
            Some(old) if !e.due => known.push((e.id, old)),
            Some(old) => {
                let mask = changed(&old, &e.state);
                if mask != 0 {
                    records.var(e.id - prev_id);
                    prev_id = e.id;
                    records.bit(false);
                    write_fields(&mut records, mask, &e.state, Some(&old));
                    count += 1;
                }
                known.push((e.id, e.state));
            }
            None => {
                records.var(e.id - prev_id);
                prev_id = e.id;
                records.bit(true);
                records.write(e.state.kind as u32, 2);
                records.write(e.state.def as u32, 16);
                let name = e.name.as_bytes();
                let n = name.len().min(32);
                records.write(n as u32, 6);
                for b in &name[..n] {
                    records.write(*b as u32, 8);
                }
                write_fields(&mut records, field::ALL, &e.state, None);
                count += 1;
                known.push((e.id, e.state));
            }
        }
    }
    removed.extend(base[bi..].iter().map(|(id, _)| *id));

    w.var(count);
    let rec = records.finish();
    // Records are bit-packed; append them byte by byte after the header.
    w.var(rec.len() as u32);
    for b in &rec {
        w.write(*b as u32, 8);
    }
    w.var(removed.len() as u32);
    let mut prev = 0;
    for id in removed {
        w.var(id - prev);
        prev = id;
    }
    (w.finish(), known)
}

fn write_fields(w: &mut BitWriter, mask: u32, s: &EntState, old: Option<&EntState>) {
    if old.is_some() {
        w.write(mask, field::BITS);
    }
    if mask & field::POS_XZ != 0 {
        let small = old.and_then(|o| {
            let dx = s.pos[0] as i32 - o.pos[0] as i32;
            let dz = s.pos[2] as i32 - o.pos[2] as i32;
            (dx.abs() < 256 && dz.abs() < 256).then_some((dx, dz))
        });
        match small {
            Some((dx, dz)) => {
                w.bit(true);
                write_signed(w, dx, 9);
                write_signed(w, dz, 9);
            }
            None => {
                w.bit(false);
                w.write(s.pos[0] as u32, 16);
                w.write(s.pos[2] as u32, 16);
            }
        }
    }
    if mask & field::POS_Y != 0 {
        w.write(s.pos[1] as u32, 16);
    }
    if mask & field::FACING != 0 {
        w.write(s.facing as u32, 8);
    }
    if mask & field::HP != 0 {
        w.var(s.hp);
    }
    if mask & field::MAX_HP != 0 {
        w.var(s.max_hp);
    }
    if mask & field::MANA != 0 {
        w.var(s.mana);
        w.var(s.max_mana);
    }
    if mask & field::FLAGS != 0 {
        w.write(s.flags as u32, flags::ALL_BITS);
    }
    if mask & field::CAST != 0 {
        match s.cast {
            Some((ab, t)) => {
                w.bit(true);
                w.write(ab as u32, 16);
                w.write(t, 32);
            }
            None => w.bit(false),
        }
    }
    if mask & field::AURAS != 0 {
        w.write(s.aura_count as u32, 4);
        for (a, t) in s.auras() {
            w.write(*a as u32, 16);
            w.write(*t, 32);
        }
    }
    if mask & field::LEVEL != 0 {
        w.write(s.level as u32, 8);
    }
}

fn read_fields(r: &mut BitReader, s: &mut EntState, full: bool) -> Option<u32> {
    let mask = if full { field::ALL } else { r.read(field::BITS)? };
    if mask & field::POS_XZ != 0 {
        if r.bit()? {
            let dx = read_signed(r, 9)?;
            let dz = read_signed(r, 9)?;
            s.pos[0] = (s.pos[0] as i32 + dx) as u16;
            s.pos[2] = (s.pos[2] as i32 + dz) as u16;
        } else {
            s.pos[0] = r.read(16)? as u16;
            s.pos[2] = r.read(16)? as u16;
        }
    }
    if mask & field::POS_Y != 0 {
        s.pos[1] = r.read(16)? as u16;
    }
    if mask & field::FACING != 0 {
        s.facing = r.read(8)? as u8;
    }
    if mask & field::HP != 0 {
        s.hp = r.var()?;
    }
    if mask & field::MAX_HP != 0 {
        s.max_hp = r.var()?;
    }
    if mask & field::MANA != 0 {
        s.mana = r.var()?;
        s.max_mana = r.var()?;
    }
    if mask & field::FLAGS != 0 {
        s.flags = r.read(flags::ALL_BITS)? as u8;
    }
    if mask & field::CAST != 0 {
        s.cast = if r.bit()? {
            Some((r.read(16)? as u16, r.read(32)?))
        } else {
            None
        };
    }
    if mask & field::AURAS != 0 {
        let n = (r.read(4)? as usize).min(MAX_SNAP_AURAS);
        s.aura_count = n as u8;
        for i in 0..n {
            s.auras[i] = (r.read(16)? as u16, r.read(32)?);
        }
    }
    if mask & field::LEVEL != 0 {
        s.level = r.read(8)? as u8;
    }
    Some(mask)
}

#[derive(Debug, Default)]
pub struct DecodedSnapshot {
    pub snap_id: u32,
    pub baseline: u32,
    pub server_time: u32,
    /// Full entity state after applying the delta, sorted by id.
    pub entities: Vec<(EntityId, EntState)>,
    /// Entities whose position was sent in this snapshot.
    pub moved: Vec<EntityId>,
    /// (id, name) of entities seen for the first time.
    pub entered: Vec<(EntityId, String)>,
}

/// Peek at the baseline id a snapshot was encoded against.
pub fn snapshot_baseline(bytes: &[u8]) -> Option<u32> {
    let mut r = BitReader::new(bytes);
    r.read(32)?;
    r.read(32)
}

/// Decode a snapshot against `baseline` (which must be the state of the
/// snapshot whose id the header names, or empty when it names 0).
pub fn decode_snapshot(bytes: &[u8], baseline: &[(EntityId, EntState)]) -> Option<DecodedSnapshot> {
    let mut r = BitReader::new(bytes);
    let snap_id = r.read(32)?;
    let base_id = r.read(32)?;
    let server_time = r.read(32)?;
    let count = r.var()?;
    let rec_len = r.var()? as usize;
    let mut rec = Vec::with_capacity(rec_len);
    for _ in 0..rec_len {
        rec.push(r.read(8)? as u8);
    }
    let removed_n = r.var()?;
    let mut removed = Vec::with_capacity(removed_n as usize);
    let mut prev = 0;
    for _ in 0..removed_n {
        prev += r.var()?;
        removed.push(prev);
    }

    let mut out = DecodedSnapshot {
        snap_id,
        baseline: base_id,
        server_time,
        entities: Vec::with_capacity(baseline.len() + count as usize),
        ..Default::default()
    };
    let mut rr = BitReader::new(&rec);
    let mut updates: Vec<(EntityId, EntState)> = Vec::with_capacity(count as usize);
    let mut id = 0u32;
    for _ in 0..count {
        id += rr.var()?;
        let full = rr.bit()?;
        let mut s;
        if full {
            s = EntState {
                kind: if rr.read(2)? == 1 {
                    EntityKind::Mob
                } else {
                    EntityKind::Player
                },
                def: rr.read(16)? as u16,
                ..Default::default()
            };
            let n = rr.read(6)? as usize;
            let mut name = Vec::with_capacity(n);
            for _ in 0..n {
                name.push(rr.read(8)? as u8);
            }
            out.entered
                .push((id, String::from_utf8_lossy(&name).into_owned()));
        } else {
            s = baseline
                .binary_search_by_key(&id, |(i, _)| *i)
                .ok()
                .map(|i| baseline[i].1)?;
        }
        let mask = read_fields(&mut rr, &mut s, full)?;
        if mask & (field::POS_XZ | field::POS_Y) != 0 {
            out.moved.push(id);
        }
        updates.push((id, s));
    }

    // Merge: baseline minus removed, overwritten/extended by updates.
    let (mut bi, mut ui) = (0, 0);
    while bi < baseline.len() || ui < updates.len() {
        let take_update = match (baseline.get(bi), updates.get(ui)) {
            (Some(b), Some(u)) => u.0 <= b.0,
            (None, Some(_)) => true,
            _ => false,
        };
        if take_update {
            let u = updates[ui];
            if baseline.get(bi).is_some_and(|b| b.0 == u.0) {
                bi += 1;
            }
            out.entities.push(u);
            ui += 1;
        } else {
            let b = baseline[bi];
            if !removed.contains(&b.0) {
                out.entities.push(b);
            }
            bi += 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(x: f32, hp: u32) -> EntState {
        EntState {
            kind: EntityKind::Mob,
            def: 3,
            level: 5,
            pos: quantize_pos([x, 0.0, -x]),
            hp,
            max_hp: 1000,
            ..Default::default()
        }
    }

    #[test]
    fn quantize_round_trip_within_a_centimetre() {
        for v in [-600.0f32, -12.345, 0.0, 0.011, 99.99, 600.0] {
            assert!((dequantize(quantize(v)) - v).abs() <= POS_STEP / 2.0 + 1e-4);
        }
    }

    #[test]
    fn move_round_trip() {
        let m = MoveInput {
            seq: 77,
            ack: 12,
            pos: [10.5, 0.0, -3.25],
            facing: 1.0,
            moving: true,
            target: 1234,
        };
        let d = MoveInput::decode(&m.encode()).unwrap();
        assert_eq!(d.seq, 77);
        assert_eq!(d.ack, 12);
        assert!(d.moving);
        assert_eq!(d.target, 1234);
        assert!((d.pos[0] - 10.5).abs() < 0.02);
        assert!((d.facing - 1.0).abs() < 0.03);
    }

    #[test]
    fn delta_snapshot_round_trip() {
        let view1 = vec![
            SnapEntity { id: 5, state: st(1.0, 1000), due: true, name: "" },
            SnapEntity { id: 9, state: st(2.0, 1000), due: true, name: "Bob" },
            SnapEntity { id: 300, state: st(3.0, 1000), due: true, name: "" },
        ];
        let (b1, known1) = encode_snapshot(1, 100, None, &view1);
        let d1 = decode_snapshot(&b1, &[]).unwrap();
        assert_eq!(d1.entities, known1);
        assert_eq!(d1.entered.len(), 3);
        assert_eq!(d1.entered[1].1, "Bob");

        // 5 moves, 9 is not due this tick (keeps old state), 300 leaves, 400 enters.
        let mut s5 = st(1.3, 990);
        s5.cast = Some((2, 150));
        s5.set_auras([(1u16, 9000u32)].into_iter());
        let view2 = vec![
            SnapEntity { id: 5, state: s5, due: true, name: "" },
            SnapEntity { id: 9, state: st(9.0, 1), due: false, name: "Bob" },
            SnapEntity { id: 400, state: st(4.0, 50), due: true, name: "" },
        ];
        let (b2, known2) = encode_snapshot(2, 150, Some((1, &known1)), &view2);
        let d2 = decode_snapshot(&b2, &d1.entities).unwrap();
        assert_eq!(d2.entities, known2);
        assert_eq!(d2.entities.len(), 3);
        assert_eq!(d2.entities[1].1, known1[1].1, "not-due entity keeps baseline");
        assert_eq!(d2.moved, vec![5, 400]);
        assert!(b2.len() < b1.len() + 20);

        // Nothing changes: the snapshot is just a header.
        let (b3, _) = encode_snapshot(3, 200, Some((2, &known2)), &view2);
        assert!(b3.len() <= 16, "{} bytes", b3.len());
    }

    #[test]
    fn walking_entity_costs_few_bytes() {
        let a = st(1.0, 1000);
        let mut b = a;
        b.pos = quantize_pos([1.35, 0.0, -1.35]);
        b.facing = 40;
        let base = vec![(7, a)];
        let view = vec![SnapEntity { id: 7, state: b, due: true, name: "" }];
        let (empty, _) = encode_snapshot(2, 0, Some((1, &base)), &[SnapEntity { id: 7, state: a, due: true, name: "" }]);
        let (bytes, _) = encode_snapshot(2, 0, Some((1, &base)), &view);
        assert!(bytes.len() - empty.len() <= 6, "{} bytes", bytes.len() - empty.len());
    }
}
