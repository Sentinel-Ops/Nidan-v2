//! Injection des événements d'entrée (clavier/souris) via le portail
//! XDG `org.freedesktop.portal.RemoteDesktop` — compatible Wayland natif.
//!
//! XTEST (voir `input.rs`) ne fonctionne pas sous Wayland : le compositeur
//! isole les entrées. Le mécanisme officiel est le portail RemoteDesktop,
//! couplé à ScreenCast sur la MÊME session pour obtenir le `stream` de
//! référence (nécessaire au positionnement absolu du pointeur).
//!
//! Derrière la feature `remotedesktop-input` (dépend d'ashpd + pollster).
//!
//! Architecture : la négociation et les notifications portail sont async
//! (D-Bus). On les exécute sur un thread dédié possédant son propre exécuteur
//! `pollster`, et on reçoit les `InputBatch` via un canal mpsc. Cela évite de
//! bloquer un thread du runtime tokio et garde l'API d'injection synchrone,
//! homogène avec `InputInjector`.

#![cfg(feature = "remotedesktop-input")]

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

use nidan_proto::{InputBatch, InputEventPayload};

/// Injecteur d'entrées via le portail RemoteDesktop (Wayland).
///
/// L'objet public est un simple expéditeur vers le thread portail : il reçoit
/// des `InputBatch` et les relaie. Toute la logique async vit dans le thread.
pub struct RemoteDesktopInjector {
    tx: std::sync::mpsc::Sender<InputBatch>,
    width: u32,
    height: u32,
    injected: u64,
    _thread: std::thread::JoinHandle<()>,
}

impl RemoteDesktopInjector {
    /// Crée l'injecteur : ouvre la session portail (autorisation utilisateur),
    /// puis démarre le thread qui applique les events reçus.
    ///
    /// `restore_token` permet une ré-autorisation silencieuse si supportée.
    pub fn new(width: u32, height: u32, restore_token: Option<String>) -> Result<Self> {
        let (tx, rx) = std::sync::mpsc::channel::<InputBatch>();
        // Canal de retour pour signaler le succès/échec de la négociation initiale.
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();

        let thread = std::thread::Builder::new()
            .name("nidan-remotedesktop".into())
            .spawn(move || {
                portal_thread(rx, ready_tx, restore_token, width, height);
            })
            .context("démarrage du thread RemoteDesktop")?;

        // Attendre le résultat de la négociation (popup d'autorisation incluse).
        match ready_rx.recv() {
            Ok(Ok(())) => {
                info!("injecteur RemoteDesktop (portail Wayland) initialisé");
            }
            Ok(Err(e)) => {
                anyhow::bail!("négociation RemoteDesktop échouée : {e}");
            }
            Err(_) => {
                anyhow::bail!("thread RemoteDesktop terminé avant initialisation");
            }
        }

        Ok(Self { tx, width, height, injected: 0, _thread: thread })
    }

    /// Injecte tous les événements d'un batch (envoi au thread portail).
    pub fn inject_batch(&mut self, batch: &InputBatch) -> Result<()> {
        self.injected += batch.events.len() as u64;
        self.tx
            .send(batch.clone())
            .context("envoi du batch au thread RemoteDesktop")?;
        Ok(())
    }

    pub fn injected_count(&self) -> u64 {
        self.injected
    }
}

