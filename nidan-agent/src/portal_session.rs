//! Session portail XDG unique, partagée entre la capture ScreenCast et
//! l'injection RemoteDesktop.
//!
//! # Pourquoi ce module existe
//!
//! Avant l'étape 6j, la capture (`capture/pipewire.rs`) et l'injecteur
//! (`remote_desktop.rs`) créaient deux sessions D-Bus indépendantes.
//! Le compositeur GNOME lie chaque appel `notify_pointer_motion_absolute
//! (stream)` à un stream de la session qui injecte ; le stream de la
//! session "injection" étant déclaré via `select_sources` mais jamais
//! ouvert via `open_pipe_wire_remote`, GNOME le considère inactif et
//! rejette silencieusement toutes les injections absolues (log :
//! `error=notify_pointer_motion_absolute`). Effet visible côté client :
//! curseur invité figé, clic droit et clavier fonctionnels (position-
//! indépendants), clic gauche apparemment inactif (en réalité injecté
//! à la position figée du curseur invité).
//!
//! # Ce que fait ce module
//!
//! Négocie UNE seule session `RemoteDesktop` qui référence aussi une
//! source `ScreenCast` (couplage explicite sur la même session D-Bus),
//! ouvre le fd PipeWire (activation effective du stream côté GNOME),
//! puis :
//! - envoie le `PortalStream` (node_id + fd + dimensions) au capturer
//!   via un oneshot ;
//! - garde la session et le proxy dans le thread portail pour appliquer
//!   les `InputBatch` reçus via un `std::sync::mpsc`, en utilisant le
//!   MÊME `stream_node` que le capturer — c'est ce qui rend
//!   `notify_pointer_motion_absolute` fonctionnel pixel-parfait.
//!
//! # Bénéfices annexes
//!
//! - Une seule popup GNOME d'autorisation au premier démarrage (au lieu
//!   de deux).
//! - Un seul token de restauration à gérer.
//! - Surface D-Bus réduite : un seul canal à auditer pour la CSPN.
//! - Lifecycle cohérent : si l'utilisateur révoque l'autorisation, tout
//!   s'arrête proprement en même temps.

#![cfg(all(feature = "pipewire-capture", feature = "remotedesktop-input"))]

use std::sync::mpsc as std_mpsc;
use std::thread;

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

use nidan_proto::{InputBatch, InputEventPayload};

use crate::capture::pipewire::PortalStream;

/// Handles exposés à l'application après la négociation portail.
///
/// - `stream` : à passer au capturer via `PipeWireCapturer::from_shared_stream`.
/// - `inputs_tx` : à passer à l'injecteur via
///   `RemoteDesktopInjector::from_shared_channel`.
/// - `restore_token` : à persister sur disque pour les démarrages suivants.
pub struct SharedPortalHandles {
    pub stream: PortalStream,
    pub inputs_tx: std_mpsc::Sender<InputBatch>,
    pub restore_token: Option<String>,
    /// Handle du thread portail. Gardé vivant tant que l'agent tourne :
    /// sa mort implique la perte de la session (et l'arrêt de l'injection).
    pub _thread: thread::JoinHandle<()>,
}

/// Négocie la session portail unique et lance le thread d'injection.
///
/// `saved_token` : token RemoteDesktop chargé depuis
/// `~/.local/state/nidan-agent/remotedesktop.token` (ou None au premier
/// démarrage, ce qui déclenche la popup d'autorisation GNOME).
///
/// Bloquant : attend que la négociation portail soit terminée (popup
/// affichée et validée par l'utilisateur au premier démarrage) avant de
/// rendre la main.
pub fn spawn_shared_portal(saved_token: Option<String>) -> Result<SharedPortalHandles> {
    // Canal pour recevoir le résultat de la négociation initiale.
    let (ready_tx, ready_rx) = std_mpsc::channel::<Result<InitOutcome, String>>();
    // Canal pour envoyer les InputBatch au thread portail (jamais consommé
    // avant que la négociation soit OK).
    let (inputs_tx, inputs_rx) = std_mpsc::channel::<InputBatch>();

    let handle = thread::Builder::new()
        .name("nidan-portal".into())
        .spawn(move || {
            portal_thread(ready_tx, inputs_rx, saved_token);
        })
        .context("démarrage du thread portail")?;

    // Attente bloquante du résultat de la négociation (peut prendre du temps
    // au premier démarrage — l'utilisateur doit valider la popup GNOME).
    let outcome = match ready_rx.recv() {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => anyhow::bail!("négociation portail échouée : {e}"),
        Err(_) => anyhow::bail!("thread portail terminé avant initialisation"),
    };

    info!(
        stream_node = outcome.stream.node_id,
        width = outcome.stream.width,
        height = outcome.stream.height,
        "session portail unifiée prête (capture + injection partagent le même stream)"
    );

    Ok(SharedPortalHandles {
        stream: outcome.stream,
        inputs_tx,
        restore_token: outcome.restore_token,
        _thread: handle,
    })
}

