//! Diagnostic: try to end-to-end decode an AVIF via oxideav-av1, and
//! print a detailed report of what parts of the pipeline succeed vs
//! fail. Useful when the AV1 decoder still errors out on rich content
//! — the output lets us characterise exactly which stage of the HEIF
//! pass → OBU extraction → AV1 decode pipeline breaks on a given
//! input.
//!
//! Usage: `cargo run --example diag_decode -p oxideav-avif -- <file.avif> [...]`

use std::path::PathBuf;

use oxideav_av1::{iter_obus, Av1CodecConfig, ObuType};
use oxideav_avif::meta::Property;
use oxideav_avif::parser::item_bytes;
use oxideav_avif::{find_alpha_item_id, inspect, parse, parse_header, AvifDecoder};
use oxideav_core::Decoder;
use oxideav_core::{CodecId, Frame, Packet, TimeBase};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: diag_decode <file.avif> [...]");
        std::process::exit(2);
    }
    for a in &args {
        probe(&PathBuf::from(a));
    }
}

fn probe(path: &PathBuf) {
    println!("==== {} ====", path.display());
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            println!("  read error: {e}");
            return;
        }
    };
    println!("  file_size={}", bytes.len());

    // 1. inspect: HEIF box walk + primary-item extraction.
    let info = match inspect(&bytes) {
        Ok(i) => i,
        Err(e) => {
            println!("  inspect() FAILED: {e}");
            return;
        }
    };
    println!(
        "  inspect: {}x{} bpc={:?} av1c_len={} obu_len={} grid={} alpha={}",
        info.width,
        info.height,
        info.bits_per_channel,
        info.av1c.len(),
        info.obu_bytes.len(),
        info.is_grid,
        info.has_alpha,
    );

    // 2. Decode the av1C record — print seq header summary.
    match Av1CodecConfig::parse(&info.av1c) {
        Ok(cfg) => {
            let depth = if cfg.twelve_bit {
                12
            } else if cfg.high_bitdepth {
                10
            } else {
                8
            };
            println!(
                "  av1C: profile={} level={} tier={} depth={} mono={} chroma_sub=({},{}) has_seq_header={}",
                cfg.seq_profile,
                cfg.seq_level_idx_0,
                cfg.seq_tier_0,
                depth,
                cfg.monochrome,
                cfg.chroma_subsampling_x as u8,
                cfg.chroma_subsampling_y as u8,
                cfg.seq_header.is_some(),
            );
        }
        Err(e) => println!("  av1C parse FAILED: {e}"),
    }

    // 3. Walk the OBUs in the primary item.
    if !info.obu_bytes.is_empty() {
        let mut count = 0usize;
        let mut by_type = std::collections::BTreeMap::<u8, usize>::new();
        let mut walk_err: Option<String> = None;
        for o in iter_obus(&info.obu_bytes) {
            match o {
                Ok(obu) => {
                    count += 1;
                    *by_type.entry(obu.header.obu_type as u8).or_insert(0) += 1;
                }
                Err(e) => {
                    walk_err = Some(format!("{e}"));
                    break;
                }
            }
        }
        let mut breakdown = Vec::new();
        for (t, n) in &by_type {
            let name = ObuType::from_u8(*t).name();
            breakdown.push(format!("{}={}", name, n));
        }
        match walk_err {
            Some(e) => println!(
                "  obu walk FAILED after {count}: {e} ({})",
                breakdown.join(" ")
            ),
            None => println!("  obus: total={} {}", count, breakdown.join(" ")),
        }
    }

    // 3b. If the file carries an alpha auxiliary, probe its OBU stream too.
    if info.has_alpha {
        if let Ok(hdr) = parse_header(&bytes) {
            if let Some(primary_id) = hdr.meta.primary_item_id {
                if let Some(alpha_id) = find_alpha_item_id(&hdr.meta, primary_id) {
                    if let Some(loc) = hdr.meta.location_by_id(alpha_id) {
                        if let Ok(abytes) = item_bytes(&bytes, loc) {
                            let aav1c = match hdr.meta.property_for(alpha_id, b"av1C") {
                                Some(Property::Av1C(b)) => b.clone(),
                                _ => Vec::new(),
                            };
                            let aispe = match hdr.meta.property_for(alpha_id, b"ispe") {
                                Some(Property::Ispe(e)) => Some((e.width, e.height)),
                                _ => None,
                            };
                            println!(
                                "  alpha item id={alpha_id} bytes={} av1c_len={} ispe={:?}",
                                abytes.len(),
                                aav1c.len(),
                                aispe
                            );
                            let mut a_count = 0usize;
                            let mut a_by_type = std::collections::BTreeMap::<u8, usize>::new();
                            for o in iter_obus(abytes) {
                                match o {
                                    Ok(obu) => {
                                        a_count += 1;
                                        *a_by_type.entry(obu.header.obu_type as u8).or_insert(0) +=
                                            1;
                                    }
                                    Err(_) => break,
                                }
                            }
                            let mut bd = Vec::new();
                            for (t, n) in &a_by_type {
                                bd.push(format!("{}={}", ObuType::from_u8(*t).name(), n));
                            }
                            println!("  alpha obus: total={} {}", a_count, bd.join(" "));
                        }
                    }
                }
            }
        }
    }

    // 4. Attempt to parse the AVIF image (primary bytes + parent iprp).
    match parse(&bytes) {
        Ok(img) => {
            println!(
                "  parse: primary_len={} av1c={} ispe={:?}",
                img.primary_item_data.len(),
                img.av1c.as_ref().map(|b| b.len()).unwrap_or(0),
                img.ispe,
            );
        }
        Err(e) => println!("  parse FAILED (OK if grid): {e}"),
    }

    // 5. Full AvifDecoder — send packet, receive frame. Record
    //    exactly which stage returns what.
    let mut d = AvifDecoder::new(CodecId::new(oxideav_avif::CODEC_ID_STR));
    let pkt = Packet::new(0, TimeBase::new(1, 1), bytes.clone());
    let send_result =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| d.send_packet(&pkt)));
    match send_result {
        Ok(Ok(())) => {
            println!("  decode: send_packet OK");
            match d.receive_frame() {
                Ok(Frame::Video(vf)) => {
                    // Summarise pixels.
                    let mut plane_sums = Vec::new();
                    for (i, p) in vf.planes.iter().enumerate() {
                        let sum: u64 = p.data.iter().map(|&x| x as u64).sum();
                        let n = p.data.len().max(1) as u64;
                        let mean = sum as f64 / n as f64;
                        let mn = *p.data.iter().min().unwrap_or(&0);
                        let mx = *p.data.iter().max().unwrap_or(&0);
                        plane_sums.push(format!(
                            "p{i}: stride={} bytes={} mean={:.1} range={}..{}",
                            p.stride,
                            p.data.len(),
                            mean,
                            mn,
                            mx
                        ));
                    }
                    let inferred_w = vf.planes.first().map(|p| p.stride).unwrap_or(0);
                    let inferred_h = vf
                        .planes
                        .first()
                        .map(|p| p.data.len().checked_div(p.stride).unwrap_or(0))
                        .unwrap_or(0);
                    println!(
                        "  frame: {}x{} planes=[{}]",
                        inferred_w,
                        inferred_h,
                        plane_sums.join(" | "),
                    );
                }
                Ok(other) => println!("  frame: non-video {other:?}"),
                Err(e) => println!("  receive_frame FAILED: {e}"),
            }
        }
        Ok(Err(e)) => println!("  decode: send_packet FAILED: {e}"),
        Err(panic) => {
            let msg = panic
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| panic.downcast_ref::<&'static str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "<non-string panic>".to_string());
            println!("  decode: PANIC inside av1 decoder: {msg}");
        }
    }
}
