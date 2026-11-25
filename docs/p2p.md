# Peer-to-peer sync for standalone apps

An app built on Drive (Season, Studio) can sync its documents between
the account's devices with **no Drive app, no agent and no node**
(drive#182). Each device runs its own rooms (`drive-sync`), keeps its
copies in a folder, and links to the other devices over iroh.

```text
 window ──in-process──▶ this device's rooms ──iroh (drive/sync/1)──▶ other devices' rooms
                               │
                               └──WebSocket (optional)──▶ a drive-node: one more peer
```

## Turning it on

In a Dioxus app (`drive-dioxus`, feature `p2p`, native builds):

```rust
use drive_dioxus::p2p::{PeerOptions, Settings};

fn main() {
    drive_dioxus::peer_to_peer(PeerOptions {
        // our relay (empty: n0's public ones)
        settings: Settings { relay: Some("https://relay.example".into()), local_only: false },
        mdns: true,               // find this account's devices nearby
        node: None,               // or Some("wss://drive.example/sync") to keep copies there too
        ..Default::default()
    }.trusting_the_network());   // until drive#183: see "Who is let in"
    dioxus::launch(App);
}
```

`DriveProvider`, `use_doc` and `Doc` are unchanged. With no node named, the
app opens at once on this device's copies and never asks a node for
anything. `drive_dioxus::peers()` gives the device's [`Peers`]: its
`ticket()` to pair with, `dial_ticket()` another device's, `connected()`.

Without Dioxus: `drive_client::Drive::peer_to_peer(opts, token)` (feature
`p2p`) returns the `Drive`, its events and its `Peers`.

`Drive::is_restored(id)` / `Doc::restored()` say when this device's own
copy of a document has loaded (found or not): offline, an empty document
is really empty only then.

## Who is let in

Until devices are vouched for by their account (drive#183), a device lets
in only the devices in `PeerOptions::peers` and those it dialed, or every
Drive device on the network with `trusting_the_network()`. A device that
is not let in is refused at the handshake, both ways.

## How it works

- **Rooms** (`drive-sync`): each open document, who has it open, their
  presence; a join is answered with what the joiner lacks by version
  vector. A room passes on only what was new to it, once per device, so a
  mesh of devices settles. drive-node runs on the same rooms.
- **Links**: a device is the client of each device it is connected to,
  joining there every document a window here has open; each connection
  carries this both ways (one QUIC stream per direction).
- **Discovery**: mDNS (service `drive`), addresses dialed (a ticket, the
  account's directory from drive#183), a relay for everything else.
- **Presence** rides the same connection (`ToNode::Presence` frames): who
  is here and what they are doing reach every directly connected device.
- **Other protocols** can share the endpoint (`PeerOptions::protocols`),
  each behind the same door as the documents: the graph's derived data
  (`drive/graph/1`, docs/graph.md). `Peers::endpoint` asks the connected
  devices over them.