/// Résultat de la négociation initiale du portail, remonté à l'appelant
/// via le canal `ready_tx`.
struct InitOutcome {
    stream: PortalStream,
    restore_token: Option<String>,
}

/// Corps du thread portail : négocie la session UNE fois, puis boucle
/// d'injection jusqu'à la fermeture du canal `inputs_rx`.
fn portal_thread(
    ready_tx: std_mpsc::Sender<Result<InitOutcome, String>>,
    inputs_rx: std_mpsc::Receiver<InputBatch>,
    saved_token: Option<String>,
) {
    use ashpd::desktop::remote_desktop::{DeviceType, RemoteDesktop};
    use ashpd::desktop::screencast::{CursorMode, Screencast, SourceType};
    use ashpd::desktop::PersistMode;

    let ready_tx_outer = ready_tx.clone();

    let result: Result<()> = pollster::block_on(async move {
        // 1. Créer les deux proxies (mais UNE seule session).
        let remote = RemoteDesktop::new().await.context("proxy RemoteDesktop")?;
        let screencast = Screencast::new().await.context("proxy ScreenCast")?;

        // 2. Session unique, créée via RemoteDesktop (qui devient "propriétaire").
        let session = remote
            .create_session()
            .await
            .context("création de la session portail unique")?;
        debug!("session portail créée");

        // 3. Devices RemoteDesktop (clavier + pointeur) — PersistMode
        //    s'applique à la session complète via cet appel.
        remote
            .select_devices(
                &session,
                DeviceType::Keyboard | DeviceType::Pointer,
                saved_token.as_deref(),
                PersistMode::ExplicitlyRevoked,
            )
            .await
            .context("select_devices RemoteDesktop (kbd + pointer)")?;

        // 4. Sources ScreenCast SUR LA MÊME SESSION.
        //    NOTE : le portail refuse PersistMode != DoNot ici quand la
        //    session est une RemoteDesktop combinée (erreur "Remote
        //    desktop sessions cannot persist"). La persistance vit sur
        //    select_devices ci-dessus.
        screencast
            .select_sources(
                &session,
                CursorMode::Embedded,
                SourceType::Monitor | SourceType::Window,
                false, // multiple = false
                None,  // pas de restore_token ScreenCast séparé
                PersistMode::DoNot,
            )
            .await
            .context("select_sources ScreenCast sur la session partagée")?;

        // 5. Start UNIQUE (via RemoteDesktop). Popup si saved_token absent.
        let response = remote
            .start(&session, &ashpd::WindowIdentifier::default())
            .await
            .context("démarrage de la session portail")?
            .response()
            .context("réponse Start portail")?;

        // 6. Récupérer le stream depuis la réponse (Option en RemoteDesktop).
        let stream = response
            .streams()
            .and_then(|s| s.first().cloned())
            .context(
                "aucun stream retourné par la session portail — \
                 select_sources a-t-il été appelé avant Start ?",
            )?;

        let stream_node = stream.pipe_wire_node_id();
        let (w, h) = stream.size().unwrap_or((1920, 1080));
        let width = w as u32;
        let height = h as u32;

        // 7. Ouvrir le fd PipeWire (indispensable : sans ça, GNOME considère
        //    le stream inactif et rejette notify_pointer_motion_absolute).
        let fd = screencast
            .open_pipe_wire_remote(&session)
            .await
            .context("open_pipe_wire_remote sur la session partagée")?;

        // 8. Publier le résultat vers l'appelant.
        let restore_token = response.restore_token().map(String::from);
        let stream = PortalStream {
            node_id: stream_node,
            fd,
            width,
            height,
        };
        ready_tx
            .send(Ok(InitOutcome {
                stream,
                restore_token,
            }))
            .map_err(|_| anyhow::anyhow!("appelant disparu avant réception du stream"))?;

        // 9. Boucle d'injection : consomme les InputBatch reçus via mpsc.
        //    On garde `remote` + `session` VIVANTS ici — c'est LE point
        //    critique du fix, car les notify_* nécessitent la même session
        //    que celle qui a créé le stream_node.
        info!(
            stream_node,
            width, height, "thread portail : entrée dans la boucle d'injection"
        );
        loop {
            let batch = match inputs_rx.recv() {
                Ok(b) => b,
                Err(_) => break, // sender droppé → fin propre
            };
            for event in &batch.events {
                if let Err(e) = apply_event(
                    &remote,
                    &session,
                    stream_node,
                    width,
                    height,
                    event,
                )
                .await
                {
                    warn!(error = %e, seq = event.seq, "injection RemoteDesktop échouée");
                }
            }
        }
        debug!("thread portail : boucle d'injection terminée");
        Ok(())
    });

    if let Err(e) = result {
        let _ = ready_tx_outer.send(Err(format!("{e:#}")));
        warn!(error = format!("{e:#}"), "thread portail arrêté sur erreur");
    }
}

