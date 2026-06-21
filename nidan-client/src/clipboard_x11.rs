//! Propriétaire de la sélection X CLIPBOARD côté client (poste local).
//!
//! Symétrique du module serveur : quand le client reçoit un presse-papier du
//! serveur (sens serveur→client), il le rend disponible aux applications du
//! POSTE LOCAL — un Ctrl+V colle ce contenu. Mécanique X11 identique :
//! posséder CLIPBOARD via une fenêtre cachée et répondre aux SelectionRequest.
//!
//! Spécifique au poste local sous X11 (sous Wayland, ce module ne s'active pas ;
//! la feature x11-clipboard reste optionnelle).

#![cfg(feature = "x11-clipboard")]

use anyhow::{Context, Result};
use std::sync::{Arc, Mutex};
use tracing::{debug, warn};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::COPY_DEPTH_FROM_PARENT;

/// Atomes X nécessaires à la gestion de la sélection.
struct Atoms {
    clipboard: Atom,
    utf8_string: Atom,
    targets: Atom,
    string: Atom,
    text: Atom,
}

fn intern(conn: &RustConnection, name: &str) -> Result<Atom> {
    Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
}

/// Propriétaire de la sélection CLIPBOARD. Le contenu est partagé (Arc<Mutex>)
/// avec le thread serveur qui le met à jour à chaque transfert reçu.
pub struct ClipboardOwner {
    content: Arc<Mutex<Vec<u8>>>,
}

impl ClipboardOwner {
    /// Démarre le propriétaire de sélection sur le display par défaut ($DISPLAY).
    /// Utilisé côté client (poste local).
    pub fn start_default() -> Result<Self> {
        Self::start_on(None)
    }

    /// Démarre le propriétaire de sélection sur le display donné.
    /// Lance un thread qui possède CLIPBOARD et répond aux requêtes de collage.
    pub fn start(display: u32) -> Result<Self> {
        Self::start_on(Some(format!(":{display}")))
    }

    fn start_on(display: Option<String>) -> Result<Self> {
        let content = Arc::new(Mutex::new(Vec::<u8>::new()));
        let content_thread = Arc::clone(&content);

        // Connexion X dédiée à ce thread (une connexion X n'est pas partageable
        // entre threads de façon sûre pour la boucle d'événements).
        let (conn, screen_num) = x11rb::connect(display.as_deref())
            .context("connexion X11 (clipboard)")?;
        let screen = &conn.setup().roots[screen_num];
        let root = screen.root;

        let atoms = Atoms {
            clipboard: intern(&conn, "CLIPBOARD")?,
            utf8_string: intern(&conn, "UTF8_STRING")?,
            targets: intern(&conn, "TARGETS")?,
            string: intern(&conn, "STRING")?,
            text: intern(&conn, "TEXT")?,
        };

        // Fenêtre cachée (1x1, jamais mappée) qui détient la sélection.
        let win = conn.generate_id()?;
        conn.create_window(
            COPY_DEPTH_FROM_PARENT, win, root,
            0, 0, 1, 1, 0,
            WindowClass::INPUT_OUTPUT, screen.root_visual,
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )?;
        conn.flush()?;

        std::thread::spawn(move || {
            if let Err(e) = run_selection_loop(conn, win, atoms, content_thread) {
                warn!(error = %e, "boucle de sélection CLIPBOARD arrêtée");
            }
        });

        debug!("propriétaire CLIPBOARD démarré");
        Ok(Self { content })
    }

    /// Met à jour le contenu servi à la VM (déjà filtré par la politique).
    pub fn set_content(&self, data: &[u8]) {
        if let Ok(mut c) = self.content.lock() {
            *c = data.to_vec();
        }
    }
}

/// Boucle d'événements : prend la sélection puis répond aux SelectionRequest.
fn run_selection_loop(
    conn: RustConnection,
    win: Window,
    atoms: Atoms,
    content: Arc<Mutex<Vec<u8>>>,
) -> Result<()> {
    // Prise de possession de CLIPBOARD par notre fenêtre cachée.
    conn.set_selection_owner(win, atoms.clipboard, x11rb::CURRENT_TIME)?;
    conn.flush()?;

    loop {
        let event = conn.wait_for_event()?;
        match event {
            Event::SelectionRequest(req) => {
                let prop = handle_selection_request(&conn, win, &atoms, &content, &req)
                    .unwrap_or(x11rb::NONE);
                // Notifier le demandeur du résultat (propriété remplie ou NONE).
                let notify = SelectionNotifyEvent {
                    response_type: SELECTION_NOTIFY_EVENT,
                    sequence: 0,
                    time: req.time,
                    requestor: req.requestor,
                    selection: req.selection,
                    target: req.target,
                    property: prop,
                };
                conn.send_event(false, req.requestor, EventMask::NO_EVENT, notify)?;
                conn.flush()?;
            }
            // Un autre client a pris la sélection : on cesse d'être propriétaire.
            Event::SelectionClear(_) => {
                debug!("CLIPBOARD : possession perdue (autre propriétaire)");
            }
            _ => {}
        }
    }
}

/// Sert une SelectionRequest : remplit la propriété demandée avec le contenu
/// (pour les cibles texte) ou la liste des cibles supportées (TARGETS).
/// Retourne l'atome de propriété rempli, ou None si la cible n'est pas servie.
fn handle_selection_request(
    conn: &RustConnection,
    _win: Window,
    atoms: &Atoms,
    content: &Arc<Mutex<Vec<u8>>>,
    req: &SelectionRequestEvent,
) -> Result<Atom> {
    if req.target == atoms.targets {
        // Annonce des cibles supportées.
        let targets = [atoms.targets, atoms.utf8_string, atoms.string, atoms.text];
        let bytes: Vec<u8> = targets.iter().flat_map(|a| a.to_ne_bytes()).collect();
        conn.change_property(
            PropMode::REPLACE, req.requestor, req.property,
            AtomEnum::ATOM, 32, targets.len() as u32, &bytes,
        )?;
        return Ok(req.property);
    }

    if req.target == atoms.utf8_string
        || req.target == atoms.string
        || req.target == atoms.text
    {
        let data = content.lock().map(|c| c.clone()).unwrap_or_default();
        conn.change_property(
            PropMode::REPLACE, req.requestor, req.property,
            req.target, 8, data.len() as u32, &data,
        )?;
        return Ok(req.property);
    }

    // Cible non supportée.
    Ok(x11rb::NONE)
}
