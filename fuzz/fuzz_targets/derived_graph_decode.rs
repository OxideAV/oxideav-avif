#![no_main]

//! Derived-image graph fuzz: synthesise a HEIF `meta` from the fuzz
//! bytes — `av01` / `grid` / `iovl` / `iden` items, alpha auxiliaries,
//! fuzz-chosen `dimg` / `auxl` / `prem` edges (cycles, self references,
//! counts that disagree with descriptors, off-canvas and overlapping
//! placements, oversize canvases, `lsel` / `a1op` selectors, `clap` /
//! `irot` / `imir` chains) around one valid pre-encoded AV1 tile — and
//! push it through the recursive derived-image decoder, `inspect`, and
//! the derivation-graph walker. The invariant is "no panic, bounded
//! memory": every hostile shape must surface as an `Err`.

use libfuzzer_sys::fuzz_target;
use oxideav_avif::{encode_still, parse, AvifDecoder, StillChroma, StillEncodeOptions, StillImage};
use oxideav_core::{CodecId, Decoder, Packet, TimeBase};
use std::sync::OnceLock;

/// `(colour payload, colour av1C, mono payload, mono av1C)` of one
/// lossless 8×8 tile pair, encoded once.
fn tiles() -> &'static (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>) {
    static TILES: OnceLock<(Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>)> = OnceLock::new();
    TILES.get_or_init(|| {
        let y: Vec<u16> = (0..64).map(|i| (i * 3) as u16).collect();
        let c: Vec<u16> = (0..16).map(|i| 100 + i as u16).collect();
        let colour = StillImage::yuv(8, 8, 8, StillChroma::Yuv420, y.clone(), c.clone(), c).unwrap();
        let mono = StillImage::yuv(8, 8, 8, StillChroma::Monochrome, y, vec![], vec![]).unwrap();
        let cf = encode_still(&colour, &StillEncodeOptions::default()).unwrap();
        let mf = encode_still(&mono, &StillEncodeOptions::default()).unwrap();
        let cp = parse(&cf).unwrap();
        let mp = parse(&mf).unwrap();
        (
            cp.primary_item_data.to_vec(),
            cp.av1c.clone().unwrap(),
            mp.primary_item_data.to_vec(),
            mp.av1c.clone().unwrap(),
        )
    })
}

struct Bytes<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Bytes<'_> {
    fn u8(&mut self) -> u8 {
        let b = self.data.get(self.pos).copied().unwrap_or(0);
        self.pos += 1;
        b
    }
    fn u16(&mut self) -> u16 {
        (u16::from(self.u8()) << 8) | u16::from(self.u8())
    }
}

fn boxed(t: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = ((8 + body.len()) as u32).to_be_bytes().to_vec();
    out.extend_from_slice(t);
    out.extend_from_slice(body);
    out
}

fn full(t: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
    let mut inner = vec![version, (flags >> 16) as u8, (flags >> 8) as u8, flags as u8];
    inner.extend_from_slice(body);
    boxed(t, &inner)
}

struct Item {
    id: u16,
    item_type: [u8; 4],
    hidden: bool,
    payload: Vec<u8>,
    /// Property boxes + essential flags.
    props: Vec<(Vec<u8>, bool)>,
}

struct Ref {
    kind: [u8; 4],
    from: u16,
    to: Vec<u16>,
}