/// Applique un événement NIDAN via le portail (identique au corps de
/// `remote_desktop::apply_event`, ici pour ne pas polluer ce module).
///
/// Le mapping scancode SDL→evdev est délégué à
/// `crate::remote_desktop::sdl_scancode_to_evdev` (rendu `pub(crate)`).
async fn apply_event(
    remote: &ashpd::desktop::remote_desktop::RemoteDesktop<'_>,
    session: &ashpd::desktop::Session<'_, ashpd::desktop::remote_desktop::RemoteDesktop<'_>>,
    stream_node: u32,
    width: u32,
    height: u32,
    event: &nidan_proto::InputEvent,
) -> Result<()> {
    use ashpd::desktop::remote_desktop::{Axis, KeyState};

    match &event.event {
        Some(InputEventPayload::Key(k)) => {
            let state = if event.event_type == 1 {
                KeyState::Pressed
            } else {
                KeyState::Released
            };
            match crate::remote_desktop::sdl_scancode_to_evdev(k.scancode) {
                Some(evdev_code) => {
                    remote
                        .notify_keyboard_keycode(session, evdev_code, state)
                        .await
                        .context("notify_keyboard_keycode")?;
                }
                None => {
                    debug!(scancode = k.scancode, "scancode non mappé — ignoré (sécurité)");
                }
            }
        }
        Some(InputEventPayload::Mouse(m)) => {
            match event.event_type {
                3 => {
                    let x = (m.x * width as f32) as f64;
                    let y = (m.y * height as f32) as f64;
                    remote
                        .notify_pointer_motion_absolute(session, stream_node, x, y)
                        .await
                        .context("notify_pointer_motion_absolute")?;
                }
                4 | 5 => {
                    let state = if event.event_type == 4 {
                        KeyState::Pressed
                    } else {
                        KeyState::Released
                    };
                    let btn = match m.button {
                        1 => 0x110,
                        2 => 0x112,
                        3 => 0x111,
                        _ => 0x110,
                    };
                    remote
                        .notify_pointer_button(session, btn, state)
                        .await
                        .context("notify_pointer_button")?;
                }
                6 => {
                    let steps = if m.scroll_dy > 0.0 { -1 } else { 1 };
                    remote
                        .notify_pointer_axis_discrete(session, Axis::Vertical, steps)
                        .await
                        .context("notify_pointer_axis_discrete")?;
                }
                _ => {}
            }
        }
        None => {}
    }
    Ok(())
}

/// Chemin du fichier de token de restauration RemoteDesktop.
pub fn token_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    std::path::PathBuf::from(home).join(".local/state/nidan-agent/remotedesktop.token")
}

pub fn load_token() -> Option<String> {
    std::fs::read_to_string(token_path())
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn save_token(token: &str) {
    let path = token_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::write(&path, token) {
        Ok(_) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(
                    &path,
                    std::fs::Permissions::from_mode(0o600),
                );
            }
            info!(path = ?path, "token RemoteDesktop persisté");
        }
        Err(e) => warn!(error = %e, "échec écriture token RemoteDesktop"),
    }
}
