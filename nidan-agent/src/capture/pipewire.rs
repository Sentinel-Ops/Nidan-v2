//! Capture d'écran Wayland via le portail XDG ScreenCast + PipeWire.
//!
//! Sous Wayland, `XGetImage` ne peut pas capturer le bureau (isolation du
//! compositeur). Le mécanisme officiel et sécurisé est le portail
//! `org.freedesktop.portal.ScreenCast` : l'utilisateur autorise le partage via
//! une fenêtre du compositeur, le portail retourne un flux PipeWire, et on lit
//! les buffers vidéo depuis ce flux.
//!
//! Déroulé :
//!   1. ashpd : créer une session ScreenCast, sélectionner les sources,
//!      démarrer → obtient un node PipeWire + un file descriptor.
//!   2. pipewire : se connecter au flux, négocier le format (BGRA/RGBA),
//!      recevoir les buffers, les convertir en `RawFrame`.
//!
//! La négociation du portail demande une autorisation utilisateur (popup du
//! compositeur). Un jeton de restauration (`restore_token`) permet de
//! ré-autoriser silencieusement les sessions suivantes (selon le compositeur).

#![cfg(feature = "pipewire-capture")]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::os::fd::OwnedFd;

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use super::{Capturer, CapturerCapabilities, PixelFormat, RawFrame};

/// Résultat de la négociation du portail : node PipeWire + fd du flux.
pub struct PortalStream {
    pub node_id: u32,
    pub fd: OwnedFd,
    pub width: u32,
    pub height: u32,
}

/// Capturer Wayland basé sur le portail ScreenCast + PipeWire.
pub struct PipeWireCapturer {
    caps: CapturerCapabilities,
    node_id: u32,
    fd: std::sync::Mutex<Option<OwnedFd>>,
    seq: AtomicU64,
}

impl PipeWireCapturer {
    /// Négocie l'accès via le portail (peut afficher une autorisation) puis
    /// prépare le capturer. `restore_token` permet une ré-autorisation
    /// silencieuse si le compositeur le supporte.
    pub fn new(restore_token: Option<String>) -> Result<Self> {
        // La négociation ashpd est asynchrone : on l'exécute sur un petit
        // runtime dédié, car create_capturer est synchrone.
        let stream = negotiate_portal(restore_token)
            .context("négociation du portail ScreenCast")?;

        let caps = CapturerCapabilities {
            width: stream.width,
            height: stream.height,
            supports_xshm: false,
            supports_xdamage: false,
            pixel_format: PixelFormat::Bgra8888,
        };

        info!(node = stream.node_id, width = stream.width, height = stream.height,
              "capture Wayland (PipeWire) initialisée");

        Ok(Self {
            caps,
            node_id: stream.node_id,
            fd: std::sync::Mutex::new(Some(stream.fd)),
            seq: AtomicU64::new(0),
        })
    }
}

impl Capturer for PipeWireCapturer {
    fn capabilities(&self) -> &CapturerCapabilities {
        &self.caps
    }

    fn start(
        self: Arc<Self>,
        tx: mpsc::Sender<RawFrame>,
        fps_limit: u32,
        shutdown: CancellationToken,
    ) -> tokio::task::JoinHandle<Result<()>> {
        let node_id = self.node_id;
        let fd = self.fd.lock().unwrap().take();
        let width = self.caps.width;
        let height = self.caps.height;
        let _seq = Arc::clone(&self);

        // La boucle PipeWire est synchrone (mainloop C) : on l'exécute sur un
        // thread bloquant dédié, et on relaie les frames via le channel tokio.
        tokio::task::spawn_blocking(move || {
            let fd = fd.context("fd PipeWire déjà consommé")?;
            run_pipewire_loop(node_id, fd, width, height, fps_limit, tx, shutdown)
        })
    }
}

