//! Boucle de rendu SDL2 pour NIDAN.
//!
//! Tourne dans le thread principal (exigence SDL2).
//! Gère la fenêtre, la texture vidéo, les événements et le HUD.

use std::sync::mpsc;
use std::time::Instant;

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

use crate::config::DisplayConfig;
use crate::decoder::DecodedFrame;
use crate::input::InputEvent;
use crate::renderer::{ConnectionStatus, RenderMetrics, RenderRect, ScalingMode};

/// Point d'entrée de la boucle SDL2
pub fn run_sdl2_loop(
    config: DisplayConfig,
    initial_width: u32,
    initial_height: u32,
    frame_rx: mpsc::Receiver<DecodedFrame>,
    input_tx: tokio::sync::mpsc::Sender<InputEvent>,
    metrics_tx: tokio::sync::watch::Sender<RenderMetrics>,
) -> Result<()> {
    #[cfg(all(feature = "sdl2-renderer", not(feature = "stub")))]
    {
        run_sdl2_real(config, initial_width, initial_height, frame_rx, input_tx, metrics_tx)
    }

    #[cfg(any(not(feature = "sdl2-renderer"), feature = "stub"))]
    {
        run_sdl2_stub(config, initial_width, initial_height, frame_rx, input_tx, metrics_tx)
    }
}