fuzz_target!(|data: &[u8]| {
    if data.len() < 4 {
        return;
    }
    let (cpay, cav1c, mpay, mav1c) = tiles();
    let mut b = Bytes { data, pos: 0 };
    let n = 1 + usize::from(b.u8() & 7);
    let mut items = Vec::with_capacity(n);
    let mut refs = Vec::new();
    for i in 0..n {
        let id = i as u16 + 1;
        let kind = b.u8() % 6;
        let flags = b.u8();
        let hidden = flags & 1 != 0;
        // Dimensions bounded to 11 bits: hostile enough to exercise the
        // canvas caps' neighbourhood without breaching the fuzz memory
        // bound per derivation level.
        let w = u32::from(b.u16() & 0x7FF);
        let h = u32::from(b.u16() & 0x7FF);
        let mut props: Vec<(Vec<u8>, bool)> = Vec::new();
        let mut ispe = Vec::new();
        ispe.extend_from_slice(&w.to_be_bytes());
        ispe.extend_from_slice(&h.to_be_bytes());
        let (item_type, payload) = match kind {
            0 | 5 => {
                props.push((boxed(b"av1C", cav1c), true));
                (*b"av01", cpay.clone())
            }
            1 => {
                props.push((boxed(b"av1C", mav1c), true));
                if flags & 2 != 0 {
                    props.push((
                        full(b"auxC", 0, 0, b"urn:mpeg:mpegB:cicp:systems:auxiliary:alpha\0"),
                        false,
                    ));
                    let to = b.u8() as u16 % n as u16 + 1;
                    refs.push(Ref {
                        kind: *b"auxl",
                        from: id,
                        to: vec![to],
                    });
                    if flags & 4 != 0 {
                        refs.push(Ref {
                            kind: *b"prem",
                            from: id,
                            to: vec![to],
                        });
                    }
                }
                (*b"av01", mpay.clone())
            }
            2 => {
                let rows = b.u8() & 3;
                let cols = b.u8() & 3;
                let wide = flags & 8 != 0;
                let mut d = vec![0u8, u8::from(wide), rows, cols];
                if wide {
                    d.extend_from_slice(&w.to_be_bytes());
                    d.extend_from_slice(&h.to_be_bytes());
                } else {
                    d.extend_from_slice(&(w as u16).to_be_bytes());
                    d.extend_from_slice(&(h as u16).to_be_bytes());
                }
                (*b"grid", d)
            }
            3 => {
                let count = usize::from(b.u8() & 3) + 1;
                let wide = flags & 8 != 0;
                let mut d = vec![0u8, u8::from(wide)];
                for _ in 0..4 {
                    d.extend_from_slice(&b.u16().to_be_bytes());
                }
                if wide {
                    d.extend_from_slice(&w.to_be_bytes());
                    d.extend_from_slice(&h.to_be_bytes());
                } else {
                    d.extend_from_slice(&(w as u16).to_be_bytes());
                    d.extend_from_slice(&(h as u16).to_be_bytes());
                }
                let mut to = Vec::with_capacity(count);
                for _ in 0..count {
                    let ox = i16::from(b.u8() as i8) * 8;
                    let oy = i16::from(b.u8() as i8) * 8;
                    if wide {
                        d.extend_from_slice(&i32::from(ox).to_be_bytes());
                        d.extend_from_slice(&i32::from(oy).to_be_bytes());
                    } else {
                        d.extend_from_slice(&ox.to_be_bytes());
                        d.extend_from_slice(&oy.to_be_bytes());
                    }
                    to.push(b.u8() as u16 % n as u16 + 1);
                }
                // The descriptor's entry count may disagree with the
                // dimg count (fuzz decides).
                if flags & 16 != 0 {
                    to.push(b.u8() as u16 % n as u16 + 1);
                }
                refs.push(Ref {
                    kind: *b"dimg",
                    from: id,
                    to,
                });
                (*b"iovl", d)
            }
            _ => {
                let count = usize::from(b.u8() & 3);
                let to: Vec<u16> = (0..count).map(|_| b.u8() as u16 % n as u16 + 1).collect();
                refs.push(Ref {
                    kind: *b"dimg",
                    from: id,
                    to,
                });
                (*b"iden", Vec::new())
            }
        };
        if kind == 2 {
            let count = usize::from(b.u8() & 15);
            let to: Vec<u16> = (0..count).map(|_| b.u8() as u16 % n as u16 + 1).collect();
            refs.push(Ref {
                kind: *b"dimg",
                from: id,
                to,
            });
        }
        if kind == 5 {
            props.push((boxed(b"lsel", &b.u16().to_be_bytes()), true));
            props.push((boxed(b"a1op", &[b.u8() & 3]), true));
        }
        if flags & 32 == 0 {
            props.push((full(b"ispe", 0, 0, &ispe), false));
        }
        let xf = b.u8();
        if xf & 1 != 0 {
            let mut clap = Vec::new();
            for _ in 0..8 {
                clap.extend_from_slice(&i32::from(b.u8() as i8).to_be_bytes());
            }
            props.push((boxed(b"clap", &clap), true));
        }
        if xf & 2 != 0 {
            props.push((boxed(b"irot", &[b.u8() & 3]), true));
        }
        if xf & 4 != 0 {
            props.push((boxed(b"imir", &[b.u8() & 1]), true));
        }
        if xf & 8 != 0 {
            props.push((full(b"pixi", 0, 0, &[1, 8]), false));
        }
        items.push(Item {
            id,
            item_type,
            hidden,
            payload,
            props,
        });
    }
    let pitm = b.u8() as u16 % n as u16 + 1;

    // ---- assemble ----
    let mut mdat = Vec::new();
    let mut offsets = Vec::with_capacity(items.len());
    for it in &items {
        offsets.push(mdat.len() as u32);
        mdat.extend_from_slice(&it.payload);
    }
    let build_meta = |mdat_start: u32| -> Vec<u8> {
        let mut body = Vec::new();
        let mut hdlr = vec![0u8; 4];
        hdlr.extend_from_slice(b"pict");
        hdlr.extend_from_slice(&[0u8; 12]);
        hdlr.push(0);
        body.extend_from_slice(&full(b"hdlr", 0, 0, &hdlr));
        body.extend_from_slice(&full(b"pitm", 0, 0, &pitm.to_be_bytes()));
        let mut iinf = (items.len() as u16).to_be_bytes().to_vec();
        for it in &items {
            let mut infe = it.id.to_be_bytes().to_vec();
            infe.extend_from_slice(&[0, 0]);
            infe.extend_from_slice(&it.item_type);
            infe.push(0);
            iinf.extend_from_slice(&full(b"infe", 2, u32::from(it.hidden), &infe));
        }
        body.extend_from_slice(&full(b"iinf", 0, 0, &iinf));
        if !refs.is_empty() {
            let mut iref = Vec::new();
            for r in &refs {
                let mut e = r.from.to_be_bytes().to_vec();
                e.extend_from_slice(&(r.to.len() as u16).to_be_bytes());
                for t in &r.to {
                    e.extend_from_slice(&t.to_be_bytes());
                }
                iref.extend_from_slice(&boxed(&r.kind, &e));
            }
            body.extend_from_slice(&full(b"iref", 0, 0, &iref));
        }
        let mut ipco = Vec::new();
        let mut ipma = (items.len() as u32).to_be_bytes().to_vec();
        let mut index = 0u8;
        for it in &items {
            ipma.extend_from_slice(&it.id.to_be_bytes());
            ipma.push(it.props.len() as u8);
            for (bytes, essential) in &it.props {
                ipco.extend_from_slice(bytes);
                index = index.wrapping_add(1);
                ipma.push((if *essential { 0x80 } else { 0 }) | (index & 0x7f));
            }
        }
        let mut iprp = boxed(b"ipco", &ipco);
        iprp.extend_from_slice(&full(b"ipma", 0, 0, &ipma));
        body.extend_from_slice(&boxed(b"iprp", &iprp));
        let mut iloc = vec![0x44u8, 0x00];
        let located: Vec<(&Item, u32)> = items
            .iter()
            .zip(&offsets)
            .filter(|(it, _)| it.item_type != *b"iden")
            .map(|(it, &o)| (it, o))
            .collect();
        iloc.extend_from_slice(&(located.len() as u16).to_be_bytes());
        for (it, off) in located {
            iloc.extend_from_slice(&it.id.to_be_bytes());
            iloc.extend_from_slice(&[0, 0, 0, 1]);
            iloc.extend_from_slice(&(mdat_start + off).to_be_bytes());
            iloc.extend_from_slice(&(it.payload.len() as u32).to_be_bytes());
        }
        body.extend_from_slice(&full(b"iloc", 0, 0, &iloc));
        full(b"meta", 0, 0, &body)
    };
    let mut ftyp = b"avif".to_vec();
    ftyp.extend_from_slice(&[0, 0, 0, 0]);
    ftyp.extend_from_slice(b"avifmif1miaf");
    let ftyp = boxed(b"ftyp", &ftyp);
    let probe = build_meta(0);
    let start = (ftyp.len() + probe.len() + 8) as u32;
    let meta = build_meta(start);
    let mut file = ftyp;
    file.extend_from_slice(&meta);
    file.extend_from_slice(&boxed(b"mdat", &mdat));

    // ---- exercise ----
    let _ = oxideav_avif::inspect(&file);
    let _ = oxideav_avif::derivation_graph(&file);
    let mut dec = AvifDecoder::new(CodecId::new(oxideav_avif::CODEC_ID_STR));
    let pkt = Packet::new(0, TimeBase::new(1, 1), file);
    let _ = dec.send_packet(&pkt);
    let _ = dec.receive_frame();
});
