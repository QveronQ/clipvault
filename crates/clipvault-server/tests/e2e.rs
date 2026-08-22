//! Test bout en bout : serveur réel + client REST/WebSocket.

use std::net::TcpStream;
use std::process::{Child, Command};
use std::time::Duration;

use clipvault_core::sync::{PushAck, PushItem, SyncEntry, SyncEvent};
use clipvault_core::types::{ContentKind, EntryMeta};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::WebSocket;

const TOKEN: &str = "jeton-de-test";
const ADDR: &str = "127.0.0.1:17701";

struct Server {
    child: Child,
    _data: tempfile::TempDir,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start_server() -> Server {
    let data = tempfile::TempDir::new().unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_clipvault-server"))
        .env("CLIPVAULT_TOKEN", TOKEN)
        .env("CLIPVAULT_LISTEN", ADDR)
        .env("CLIPVAULT_DATA_DIR", data.path())
        .spawn()
        .unwrap();
    // Attendre que /v1/health réponde.
    for _ in 0..50 {
        std::thread::sleep(Duration::from_millis(100));
        if ureq::get(&format!("http://{ADDR}/v1/health")).call().is_ok() {
            return Server { child, _data: data };
        }
    }
    panic!("le serveur n'a pas démarré");
}

fn push(item: &PushItem, device: &str) -> PushAck {
    ureq::post(&format!("http://{ADDR}/v1/push"))
        .header("Authorization", &format!("Bearer {TOKEN}"))
        .header("X-Device", device)
        .send_json(item)
        .unwrap()
        .body_mut()
        .read_json()
        .unwrap()
}

fn text_entry(id: &str, device: &str, text: &str) -> PushItem {
    PushItem::Entry(SyncEntry {
        meta: EntryMeta {
            id: id.into(),
            device_id: device.into(),
            kind: ContentKind::Text,
            mime: "text/plain;charset=utf-8".into(),
            size: text.len() as u64,
            preview: text.into(),
            thumb_path: None,
            created_at: 1,
            last_used_at: 1,
            pinned: false,
        },
        text: Some(text.into()),
        object_hash: None,
    })
}

fn connect_ws(device: &str, since: i64) -> WebSocket<MaybeTlsStream<TcpStream>> {
    let url = format!("ws://{ADDR}/v1/ws?token={TOKEN}&since={since}&device={device}");
    let (ws, _) = tungstenite::connect(url).unwrap();
    if let MaybeTlsStream::Plain(tcp) = ws.get_ref() {
        tcp.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    }
    ws
}

fn next_event(ws: &mut WebSocket<MaybeTlsStream<TcpStream>>) -> SyncEvent {
    loop {
        match ws.read().expect("lecture WS (timeout = aucun événement reçu)") {
            tungstenite::Message::Text(txt) => {
                return serde_json::from_str(txt.as_str()).unwrap()
            }
            _ => continue,
        }
    }
}

#[test]
fn push_replay_live_and_origin_filter() {
    let _server = start_server();

    // Auth refusée sans jeton.
    let status = ureq::post(&format!("http://{ADDR}/v1/push"))
        .header("X-Device", "a")
        .send_json(&text_entry("01E1", "a", "x"))
        .map(|r| r.status().as_u16())
        .unwrap_or_else(|e| match e {
            ureq::Error::StatusCode(code) => code,
            other => panic!("erreur inattendue: {other}"),
        });
    assert_eq!(status, 401);

    // Push d'une entrée depuis la machine "a".
    let ack = push(&text_entry("01AAA", "a", "depuis a"), "a");
    assert_eq!(ack.seq, 1);

    // Blob : PUT puis GET identique.
    let hash = blake3_hex(b"binaire");
    ureq::put(&format!("http://{ADDR}/v1/objects/{hash}"))
        .header("Authorization", &format!("Bearer {TOKEN}"))
        .send(&b"binaire"[..])
        .unwrap();
    let got = ureq::get(&format!("http://{ADDR}/v1/objects/{hash}"))
        .header("Authorization", &format!("Bearer {TOKEN}"))
        .call()
        .unwrap()
        .body_mut()
        .read_to_vec()
        .unwrap();
    assert_eq!(got, b"binaire");

    // "b" se connecte : replay de l'événement 1 (venu de "a").
    let mut ws = connect_ws("b", 0);
    let ev = next_event(&mut ws);
    assert_eq!(ev.seq, 1);
    assert_eq!(ev.origin, "a");

    // Événement live depuis "a" -> reçu par "b".
    push(&text_entry("01BBB", "a", "live"), "a");
    let ev = next_event(&mut ws);
    assert_eq!(ev.seq, 2);

    // "b" pousse : il ne doit PAS recevoir son propre événement (seq 3),
    // mais bien le suivant venu de "a" (seq 4).
    push(&text_entry("01CCC", "b", "de b"), "b");
    push(&text_entry("01DDD", "a", "encore a"), "a");
    let ev = next_event(&mut ws);
    assert_eq!(ev.seq, 4);
    assert_eq!(ev.origin, "a");

    // Reprise sur curseur : un nouveau client depuis seq=2 ne reçoit que 3 et 4.
    let mut ws2 = connect_ws("c", 2);
    assert_eq!(next_event(&mut ws2).seq, 3);
    assert_eq!(next_event(&mut ws2).seq, 4);
}

fn blake3_hex(data: &[u8]) -> String {
    // Petit hash hex 64 chars sans dépendre de blake3 dans les dev-deps :
    // le serveur ne vérifie que le format, pas l'algorithme.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in data {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}{h:016x}{h:016x}{h:016x}")
}