/// Boucle du thread portail : négocie la session puis applique les batches.
#[cfg(feature = "remotedesktop-input")]
fn portal_thread(
    rx: std::sync::mpsc::Receiver<InputBatch>,
    ready_tx: std::sync::mpsc::Sender<Result<(), String>>,
    restore_token: Option<String>,
    width: u32,
    height: u32,
) {
    use ashpd::desktop::remote_desktop::{DeviceType, KeyState, RemoteDesktop};
    use ashpd::desktop::screencast::{CursorMode, Screencast, SourceType};
    use ashpd::desktop::PersistMode;

    // Clone pour signaler un échec survenant AVANT le ready_tx.send interne
    // (le bloc async consomme l'original).
    let ready_tx_outer = ready_tx.clone();

    // Étape 6e (prod) : token de restauration chargé depuis un fichier local,
    // sauvegardé après négociation réussie. PersistMode::ExplicitlyRevoked
    // (au lieu de DoNot) permet au portail de se souvenir de l'autorisation
    // entre les démarrages de l'agent.
    let token_path = remotedesktop_token_path();
    let saved_token = restore_token.or_else(|| read_token(&token_path));

    // Exécuteur async local au thread (ashpd/zbus sont async).
    let result: Result<()> = pollster::block_on(async move {
        let remote = RemoteDesktop::new().await.context("proxy RemoteDesktop")?;
        let screencast = Screencast::new().await.context("proxy ScreenCast")?;

        // Une seule session partagée RemoteDesktop + ScreenCast.
        let session = remote.create_session().await.context("création session")?;

        // Sélection des périphériques : clavier + pointeur.
        remote
            .select_devices(
                &session,
                DeviceType::Keyboard | DeviceType::Pointer,
                saved_token.as_deref(),
                PersistMode::ExplicitlyRevoked,
            )
            .await
            .context("sélection des périphériques")?;

        // Sélection d'une source écran sur la MÊME session : indispensable pour
        // le positionnement absolu du pointeur (notify_pointer_motion_absolute
        // référence un `stream`).
        // NOTE (vérifié en conditions réelles) : le portail refuse la
        // persistance à ce niveau précis quand select_sources est appelé à
        // l'intérieur d'une session RemoteDesktop combinée — erreur
        // "Remote desktop sessions cannot persist". La persistance de la
        // session complète est gouvernée par select_devices() ci-dessus ;
        // ce sous-appel doit rester DoNot.
        screencast
            .select_sources(
                &session,
                CursorMode::Embedded,
                SourceType::Monitor | SourceType::Window,
                false,
                None,
                PersistMode::DoNot,
            )
            .await
            .context("sélection des sources écran")?;

        // Démarrage : affiche la fenêtre d'autorisation du compositeur, sauf
        // si un token valide a été fourni ci-dessus (auquel cas c'est silencieux).
        let response = remote
            .start(&session, &ashpd::WindowIdentifier::default())
            .await
            .context("démarrage RemoteDesktop")?
            .response()
            .context("réponse RemoteDesktop")?;

        // Sauvegarde le token retourné pour que le prochain démarrage de
        // l'agent n'affiche plus de popup.
        if let Some(new_token) = response.restore_token() {
            write_token(&token_path, new_token);
        }

        // Node du flux écran (pour le motion absolu). 0 par défaut si absent.
        let stream_node: u32 = response
            .streams()
            .and_then(|s| s.first())
            .map(|s| s.pipe_wire_node_id())
            .unwrap_or(0);

        debug!(stream = stream_node, "session RemoteDesktop démarrée");

        // Négociation réussie : on débloque l'appelant.
        let _ = ready_tx.send(Ok(()));

        // Boucle d'application des batches reçus du serveur.
        // `recv()` est bloquant ; on reste dans le contexte async pour les
        // notify_* (qui sont async), via une boucle simple.
        loop {
            // Réception bloquante d'un batch (hors async : OK, on est sur un
            // thread dédié). On sort si le canal est fermé (fin de session).
            let batch = match rx.recv() {
                Ok(b) => b,
                Err(_) => break, // expéditeur libéré → fin
            };
            for event in &batch.events {
                if let Err(e) =
                    apply_event(&remote, &session, stream_node, width, height, event).await
                {
                    warn!(error = %e, seq = event.seq, "injection RemoteDesktop échouée");
                }
            }
        }

        debug!("boucle RemoteDesktop terminée");
        Ok(())
    });

    if let Err(e) = result {
        // Si l'échec survient avant le ready_tx.send(Ok), informer l'appelant.
        let _ = ready_tx_outer.send(Err(format!("{e:#}")));
        warn!(error = format!("{e:#}"), "thread RemoteDesktop arrêté sur erreur");
    }
}

/// Traduit et applique un événement NIDAN via le portail.
#[cfg(feature = "remotedesktop-input")]
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
            // event_type : 1 = KeyDown, 2 = KeyUp
            let state = if event.event_type == 1 {
                KeyState::Pressed
            } else {
                KeyState::Released
            };
            // SÉCURITÉ : le portail attend un keycode evdev (Linux). Le client
            // envoie un *scancode SDL2* (basé sur USB HID). Envoyer la valeur
            // brute (ou le SDL keycode) peut tomber sur une touche système
            // dangereuse (KEY_POWER, etc.) et provoquer un arrêt machine.
            // On convertit donc explicitement, et on IGNORE tout code non mappé.
            match sdl_scancode_to_evdev(k.scancode) {
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
                // MouseMove : coordonnées normalisées [0,1] → pixels absolus,
                // référencées au flux écran.
                3 => {
                    let x = (m.x * width as f32) as f64;
                    let y = (m.y * height as f32) as f64;
                    remote
                        .notify_pointer_motion_absolute(session, stream_node, x, y)
                        .await
                        .context("notify_pointer_motion_absolute")?;
                }
                // MouseDown / MouseUp : bouton Linux (BTN_LEFT=0x110, etc.)
                4 | 5 => {
                    let state = if event.event_type == 4 {
                        KeyState::Pressed
                    } else {
                        KeyState::Released
                    };
                    // Mapping bouton X (1=gauche,2=milieu,3=droit) → evdev BTN_*
                    let btn = match m.button {
                        1 => 0x110, // BTN_LEFT
                        2 => 0x112, // BTN_MIDDLE
                        3 => 0x111, // BTN_RIGHT
                        _ => 0x110,
                    };
                    remote
                        .notify_pointer_button(session, btn, state)
                        .await
                        .context("notify_pointer_button")?;
                }
                // MouseScroll : axe vertical discret
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
/// `~/.local/state/nidan-agent/remotedesktop.token`
#[cfg(feature = "remotedesktop-input")]
fn remotedesktop_token_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    std::path::PathBuf::from(home).join(".local/state/nidan-agent/remotedesktop.token")
}

