//! # encode-test
//!
//! Outil de test isolé pour valider l'encodage H.264 de NIDAN.
//!
//! Génère (ou charge) des frames, les encode en H.264 via openh264,
//! et écrit un fichier .h264 (Annex B) lisible par ffplay/VLC.
//!
//! ## Usage
//! ```bash
//! encode-test                      # 30 frames synthétiques animées
//! encode-test out.h264 60          # 60 frames vers out.h264
//! ```
//!
//! ## Vérification
//! ```bash
//! ffplay out.h264                  # lecture
//! ffprobe out.h264                 # infos du flux
//! ```

use std::io::Write;

use anyhow::{Context, Result};
use openh264::encoder::{Encoder, EncoderConfig};
use openh264::formats::{RgbSliceU8, YUVBuffer};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let output = args.get(1).cloned().unwrap_or_else(|| "nidan_test.h264".to_string());
    let n_frames: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(30);

    let width = 1280u32;
    let height = 720u32;

    println!("╭──────────────────────────────────────────────");
    println!("│ NIDAN — Test d'encodage H.264");
    println!("│ Sortie     : {}", output);
    println!("│ Résolution : {}x{}", width, height);
    println!("│ Frames     : {}", n_frames);
    println!("╰──────────────────────────────────────────────");

    // ── Configuration de l'encodeur ──────────────────────────────────────────
    let _config = EncoderConfig::new();
    let mut encoder = Encoder::new()
        .context("création de l'encodeur openh264")?;

    println!("\n✓ Encodeur H.264 initialisé (openh264)");

    let mut out_file = std::fs::File::create(&output)
        .with_context(|| format!("création {}", output))?;

    // ── Boucle d'encodage ──────────────────────────────────────────────────────
    println!("\n  Encodage en cours...");
    let mut total_bytes = 0usize;
    let mut keyframes = 0u32;
    let start = std::time::Instant::now();

    for i in 0..n_frames {
        // Génère une frame RGB animée (dégradé + carré mobile)
        let rgb = generate_frame(width, height, i);

        // Conversion RGB → YUV420 (format attendu par H.264)
        let rgb_source = RgbSliceU8::new(&rgb, (width as usize, height as usize));
        let yuv = YUVBuffer::from_rgb_source(rgb_source);

        // Encodage de la frame
        let t = std::time::Instant::now();
        let bitstream = encoder.encode(&yuv)
            .with_context(|| format!("encodage frame {}", i))?;
        let encode_us = t.elapsed().as_micros();

        // Écriture du flux Annex B
        let data = bitstream.to_vec();
        out_file.write_all(&data).context("écriture flux H.264")?;

        total_bytes += data.len();
        // Une frame avec beaucoup de données est généralement une keyframe (IDR)
        let is_kf = data.len() > (width * height / 20) as usize;
        if is_kf { keyframes += 1; }

        if i < 5 || i % 10 == 0 {
            println!(
                "    Frame {:3} → {:6} octets ({} µs){}",
                i, data.len(), encode_us,
                if is_kf { "  [keyframe]" } else { "" }
            );
        }
    }

    out_file.flush()?;
    let total_s = start.elapsed().as_secs_f64();

    // ── Résumé ──────────────────────────────────────────────────────────────────
    let avg_kbps = (total_bytes as f64 * 8.0) / (total_s * 1000.0);
    println!("\n╭──────────────────────────────────────────────");
    println!("│ ✓ Encodage terminé");
    println!("│   {} frames en {:.2} s ({:.1} fps)", n_frames, total_s, n_frames as f64 / total_s);
    println!("│   {:.1} Ko produits ({} keyframes)", total_bytes as f64 / 1024.0, keyframes);
    println!("│   Débit moyen : {:.0} kbps", avg_kbps);
    println!("│");
    println!("│ Vérifier : ffplay {}", output);
    println!("│       ou : ffprobe {}", output);
    println!("╰──────────────────────────────────────────────");

    Ok(())
}

/// Génère une frame RGB de test : dégradé animé + carré mobile blanc
fn generate_frame(width: u32, height: u32, frame: u32) -> Vec<u8> {
    let mut rgb = vec![0u8; (width * height * 3) as usize];

    // Position du carré mobile
    let box_size = 120u32;
    let max_x = width - box_size;
    let max_y = height - box_size;
    let bx = (frame * 13) % max_x;
    let by = (frame * 7) % max_y;

    for y in 0..height {
        for x in 0..width {
            let idx = ((y * width + x) * 3) as usize;

            // Dégradé de fond animé
            let r = ((x * 255 / width) + frame * 2) % 256;
            let g = ((y * 255 / height) + frame) % 256;
            let b = (frame * 3) % 256;

            // Carré blanc mobile par-dessus
            if x >= bx && x < bx + box_size && y >= by && y < by + box_size {
                rgb[idx] = 255;
                rgb[idx + 1] = 255;
                rgb[idx + 2] = 255;
            } else {
                rgb[idx] = r as u8;
                rgb[idx + 1] = g as u8;
                rgb[idx + 2] = b as u8;
            }
        }
    }

    rgb
}
