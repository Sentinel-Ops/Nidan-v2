//! # capture-test
//!
//! Outil de test isolé pour valider la capture X11 de NIDAN.
//! Gère les configurations multi-écrans via RandR : capture chaque
//! moniteur (CRTC) séparément pour éviter les erreurs GetImage Match
//! sur les setups multi-GPU.
//!
//! ## Usage
//! ```bash
//! capture-test           # capture sur $DISPLAY
//! capture-test 0         # capture sur :0
//! capture-test 0 5       # capture sur :0, 5 frames par moniteur
//! ```

use anyhow::{bail, Context, Result};
use x11rb::connection::{Connection, RequestConnection as _};
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    let display_str = if args.len() > 1 {
        format!(":{}", args[1])
    } else {
        std::env::var("DISPLAY").unwrap_or_else(|_| ":0".to_string())
    };
    let n_frames: u32 = if args.len() > 2 { args[2].parse().unwrap_or(3) } else { 3 };

    println!("╭──────────────────────────────────────────────");
    println!("│ NIDAN — Test de capture X11");
    println!("│ Display : {}", display_str);
    println!("│ Frames  : {} par moniteur", n_frames);
    println!("╰──────────────────────────────────────────────");

    let (conn, screen_num) = x11rb::connect(Some(&display_str))
        .with_context(|| format!("connexion X11 sur {} échouée", display_str))?;

    let screen = &conn.setup().roots[screen_num];
    let root = screen.root;

    println!("\n✓ Connecté au serveur X");
    println!("  Racine virtuelle : {}x{}", screen.width_in_pixels, screen.height_in_pixels);
    println!("  Profondeur       : {} bits", screen.root_depth);

    let has_randr = conn
        .extension_information(x11rb::protocol::randr::X11_EXTENSION_NAME)
        .ok().flatten().is_some();

    if !has_randr {
        println!("\n  RandR indisponible — capture de la racine entière");
        return capture_root(&conn, root, screen.width_in_pixels as u32,
                            screen.height_in_pixels as u32, n_frames);
    }

    // ── Énumération des moniteurs via RandR ──────────────────────────────────
    let resources = conn.randr_get_screen_resources_current(root)
        .context("get_screen_resources")?
        .reply()
        .context("get_screen_resources reply")?;

    let mut monitors = Vec::new();
    for &crtc in &resources.crtcs {
        let info = match conn.randr_get_crtc_info(crtc, 0)
            .and_then(|c| Ok(c.reply())) {
            Ok(Ok(info)) => info,
            _ => continue,
        };
        // Un CRTC actif a une largeur/hauteur non nulles et un mode défini
        if info.width > 0 && info.height > 0 && info.mode != 0 {
            monitors.push((info.x, info.y, info.width, info.height));
        }
    }

    if monitors.is_empty() {
        println!("\n  Aucun moniteur RandR actif — capture de la racine");
        return capture_root(&conn, root, screen.width_in_pixels as u32,
                            screen.height_in_pixels as u32, n_frames);
    }

    println!("\n  {} moniteur(s) actif(s) détecté(s) :", monitors.len());
    for (i, (x, y, w, h)) in monitors.iter().enumerate() {
        println!("    Moniteur {} : {}x{} à la position ({}, {})", i, w, h, x, y);
    }

    // ── Capture par moniteur ──────────────────────────────────────────────────
    println!("\n  Capture en cours...");
    for (mon_idx, (x, y, w, h)) in monitors.iter().enumerate() {
        for frame in 0..n_frames {
            let t = std::time::Instant::now();
            let reply = conn.get_image(
                ImageFormat::Z_PIXMAP, root,
                *x, *y, *w, *h, u32::MAX,
            ).context("get_image")?
             .reply()
             .with_context(|| format!("get_image moniteur {} échoué", mon_idx))?;

            let ms = t.elapsed().as_millis();
            let rgba = bgrx_to_rgba(&reply.data, *w as u32, *h as u32)?;
            let filename = format!("nidan_mon{}_frame{}.png", mon_idx, frame);
            save_png(&filename, &rgba, *w as u32, *h as u32)?;
            println!("    {} ({}x{}, {} ms)", filename, w, h, ms);

            std::thread::sleep(std::time::Duration::from_millis(300));
        }
    }

    println!("\n╭──────────────────────────────────────────────");
    println!("│ ✓ Capture terminée");
    println!("│   Ouvre les fichiers nidan_mon*_frame*.png");
    println!("│   pour vérifier la capture de chaque écran.");
    println!("╰──────────────────────────────────────────────");
    Ok(())
}

/// Capture la racine entière (fallback sans RandR)
fn capture_root(
    conn: &impl Connection,
    root: x11rb::protocol::xproto::Window,
    width: u32, height: u32, n_frames: u32,
) -> Result<()> {
    println!("\n  Capture racine {}x{}...", width, height);
    for frame in 0..n_frames {
        let t = std::time::Instant::now();
        let reply = conn.get_image(
            ImageFormat::Z_PIXMAP, root,
            0, 0, width as u16, height as u16, u32::MAX,
        ).context("get_image")?
         .reply()
         .context("get_image racine échoué")?;

        let ms = t.elapsed().as_millis();
        let rgba = bgrx_to_rgba(&reply.data, width, height)?;
        let filename = format!("nidan_capture_{}.png", frame);
        save_png(&filename, &rgba, width, height)?;
        println!("    {} ({}x{}, {} ms)", filename, width, height, ms);
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    Ok(())
}

/// Convertit BGRX (format X) → RGBA (format PNG)
fn bgrx_to_rgba(data: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    let expected = (width * height) as usize * 4;
    if data.len() < expected {
        bail!("taille image {} < attendu {}", data.len(), expected);
    }
    let mut rgba = Vec::with_capacity(expected);
    let mut idx = 0;
    while idx + 3 < expected {
        rgba.push(data[idx + 2]); // R
        rgba.push(data[idx + 1]); // G
        rgba.push(data[idx]);     // B
        rgba.push(255);           // A
        idx += 4;
    }
    Ok(rgba)
}

/// Sauvegarde RGBA en PNG
fn save_png(path: &str, rgba: &[u8], width: u32, height: u32) -> Result<()> {
    use std::fs::File;
    use std::io::BufWriter;
    let file = File::create(path)?;
    let w = BufWriter::new(file);
    let mut encoder = png::Encoder::new(w, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().context("en-tête PNG")?;
    writer.write_image_data(rgba).context("données PNG")?;
    Ok(())
}