/// Lit un token de restauration depuis un fichier, s'il existe et est non vide.
#[cfg(feature = "remotedesktop-input")]
fn read_token(path: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Écrit le token de restauration sur disque (crée le dossier si besoin).
#[cfg(feature = "remotedesktop-input")]
fn write_token(path: &std::path::Path, token: &str) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(path, token) {
        warn!(error = %e, "impossible d'écrire le token de restauration RemoteDesktop");
    } else {
        info!("token de restauration RemoteDesktop sauvegardé (démarrages futurs sans popup)");
    }
}

/// Convertit un scancode SDL2 (basé sur USB HID usage IDs) en keycode evdev
/// Linux (linux/input-event-codes.h), attendu par le portail RemoteDesktop.
///
/// SÉCURITÉ : seules les touches usuelles et sûres sont mappées. Tout scancode
/// inconnu renvoie `None` et n'est PAS injecté — cela évite d'envoyer par erreur
/// un code tombant sur une touche système (alimentation, veille…) qui pourrait
/// arrêter ou redémarrer la machine.
#[cfg(feature = "remotedesktop-input")]
fn sdl_scancode_to_evdev(sdl_scancode: u32) -> Option<i32> {
    // SDL_SCANCODE_* (HID) → KEY_* (evdev)
    let code: i32 = match sdl_scancode {
        // Lettres A–Z : HID 4..29 → evdev (table explicite, AZERTY/QWERTY gérés
        // par la disposition du compositeur côté serveur)
        4  => 30,  // A KEY_A
        5  => 48,  // B
        6  => 46,  // C
        7  => 32,  // D
        8  => 18,  // E
        9  => 33,  // F
        10 => 34,  // G
        11 => 35,  // H
        12 => 23,  // I
        13 => 36,  // J
        14 => 37,  // K
        15 => 38,  // L
        16 => 50,  // M
        17 => 49,  // N
        18 => 24,  // O
        19 => 25,  // P
        20 => 16,  // Q
        21 => 19,  // R
        22 => 31,  // S
        23 => 20,  // T
        24 => 22,  // U
        25 => 47,  // V
        26 => 17,  // W
        27 => 45,  // X
        28 => 21,  // Y
        29 => 44,  // Z
        // Chiffres 1–0 : HID 30..39 → evdev 2..11
        30 => 2,   // 1
        31 => 3,   // 2
        32 => 4,   // 3
        33 => 5,   // 4
        34 => 6,   // 5
        35 => 7,   // 6
        36 => 8,   // 7
        37 => 9,   // 8
        38 => 10,  // 9
        39 => 11,  // 0
        // Touches d'édition courantes
        40 => 28,  // Return/Enter KEY_ENTER
        41 => 1,   // Escape KEY_ESC
        42 => 14,  // Backspace
        43 => 15,  // Tab
        44 => 57,  // Space
        45 => 12,  // Minus -
        46 => 13,  // Equals =
        47 => 26,  // LeftBracket [
        48 => 27,  // RightBracket ]
        49 => 43,  // Backslash \
        51 => 39,  // Semicolon ;
        52 => 40,  // Apostrophe '
        53 => 41,  // Grave `
        54 => 51,  // Comma ,
        55 => 52,  // Period .
        56 => 53,  // Slash /
        57 => 58,  // CapsLock
        // F1–F12 : HID 58..69 → evdev 59..70
        58 => 59,  // F1
        59 => 60,  // F2
        60 => 61,  // F3
        61 => 62,  // F4
        62 => 63,  // F5
        63 => 64,  // F6
        64 => 65,  // F7
        65 => 66,  // F8
        66 => 67,  // F9
        67 => 68,  // F10
        68 => 87,  // F11
        69 => 88,  // F12
        // Navigation
        73 => 110, // Insert
        74 => 102, // Home
        75 => 104, // PageUp
        76 => 111, // Delete KEY_DELETE
        77 => 107, // End
        78 => 109, // PageDown
        79 => 106, // Right KEY_RIGHT
        80 => 105, // Left
        81 => 108, // Down
        82 => 103, // Up
        // Modificateurs (HID 224..231 → evdev)
        224 => 29,  // LeftCtrl KEY_LEFTCTRL
        225 => 42,  // LeftShift
        226 => 56,  // LeftAlt
        227 => 125, // LeftMeta (Super)
        228 => 97,  // RightCtrl
        229 => 54,  // RightShift
        230 => 100, // RightAlt (AltGr)
        231 => 126, // RightMeta
        // Tout le reste : NON injecté (sécurité)
        _ => return None,
    };
    Some(code)
}
