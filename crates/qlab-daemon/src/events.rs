//! Difusión de eventos del daemon a los clientes IPC suscritos.

use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::Mutex;

use qlab_protocol::ipc_msg::ServerEvent;

/// Cola por suscriptor; si un cliente no lee, se le descartan eventos en vez de frenar al daemon.
const SUBSCRIBER_QUEUE: usize = 1024;

#[derive(Default)]
pub struct EventHub {
    subscribers: Mutex<Vec<SyncSender<ServerEvent>>>,
}

impl EventHub {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn subscribe(&self) -> Receiver<ServerEvent> {
        let (tx, rx) = sync_channel(SUBSCRIBER_QUEUE);
        self.subscribers.lock().expect("envenenado").push(tx);
        rx
    }

    /// Entrega el evento a todos los suscriptores vivos. Nunca bloquea.
    pub fn publish(&self, event: ServerEvent) {
        let mut subs = self.subscribers.lock().expect("envenenado");
        subs.retain(|tx| match tx.try_send(event.clone()) {
            Ok(()) | Err(TrySendError::Full(_)) => true,
            Err(TrySendError::Disconnected(_)) => false,
        });
    }

    pub fn subscriber_count(&self) -> usize {
        self.subscribers.lock().expect("envenenado").len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivers_to_every_subscriber_and_drops_dead_ones() {
        let hub = EventHub::new();
        let (a, b) = (hub.subscribe(), hub.subscribe());
        hub.publish(ServerEvent::ShuttingDown);
        assert_eq!(a.try_recv().unwrap(), ServerEvent::ShuttingDown);
        assert_eq!(b.try_recv().unwrap(), ServerEvent::ShuttingDown);
        drop(b);
        hub.publish(ServerEvent::SafeMute { engaged: true });
        assert_eq!(hub.subscriber_count(), 1);
        assert!(a.try_recv().is_ok());
    }

    #[test]
    fn a_slow_subscriber_never_blocks_the_publisher() {
        let hub = EventHub::new();
        let slow = hub.subscribe();
        for i in 0..5_000 {
            hub.publish(ServerEvent::PlayheadMoved { cue: Some(i) });
        }
        assert_eq!(hub.subscriber_count(), 1, "sigue suscrito aunque se llenó su cola");
        assert_eq!(slow.try_iter().count(), SUBSCRIBER_QUEUE);
    }
}