/// Négocie une session ScreenCast via ashpd (portail XDG).
///
/// Étape 6e (prod) : le token de restauration est chargé depuis un fichier
/// local et sauvegardé après chaque négociation réussie. Après la toute
/// première autorisation manuelle (faite une fois lors de la préparation
/// du template VM), les démarrages suivants ne montrent plus de popup tant
/// que le token reste valide (PersistMode::ExplicitlyRevoked : persiste
/// jusqu'à révocation explicite par l'utilisateur via les paramètres GNOME).
#[cfg(feature = "pipewire-capture")]
fn negotiate_portal(restore_token: Option<String>) -> Result<PortalStream> {
    use ashpd::desktop::screencast::{CursorMode, Screencast, SourceType};
    use ashpd::desktop::PersistMode;

    let token_path = screencast_token_path();
    let saved_token = restore_token.or_else(|| read_token(&token_path));

    // ashpd est async ; on bloque le temps de la négociation.
    pollster::block_on(async move {
        let proxy = Screencast::new().await.context("proxy ScreenCast")?;
        let session = proxy.create_session().await.context("création session")?;

        proxy
            .select_sources(
                &session,
                CursorMode::Embedded,
                SourceType::Monitor | SourceType::Window,
                false, // multiple
                saved_token.as_deref(),
                PersistMode::ExplicitlyRevoked,
            )
            .await
            .context("sélection des sources")?;

        // Démarre : ouvre l'autorisation utilisateur (sauf si un token valide
        // a été fourni ci-dessus, auquel cas le portail répond directement
        // sans afficher de popup), retourne les flux.
        let response = proxy
            .start(&session, &ashpd::WindowIdentifier::default())
            .await
            .context("démarrage ScreenCast")?
            .response()
            .context("réponse ScreenCast")?;

        // Sauvegarde le token retourné (nouveau ou reconduit) pour que le
        // prochain démarrage de l'agent n'affiche plus de popup.
        if let Some(new_token) = response.restore_token() {
            write_token(&token_path, new_token);
        }

        let stream = response
            .streams()
            .first()
            .cloned()
            .context("aucun flux retourné par le portail")?;

        let node_id = stream.pipe_wire_node_id();
        let (w, h) = stream.size().unwrap_or((1920, 1080));

        // Ouvre le file descriptor PipeWire pour ce remote (OwnedFd).
        let fd = proxy
            .open_pipe_wire_remote(&session)
            .await
            .context("ouverture du remote PipeWire")?;

        Ok(PortalStream {
            node_id,
            fd,
            width: w as u32,
            height: h as u32,
        })
    })
}

/// Chemin du fichier de token de restauration ScreenCast.
/// `~/.local/state/nidan-agent/screencast.token`
#[cfg(feature = "pipewire-capture")]
fn screencast_token_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    std::path::PathBuf::from(home).join(".local/state/nidan-agent/screencast.token")
}

/// Lit un token de restauration depuis un fichier, s'il existe et est non vide.
#[cfg(feature = "pipewire-capture")]
fn read_token(path: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Écrit le token de restauration sur disque (crée le dossier si besoin).
#[cfg(feature = "pipewire-capture")]
fn write_token(path: &std::path::Path, token: &str) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(path, token) {
        warn!(error = %e, "impossible d'écrire le token de restauration ScreenCast");
    } else {
        info!("token de restauration ScreenCast sauvegardé (démarrages futurs sans popup)");
    }
}

