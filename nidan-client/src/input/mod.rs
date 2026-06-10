//! Gestion des entrées clavier, souris et clipboard.
//!
//! Les `InputEvent` sont générés par le renderer SDL2 et transmis
//! au serveur via le canal de contrôle QUIC.

use anyhow::Result;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use nidan_proto::{
    InputBatch, InputEvent as ProtoInputEvent,
    InputEventPayload, KeyEvent, MouseEvent,
};

/// Événement d'entrée interne (type-safe, avant conversion proto)
#[derive(Debug, Clone)]
pub enum InputEvent {
    KeyDown {
        keycode:  u32,
        scancode: u32,
        shift:    bool,
        ctrl:     bool,
        alt:      bool,
        meta:     bool,
        repeat:   bool,
    },
    KeyUp {
        keycode:  u32,
        scancode: u32,
    },
    MouseMove {
        x:       f32,   // normalisé [0.0, 1.0]
        y:       f32,
        monitor: u32,
    },
    MouseDown {
        button: u32,
        x:      f32,
        y:      f32,
    },
    MouseUp {
        button: u32,
        x:      f32,
        y:      f32,
    },
    MouseScroll {
        dx: f32,
        dy: f32,
    },
    ClipboardChanged {
        content: Vec<u8>,
        mime:    String,
    },
}

impl InputEvent {
    /// Convertit en message proto `InputEvent`
    pub fn to_proto(&self, seq: u64) -> Option<ProtoInputEvent> {
        
        let (event_type, event) = match self {
            Self::KeyDown { keycode, scancode, shift, ctrl, alt, meta, repeat } => (
                1i32,
                nidan_proto::InputEventPayload::Key(KeyEvent {
                    keycode:  *keycode,
                    scancode: *scancode,
                    shift:    *shift,
                    ctrl:     *ctrl,
                    alt:      *alt,
                    meta:     *meta,
                    repeat:   *repeat,
                }),
            ),
            Self::KeyUp { keycode, scancode } => (
                2i32,
                nidan_proto::InputEventPayload::Key(KeyEvent {
                    keycode:  *keycode,
                    scancode: *scancode,
                    ..Default::default()
                }),
            ),
            Self::MouseMove { x, y, monitor } => (
                3i32,
                nidan_proto::InputEventPayload::Mouse(MouseEvent {
                    x: *x, y: *y,
                    monitor_idx: *monitor,
                    ..Default::default()
                }),
            ),
            Self::MouseDown { button, x, y } => (
                4i32,
                nidan_proto::InputEventPayload::Mouse(MouseEvent {
                    button: *button as i32,
                    x: *x, y: *y,
                    ..Default::default()
                }),
            ),
            Self::MouseUp { button, x, y } => (
                5i32,
                nidan_proto::InputEventPayload::Mouse(MouseEvent {
                    button: *button as i32,
                    x: *x, y: *y,
                    ..Default::default()
                }),
            ),
            Self::MouseScroll { dx, dy } => (
                6i32,
                nidan_proto::InputEventPayload::Mouse(MouseEvent {
                    scroll_dx: *dx,
                    scroll_dy: *dy,
                    ..Default::default()
                }),
            ),
            Self::ClipboardChanged { .. } => return None, // Géré séparément
        };

        Some(ProtoInputEvent {
            seq,
            event_type,
            timestamp_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_millis(),
            event: Some(event),
        })
    }
}

/// Agrège et envoie les InputEvent vers le serveur.
///
/// Stratégie d'optimisation :
/// - MouseMove : coalesce (garde seulement le plus récent sur la période)
/// - KeyDown/Up : envoi immédiat (latence critique)
/// - Batch : groupes d'événements envoyés toutes les 8ms (~ 1 frame réseau)
pub struct InputSender {
    seq: u64,
    batch_interval_ms: u64,
}

impl InputSender {
    pub fn new() -> Self {
        Self { seq: 0, batch_interval_ms: 8 }
    }

    /// Démarre la tâche d'envoi des inputs
    pub fn start(
        mut self,
        mut rx: mpsc::Receiver<InputEvent>,
        tx_proto: mpsc::Sender<InputBatch>,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> tokio::task::JoinHandle<Result<()>> {
        tokio::spawn(async move {
            let mut pending: Vec<ProtoInputEvent> = Vec::with_capacity(32);
            let mut last_mouse_move: Option<ProtoInputEvent> = None;
            let mut interval = tokio::time::interval(
                tokio::time::Duration::from_millis(self.batch_interval_ms)
            );

            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,

                    ev = rx.recv() => {
                        match ev {
                            None => break,
                            Some(event) => {
                                // Coalesce les MouseMove
                                if let InputEvent::MouseMove { .. } = &event {
                                    last_mouse_move = event.to_proto(self.seq);
                                    self.seq += 1;
                                } else if let Some(proto) = event.to_proto(self.seq) {
                                    pending.push(proto);
                                    self.seq += 1;

                                    // KeyDown/Up : flush immédiat pour latence minimale
                                    if matches!(event, InputEvent::KeyDown { .. } | InputEvent::KeyUp { .. }) {
                                        if let Some(mm) = last_mouse_move.take() {
                                            pending.push(mm);
                                        }
                                        if !pending.is_empty() {
                                            let batch = InputBatch {
                                                events: std::mem::take(&mut pending)
                                            };
                                            if tx_proto.send(batch).await.is_err() { break; }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    _ = interval.tick() => {
                        // Flush périodique du batch
                        if let Some(mm) = last_mouse_move.take() {
                            pending.push(mm);
                        }
                        if !pending.is_empty() {
                            let batch = InputBatch {
                                events: std::mem::take(&mut pending)
                            };
                            debug!(count = batch.events.len(), "flush batch inputs");
                            if tx_proto.send(batch).await.is_err() { break; }
                        }
                    }
                }
            }

            Ok(())
        })
    }
}

impl Default for InputSender {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_keydown_to_proto() {
        let ev = InputEvent::KeyDown {
            keycode: 65, scancode: 30,
            shift: false, ctrl: true, alt: false, meta: false,
            repeat: false,
        };
        let proto = ev.to_proto(42).unwrap();
        assert_eq!(proto.seq, 42);
        match proto.event {
            Some(nidan_proto::InputEventPayload::Key(k)) => {
                assert_eq!(k.keycode, 65);
                assert!(k.ctrl);
                assert!(!k.shift);
            }
            _ => panic!("mauvais type d'événement"),
        }
    }

    #[test]
    fn test_mouse_move_to_proto() {
        let ev = InputEvent::MouseMove { x: 0.5, y: 0.75, monitor: 0 };
        let proto = ev.to_proto(1).unwrap();
        match proto.event {
            Some(nidan_proto::InputEventPayload::Mouse(m)) => {
                assert!((m.x - 0.5).abs() < 0.001);
                assert!((m.y - 0.75).abs() < 0.001);
            }
            _ => panic!("mauvais type d'événement"),
        }
    }

    #[test]
    fn test_clipboard_returns_none() {
        let ev = InputEvent::ClipboardChanged {
            content: b"test".to_vec(),
            mime: "text/plain".to_string(),
        };
        // Le clipboard est géré séparément
        assert!(ev.to_proto(0).is_none());
    }
}