/// Implémentation SDL2 réelle
#[cfg(all(feature = "sdl2-renderer", not(feature = "stub")))]
fn run_sdl2_real(
    config: DisplayConfig,
    initial_width: u32,
    initial_height: u32,
    frame_rx: mpsc::Receiver<DecodedFrame>,
    input_tx: tokio::sync::mpsc::Sender<InputEvent>,
    metrics_tx: tokio::sync::watch::Sender<RenderMetrics>,
) -> Result<()> {
    use sdl2::event::Event;
    use sdl2::keyboard::Keycode;
    use sdl2::pixels::PixelFormatEnum;

    let sdl_context = sdl2::init().map_err(|e| anyhow::anyhow!("SDL2 init: {}", e))?;
    let video       = sdl_context.video().map_err(|e| anyhow::anyhow!("SDL2 video: {}", e))?;

    let mut window_builder = video.window(
        &config.window_title,
        initial_width,
        initial_height,
    );
    window_builder.resizable();
    if config.fullscreen {
        window_builder.fullscreen_desktop();
    }

    let window = window_builder.build().context("création fenêtre SDL2")?;
    let mut canvas = window
        .into_canvas()
        .accelerated()
        .present_vsync()
        .build()
        .context("création canvas SDL2")?;

    let texture_creator = canvas.texture_creator();
    let mut texture = texture_creator
        .create_texture_streaming(PixelFormatEnum::BGR24, initial_width, initial_height)
        .context("création texture SDL2")?;

    let mut event_pump = sdl_context.event_pump()
        .map_err(|e| anyhow::anyhow!("SDL2 event pump: {}", e))?;

    let scaling = ScalingMode::from_str(&config.scaling);
    let mut win_w = initial_width;
    let mut win_h = initial_height;
    let mut render_rect = RenderRect::compute(initial_width, initial_height, win_w, win_h, scaling);

    let mut metrics = RenderMetrics {
        connection_status: ConnectionStatus::Connecting,
        ..Default::default()
    };

    let mut fps_counter = 0u64;
    let mut fps_timer   = Instant::now();
    let mut last_frame_time = Instant::now();

    info!(width = initial_width, height = initial_height, "SDL2 boucle démarrée");

    'main: loop {
        // ── Événements SDL2 ──────────────────────────────────────────────
        for event in event_pump.poll_iter() {
            match event {
                Event::Quit { .. } => break 'main,

                Event::KeyDown { keycode: Some(Keycode::F), keymod, .. }
                    if keymod.contains(sdl2::keyboard::Mod::LCTRLMOD)
                    && keymod.contains(sdl2::keyboard::Mod::LALTMOD) =>
                {
                    // Ctrl+Alt+F → libère la capture / quitte le mode plein écran
                    info!("combo d'évasion Ctrl+Alt+F");
                    break 'main;
                }

                Event::Window { win_event: sdl2::event::WindowEvent::Resized(w, h), .. } => {
                    win_w = w as u32;
                    win_h = h as u32;
                    render_rect = RenderRect::compute(initial_width, initial_height, win_w, win_h, scaling);
                    debug!(win_w, win_h, "fenêtre redimensionnée");
                }

                Event::KeyDown { keycode: Some(kc), scancode: Some(sc), keymod, repeat, .. } => {
                    let ev = InputEvent::KeyDown {
                        keycode:  kc as u32,
                        scancode: sc as u32,
                        shift:    keymod.contains(sdl2::keyboard::Mod::LSHIFTMOD)
                                  || keymod.contains(sdl2::keyboard::Mod::RSHIFTMOD),
                        ctrl:     keymod.contains(sdl2::keyboard::Mod::LCTRLMOD)
                                  || keymod.contains(sdl2::keyboard::Mod::RCTRLMOD),
                        alt:      keymod.contains(sdl2::keyboard::Mod::LALTMOD)
                                  || keymod.contains(sdl2::keyboard::Mod::RALTMOD),
                        meta:     keymod.contains(sdl2::keyboard::Mod::LGUIMOD)
                                  || keymod.contains(sdl2::keyboard::Mod::RGUIMOD),
                        repeat,
                    };
                    let _ = input_tx.try_send(ev);
                }

                Event::KeyUp { keycode: Some(kc), scancode: Some(sc), .. } => {
                    let ev = InputEvent::KeyUp { keycode: kc as u32, scancode: sc as u32 };
                    let _ = input_tx.try_send(ev);
                }

                Event::MouseMotion { x, y, .. } => {
                    if let Some((nx, ny)) = render_rect.window_to_normalized(x, y) {
                        let ev = InputEvent::MouseMove { x: nx, y: ny, monitor: 0 };
                        let _ = input_tx.try_send(ev);
                    }
                }

                Event::MouseButtonDown { mouse_btn, x, y, .. } => {
                    if let Some((nx, ny)) = render_rect.window_to_normalized(x, y) {
                        let ev = InputEvent::MouseDown {
                            button: mouse_btn as u32,
                            x: nx, y: ny,
                        };
                        let _ = input_tx.try_send(ev);
                    }
                }

                Event::MouseButtonUp { mouse_btn, x, y, .. } => {
                    if let Some((nx, ny)) = render_rect.window_to_normalized(x, y) {
                        let ev = InputEvent::MouseUp {
                            button: mouse_btn as u32,
                            x: nx, y: ny,
                        };
                        let _ = input_tx.try_send(ev);
                    }
                }

                Event::MouseWheel { x, y, .. } => {
                    let ev = InputEvent::MouseScroll { dx: x as f32, dy: y as f32 };
                    let _ = input_tx.try_send(ev);
                }

                _ => {}
            }
        }

        // ── Rendu de la dernière frame disponible ─────────────────────────
        // Drain toutes les frames disponibles, ne garde que la plus récente
        let mut latest_frame: Option<DecodedFrame> = None;
        loop {
            match frame_rx.try_recv() {
                Ok(f) => { latest_frame = Some(f); }
                Err(mpsc::TryRecvError::Empty)        => break,
                Err(mpsc::TryRecvError::Disconnected) => break 'main,
            }
        }

        if let Some(frame) = latest_frame {
            // Mise à jour de la texture avec les pixels BGRA
            let w = frame.width;
            let h = frame.height;

            // Recréer la texture si la résolution a changé
            if w != initial_width || h != initial_height {
                // TODO : recréer texture avec nouvelles dims
                debug!(w, h, "changement de résolution");
            }

            texture.with_lock(None, |buf, _pitch| {
                // Copie BGRA → BGR (SDL2 BGR24)
                // TODO Phase 2.1 : optimiser avec SIMD
                let src = &frame.data;
                let mut dst_idx = 0;
                let mut src_idx = 0;
                while src_idx + 3 < src.len() && dst_idx + 2 < buf.len() {
                    buf[dst_idx]     = src[src_idx];     // B
                    buf[dst_idx + 1] = src[src_idx + 1]; // G
                    buf[dst_idx + 2] = src[src_idx + 2]; // R
                    dst_idx += 3;
                    src_idx += 4; // skip A
                }
            }).map_err(|e| anyhow::anyhow!("texture lock: {}", e))?;

            // Effacement + rendu
            canvas.set_draw_color(sdl2::pixels::Color::BLACK);
            canvas.clear();

            let dst = sdl2::rect::Rect::new(
                render_rect.x,
                render_rect.y,
                render_rect.w,
                render_rect.h,
            );
            canvas.copy(&texture, None, Some(dst))
                .map_err(|e| anyhow::anyhow!("canvas copy: {}", e))?;

            canvas.present();

            // Métriques FPS
            fps_counter += 1;
            metrics.frames_rendered += 1;
            metrics.decode_latency_us = frame.decode_duration_us;

            if fps_timer.elapsed().as_secs_f32() >= 1.0 {
                metrics.fps = fps_counter as f32 / fps_timer.elapsed().as_secs_f32();
                fps_counter = 0;
                fps_timer   = Instant::now();
                metrics.connection_status = ConnectionStatus::Connected;
                let _ = metrics_tx.send(metrics.clone());
                debug!(fps = metrics.fps, "FPS actuel");
            }
        } else {
            // Pas de nouvelle frame → petite pause pour éviter busy-loop
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    info!("SDL2 boucle terminée");
    Ok(())
}

/// Implémentation stub (sans SDL2)
#[cfg(any(not(feature = "sdl2-renderer"), feature = "stub"))]
fn run_sdl2_stub(
    _config: DisplayConfig,
    _initial_width: u32,
    _initial_height: u32,
    frame_rx: mpsc::Receiver<DecodedFrame>,
    _input_tx: tokio::sync::mpsc::Sender<InputEvent>,
    metrics_tx: tokio::sync::watch::Sender<RenderMetrics>,
) -> Result<()> {
    info!("renderer stub démarré (SDL2 non disponible)");
    let mut count = 0u64;

    loop {
        match frame_rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(frame) => {
                count += 1;
                if count % 60 == 0 {
                    info!(seq = frame.seq, frames = count, "renderer stub: frame reçue");
                    let _ = metrics_tx.send(RenderMetrics {
                        fps: 30.0,
                        frames_rendered: count,
                        connection_status: ConnectionStatus::Connected,
                        ..Default::default()
                    });
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout)       => continue,
            Err(mpsc::RecvTimeoutError::Disconnected)  => break,
        }
    }

    info!("renderer stub terminé, {} frames affichées", count);
    Ok(())
}