/// Boucle PipeWire : reçoit les buffers vidéo et les pousse en `RawFrame`.
/// Exécutée sur un thread bloquant dédié (mainloop PipeWire synchrone).
#[cfg(feature = "pipewire-capture")]
fn run_pipewire_loop(
    node_id: u32,
    fd: OwnedFd,
    width: u32,
    height: u32,
    fps_limit: u32,
    tx: mpsc::Sender<RawFrame>,
    shutdown: CancellationToken,
) -> Result<()> {
    use pipewire as pw;
    use pw::{properties::properties, spa};

    pw::init();
    let mainloop = pw::main_loop::MainLoop::new(None).context("MainLoop PipeWire")?;
    let context = pw::context::Context::new(&mainloop).context("Context PipeWire")?;
    // Connexion au remote via le fd fourni par le portail (OwnedFd).
    let core = context
        .connect_fd(fd, None)
        .context("connexion au remote PipeWire (fd portail)")?;

    let stream = pw::stream::Stream::new(
        &core,
        "nidan-capture",
        properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .context("création du Stream PipeWire")?;

    let tx_frames = tx.clone();
    let frame_seq = AtomicU64::new(0);
    let min_interval_us: u64 = if fps_limit > 0 { 1_000_000 / fps_limit as u64 } else { 0 };
    let last_emit = std::sync::Mutex::new(0u64);

    // Callback à chaque buffer : convertit en RawFrame.
    let _listener = stream
        .add_local_listener_with_user_data(())
        .process(move |stream, _| {
            if let Some(mut buffer) = stream.dequeue_buffer() {
                let datas = buffer.datas_mut();
                if datas.is_empty() { return; }
                let data = &mut datas[0];
                let chunk_size = data.chunk().size() as usize;
                if let Some(slice) = data.data() {
                    let now_us = now_micros();
                    // Limitation de fréquence
                    {
                        let mut le = last_emit.lock().unwrap();
                        if min_interval_us > 0 && now_us.saturating_sub(*le) < min_interval_us {
                            return;
                        }
                        *le = now_us;
                    }
                    let n = chunk_size.min(slice.len());
                    let stride = if height > 0 { (n as u32) / height } else { width * 4 };
                    let frame = RawFrame {
                        data: slice[..n].to_vec(),
                        width,
                        height,
                        stride: if stride > 0 { stride } else { width * 4 },
                        timestamp_us: now_us,
                        seq: frame_seq.fetch_add(1, Ordering::Relaxed),
                        is_keyframe: true,
                        damage_rects: Vec::new(),
                    };
                    // Envoi non bloquant vers le pipeline d'encodage.
                    if tx_frames.try_send(frame).is_err() {
                        // file pleine : on laisse tomber cette frame (live)
                    }
                }
            }
        })
        .register()
        .context("enregistrement du listener PipeWire")?;

    // Format vidéo demandé : BGRA, taille négociée.
    let obj = pw::spa::pod::object!(
        pw::spa::utils::SpaTypes::ObjectParamFormat,
        pw::spa::param::ParamType::EnumFormat,
        pw::spa::pod::property!(pw::spa::param::format::FormatProperties::MediaType,
            Id, pw::spa::param::format::MediaType::Video),
        pw::spa::pod::property!(pw::spa::param::format::FormatProperties::MediaSubtype,
            Id, pw::spa::param::format::MediaSubtype::Raw),
        pw::spa::pod::property!(pw::spa::param::format::FormatProperties::VideoFormat,
            Id, pw::spa::param::video::VideoFormat::BGRA),
    );
    let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    ).context("sérialisation format SPA")?.0.into_inner();
    let mut params = [pw::spa::pod::Pod::from_bytes(&values).context("Pod format")?];

    stream
        .connect(
            spa::utils::Direction::Input,
            Some(node_id),
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .context("connexion du Stream au node")?;

    info!(node = node_id, "boucle de capture PipeWire démarrée");

    // Boucle : on tourne tant que shutdown n'est pas déclenché.
    // On vérifie périodiquement le token via un timer.
    let mainloop_ref = mainloop.clone();
    let timer = {
        let sd = shutdown.clone();
        mainloop.loop_().add_timer(move |_| {
            if sd.is_cancelled() {
                mainloop_ref.quit();
            }
        })
    };
    // Arme le timer : premier tir dans 200ms, puis toutes les 200ms.
    // Sans cet appel, le callback ci-dessus n'est JAMAIS invoqué et
    // mainloop.run() ne rend jamais la main (Ctrl+C reste sans effet).
    timer
        .update_timer(
            Some(std::time::Duration::from_millis(200)),
            Some(std::time::Duration::from_millis(200)),
        )
        .into_result()
        .context("armement du timer de shutdown PipeWire")?;
    mainloop.run();

    info!("boucle de capture PipeWire arrêtée");
    Ok(())
}

fn now_micros() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_micros() as u64).unwrap_or(0)
}
