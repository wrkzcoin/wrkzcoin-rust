# 08 - Peer-to-peer protocol

Source files: `src/p2p/LevinProtocol.{h,cpp}`, `src/p2p/P2pProtocolDefinitions.h`,
`src/p2p/P2pProtocolTypes.h`, `src/p2p/NetNode.{h,cpp}`,
`src/p2p/PeerListManager.cpp`, `src/p2p/ConnectionContext.h`,
`src/cryptonoteprotocol/CryptoNoteProtocolDefinitions.h`,
`src/cryptonoteprotocol/CryptoNoteProtocolHandler.cpp`. The operator-facing
description of the same behaviour is `NETWORKING.md` at the repo root.

Everything on the wire is either a Levin frame or a KV binary payload inside
one. Both formats are defined in `04-serialization.md`; this document gives
the messages and the behaviour.

## Transport

TCP, default port 17855, IPv4 always, IPv6 optional (dual-stack listener,
`NETWORKING.md`). One Levin conversation per connection, messages processed
in order. There is no encryption and no authentication; the network id in the
handshake is the only "password".

## Levin framing (`LevinProtocol.cpp`)

Every message is a 33-byte header followed by `m_cb` bytes of payload. The
header is a packed little-endian structure (`bucket_head2`, line 25):

| Offset | Size | Field | Value |
| --- | --- | --- | --- |
| 0 | 8 | `m_signature` | `0x0101010101012101` (little-endian on the wire: `01 21 01 01 01 01 01 01`) |
| 8 | 8 | `m_cb` | payload length |
| 16 | 1 | `m_have_to_return_data` | 1 for a request that expects a response, 0 for a notification or a response |
| 17 | 4 | `m_command` | command id |
| 21 | 4 | `m_return_code` | `int32`; 1 (`LEVIN_PROTOCOL_RETCODE_SUCCESS`) on a normal response, negative `LevinError` otherwise; 0 in requests |
| 25 | 4 | `m_flags` | `1` = request (`LEVIN_PACKET_REQUEST`), `2` = response (`LEVIN_PACKET_RESPONSE`) |
| 29 | 4 | `m_protocol_version` | `1` |

Rules (`readCommand`, line 67): a wrong signature closes the connection; a
payload larger than 100,000,000 bytes closes the connection; a frame with
`m_have_to_return_data == 0` and no response flag is a notification. A
request handler's reply carries the same command id, the response flag and
the return code (`sendReply`, line 105). A command the receiver does not
know is answered with return code `-6`
(`ERROR_CONNECTION_HANDLER_NOT_DEFINED`) and an empty body
(`NetNode.cpp:2868`).

Payloads are KV binary sections (`04-serialization.md`, "KV binary"), one
top-level object per message. `LevinProtocol::encode`/`decode` are exactly
`storeToBinaryKeyValue`/`loadFromBinaryKeyValue`.

Only two commands are ever *invoked* (request/response): `COMMAND_HANDSHAKE`
and `COMMAND_PING`. `COMMAND_TIMED_SYNC` is sent as a request but its
response is handled asynchronously (`NetNode.cpp:323`). Every
CryptoNoteProtocol command (2001–2010) is a notification and MUST be sent
with `m_have_to_return_data = 0` (`post_notify`, `CryptoNoteProtocolHandler.cpp:56`).

## P2P commands (1000-series, `P2pProtocolDefinitions.h`)

Field names are the KV binary keys. Types are the KV binary types the C++
serializer emits for the C++ field type.

### Shared structures

`basic_node_data` (line 49):

| Key | Type | Meaning |
| --- | --- | --- |
| `network_id` | string (16 bytes) | MUST equal `b50c4a6ccf52574165f991a4b6c143e9` |
| `version` | uint8 | sender's `P2P_CURRENT_VERSION` (19) |
| `peer_id` | uint64 | random, generated once and stored in `p2pstate.wrkz.bin` |
| `local_time` | uint64 | unix time |
| `my_port` | uint32 | the sender's listening port, or 0 when `--hide-my-port` |

Order of serialization is `network_id, version, peer_id, local_time, my_port`
(`version` is zeroed on input before reading, so a missing field reads 0).

`CORE_SYNC_DATA` (line 75):

| Key | Type | Meaning |
| --- | --- | --- |
| `current_height` | uint32 | `top block index + 1` |
| `top_id` | string (32 bytes) | top block hash |
| `capability_flags` | uint32 | bit 0 `NODE_CAPABILITY_FLAG_PRUNED`, bit 1 `NODE_CAPABILITY_FLAG_LITE`; absent reads 0 |
| `pruned_node_height` | uint32 | lowest height a pruned node still serves; 0 otherwise |
| `lite_start_height` | uint32 | lowest height a lite node serves; 0 otherwise |

Peer list entries are sent as one binary blob each, not as KV arrays
(`serializeAsBinary`, `04-serialization.md`):

`PeerlistEntry` (24 bytes, `P2pProtocolTypes.h:36`): `ip` uint32 (network
byte order as stored in `NetworkAddress.ip`), `port` uint32, `id` uint64,
`last_seen` uint64. `local_peerlist` is the concatenation of at most 250
entries.

`PeerlistEntry6` (**40 bytes**, line 29): `id` uint64 at offset 0,
`last_seen` uint64 at 8, `ip` 16 bytes at 16, `port` uint32 at 32. The struct
is not packed, so its `uint64_t` members give it 8-byte alignment and
`sizeof` rounds the 36 bytes of fields up to 40, leaving four bytes of tail
padding at offset 36. `serializeAsBinary` writes `sizeof(T)` per element, so
that padding is on the wire (with whatever the sender's memory held) and the
blob length is always a multiple of 40; the C++ reader throws
"Invalid blob size given!" otherwise. A port MUST use stride 40 and ignore
bytes 36–39. Only sent to and read from peers with `version >= 19`.

### `COMMAND_HANDSHAKE` = 1001

Request `{ node_data: basic_node_data, payload_data: CORE_SYNC_DATA }`.
Response adds `local_peerlist` and `local_peerlist6` blobs.

Outgoing side (`NodeServer::handshake`, `NetNode.cpp:860`): send, then
require in the response: same network id; `version >= 16`
(`P2P_MINIMUM_VERSION`); a version at least 2 above ours only logs a
warning. Merge the peer list (below). Feed `payload_data` to the sync
state machine as *initial*. Record the peer id; if it equals our own, drop
(connected to self). The handshake has a 15 s budget (3× connection timeout).

Incoming side (`handle_handshake`, line 2240): same checks on the request;
reject if the connection is not inbound or already has a peer id (double
handshake). If the request carries a non-zero `my_port`, perform a **back
ping**: open a new connection to `remote_ip:my_port`, invoke
`COMMAND_PING`, and require status `OK` and the same `peer_id`
(`try_ping`, line 2160). Only a peer that passes the back ping is added to
the white list; this is how the network avoids advertising unreachable
addresses. Then answer with our node data, sync data and peer list.

### `COMMAND_TIMED_SYNC` = 1002

Request `{ payload_data }`. Response `{ local_time, payload_data,
local_peerlist, local_peerlist6 }`. Sent every 60 s to every connection in
state normal or idle (`timedSyncLoop`, line 2808). Both directions feed the
sync data to the state machine as *not initial*, and the response's peer
list is merged. Loopback and private addresses are never added
(`is_ip_allowed`, `PeerListManager.cpp:126`) unless `--allow-local-ip`.

### `COMMAND_PING` = 1003

Empty request; response `{ status: "OK", peer_id }`.

### Peer list handling (`handle_remote_peerlist`, line 1912)

Keep at most 250 received entries. Reject the whole list if any
`last_seen` is in the sender's future relative to its `local_time`.
Shift every `last_seen` by `now − sender local_time`. Merge into the gray
list; entries already white are ignored. White list holds 1000, gray 5000,
oldest dropped. A peer becomes white after a successful outbound handshake
or a successful back ping. Sent lists are the white list sorted by
`last_seen` descending, entries with `last_seen == 0` skipped.

Connection making (`connections_maker`, line 1537; summary in
`NETWORKING.md`): target 15 outgoing, 70% from the white list; addresses
that failed are skipped for 10 minutes; once a minute one random gray peer
is dialled for its list; seeds are asked for peer lists only when the lists
are empty or the node is stuck, at most every 5 minutes. Incoming
connections are capped at 15 and refused from banned addresses. A port need
not copy the selection heuristics, but SHOULD keep the white/gray semantics
and the back-ping requirement, because every other node's lists are built
from them.

### Peer state file (`p2pstate.wrkz.bin`)

**Not KV binary.** `NodeServer::store_config` (`NetNode.cpp:827`) writes
through `BinaryOutputStreamSerializer`, so the file is a bare sequence of
varints and fixed-width fields with no names and no type tags. Reading it
as portable storage fails.

Top level (`NodeServer::serialize`, line 291): `version` uint8 (= 1),
then the peerlist, then `peer_id` uint64. The peerlist
(`PeerlistManager::serialize`, `PeerListManager.cpp:14`): `version` uint8
(= 2), then `whitelist` and `graylist` as length-prefixed arrays of
`{ adr: { ip uint32, port uint32 }, id uint64, last_seen uint64 }`, then,
only when the version is at least 2, `whitelist6` and `graylist6` as
arrays of `{ adr: { ip 16 bytes, port uint32 }, id, last_seen }`. A
version-1 file ends after the two IPv4 lists and must still load. A port
may use its own file; `--p2p-reset-peerstate` semantics (new peer id,
empty lists) should exist.

## CryptoNote protocol commands (2000-series)

Definitions in `CryptoNoteProtocolDefinitions.h`; serializers for the ones
with custom layouts in `CryptoNoteProtocolHandler.cpp:101-199`. All are
notifications.

Block payload `RawBlockLegacy` (line 29): `{ block: string, txs: array of
string }` — the serialized block template and the serialized transactions,
in the block's `tx_hashes` order. `txs` is a KV array of strings (a
zero-length array is omitted entirely by the writer, and readers must treat
a missing `txs` as empty; see `04-serialization.md`).

| Id | Name | Body | Purpose |
| --- | --- | --- | --- |
| 2001 | `NOTIFY_NEW_BLOCK` | `{ b: RawBlockLegacy, current_blockchain_height: uint32, hop: uint32 }` | full block relay (legacy peers only) |
| 2002 | `NOTIFY_NEW_TRANSACTIONS` | `{ txs: array of string }` | transaction relay; also the reply to 2008 and 2010 |
| 2003 | `NOTIFY_REQUEST_GET_OBJECTS` | `{ txs: blob of hashes, blocks: blob of hashes }` | ask for blocks by hash; `txs` is always empty on this network |
| 2004 | `NOTIFY_RESPONSE_GET_OBJECTS` | `{ txs: array of string, blocks: array of RawBlockLegacy, missed_ids: blob of hashes, current_blockchain_height: uint32 }` | the blocks |
| 2006 | `NOTIFY_REQUEST_CHAIN` | `{ block_ids: blob of hashes }` | sparse chain (below) |
| 2007 | `NOTIFY_RESPONSE_CHAIN_ENTRY` | `{ start_height: uint32, total_height: uint32, m_block_ids: blob of hashes }` | up to 10,000 hashes from the first common block |
| 2008 | `NOTIFY_REQUEST_TX_POOL` | `{ txs: blob of hashes }` | "here is my pool; send me what I lack" |
| 2009 | `NOTIFY_NEW_LITE_BLOCK` | `{ current_blockchain_height: uint32, hop: uint32, blockTemplate: string }` | block relay without transactions (peers with `version >= 4`, i.e. everyone) |
| 2010 | `NOTIFY_MISSING_TXS` | `{ current_blockchain_height: uint32, blockHash: string, missing_txs: blob of hashes }` | ask for the transactions a lite block referenced |

"blob of hashes" means `serializeAsBinary` of a vector of 32-byte hashes: one
KV string of `32·n` bytes. Note the key spellings (`b`, `m_block_ids`,
`blockTemplate`) are exact.

Every handler updates the peer's observed height from
`current_blockchain_height` first.

### Sync state machine (`ConnectionContext.h:38`, handler file)

Per-connection states: `before_handshake`, `synchronizing`, `idle`,
`normal`, `sync_required`, `pool_sync_required`, `shutdown`.

`process_payload_sync_data` (`CryptoNoteProtocolHandler.cpp:386`), run on
every handshake and timed sync with the peer's `CORE_SYNC_DATA`:

1. Record pruned/lite flags and heights.
2. If we already have the peer's `top_id`: on the initial call mark the
   connection `pool_sync_required` (and, if this is the first time any
   peer says we are at the top, declare ourselves synchronized); otherwise
   `normal`.
3. Else, if the peer cannot serve our chain (its lite/pruned floor is above
   our height, or after `PRUNE_CAPABILITY_FORK_HEIGHT` it is pruned and we
   are a full node), keep it for relay only: `pool_sync_required`/`normal`.
4. Else, if at least `--sync-max-peers` (default 3) connections are already
   syncing, also relay only.
5. Else mark `sync_required`.

The connection loop (`NetNode.cpp:2843`) turns `sync_required` into
`synchronizing` and sends `NOTIFY_REQUEST_CHAIN`; it turns
`pool_sync_required` into `normal` and sends `NOTIFY_REQUEST_TX_POOL` with
our pool's hashes.

`NOTIFY_REQUEST_CHAIN` carries the **sparse chain**
(`Core::doBuildSparseChain`, `Core.cpp:4081`): the top hash, then the
blocks at distances 1, 2, 4, 8, ... below it (`for (i = 1; i < blockIndex;
i *= 2)`), and the genesis hash appended when the top is not itself
genesis. There is no run of ten sequential hashes: the comment in
`CryptoNoteProtocolDefinitions.h` describes an older scheme, and the
deployed loop doubles from the first step. At mainnet height the list is
about 25 hashes. The receiver (`handle_request_chain`, line 1308) requires a non-empty
list ending in its genesis hash, finds the first hash it knows
(`findBlockchainSupplement`), and answers `NOTIFY_RESPONSE_CHAIN_ENTRY` with
`start_height` = index of that block, `total_height` = its own height, and
up to 10,000 consecutive hashes starting **with** the common block.

`handle_response_chain_entry` (line 1452): require non-empty; require the
first hash to be known; set `last_response_height = start + n − 1`;
everything after the first unknown hash goes into `needed_objects`; then
`request_missing_objects`.

`request_missing_objects` (line 1342): if `needed_objects` is non-empty,
send `NOTIFY_REQUEST_GET_OBJECTS` for the next batch (batch size adapts
between `--sync-batch-min` 120 and `--sync-batch-max` 600 blocks, bounded by
a byte budget estimated from the average block size, 2–48 MiB), moving the
hashes into `requested_objects`. Else if `last_response_height <
remote_height − 1`, send another `NOTIFY_REQUEST_CHAIN`. Else the peer is
fully synced: request its pool and go `normal`.

`handle_response_get_objects` (line 830):

- a response we deliberately abandoned (`m_discard_next_objects_response`)
  is dropped silently;
- `current_blockchain_height` below our last response height: drop peer;
- each block must parse, must be in `requested_objects` (else "wasn't
  requested", drop), and its transaction count must match `tx_hashes`;
- if some requested blocks are missing: if every missing one is listed in
  `missed_ids`, the peer reorganised; clear and re-request the chain (this
  counts as a sync failure); otherwise drop the peer;
- pipelining: if more `needed_objects` remain, the *next* batch is requested
  before this one is validated;
- `processObjects` adds each block through `Core::addBlock`:
  validation failure or deserialization failure → drop the peer (a
  checkpoint mismatch also bans it for 900 s); a rejected-as-orphan block →
  re-request the chain, up to 3 times, then drop; "already exists" →
  go `idle` and discard the pipelined response.

Failures are counted per peer and a peer over `--sync-peer-failure-threshold`
(2) is demoted from syncing.

### Block relay

On receiving a block (`handle_notify_new_block`, line 692, and the lite
path `doPushLiteBlock`, line 1155): ignore unless the connection is
`normal`; add through `Core::addBlock`; if added to the main chain or
caused a switch, increment `hop` and relay; if it became an alternative and
the peer is taller, request its chain; if rejected as orphan, request the
peer's chain; on validation failure drop the peer.

`relayBlock` (line 1585) sends `NOTIFY_NEW_LITE_BLOCK` to peers with
`version >= 4` and `NOTIFY_NEW_BLOCK` to the rest; in practice every peer
takes the lite form. A lite block carries only the template; the receiver
takes the transactions it already has from its pool and asks for the rest
with `NOTIFY_MISSING_TXS`; the answer arrives as `NOTIFY_NEW_TRANSACTIONS`
on the same connection, which the receiver recognises because it has a
pending lite block for that peer (`handle_notify_new_transactions`, line
764). A peer that cannot supply a requested transaction is dropped
(`handle_notify_missing_txs`, line 1546).

### Transaction relay

`NOTIFY_NEW_TRANSACTIONS` (line 752): ignore unless `normal`; each
transaction goes through pool admission (`06-transactions.md`); the ones
accepted are relayed to every other connection. `NOTIFY_REQUEST_TX_POOL`
(line 1508): compute `getPoolChanges` against the peer's hashes and send
the transactions the peer lacks as `NOTIFY_NEW_TRANSACTIONS`. A node MUST
tolerate receiving transactions it already has.

## Timeouts and limits

| What | Value |
| --- | --- |
| connect | 5 s (`P2P_DEFAULT_CONNECTION_TIMEOUT`) |
| handshake | 15 s |
| back ping | 10 s (2× connection timeout) |
| a write that does not complete | 120 s (`P2P_DEFAULT_INVOKE_TIMEOUT`), connection closed |
| per-connection write queue | 32 MiB, connection closed when exceeded |
| Levin payload | 100 MB hard, 50 MB configured (`P2P_DEFAULT_PACKET_MAX_SIZE`) |
| timed sync period | 60 s |

## Bans

In-memory, by IPv4 or IPv6 address, default 900 s (`ban_host`, line 2391;
console `ban add/delete/list`). Applied on accept and before dialling.
The only automatic ban is the checkpoint mismatch above.

## Lite and pruned peers

A node advertises `NODE_CAPABILITY_FLAG_LITE` with `lite_start_height` when
it holds full blocks only from that height (`LITENODE.md`), or
`NODE_CAPABILITY_FLAG_PRUNED` with `pruned_node_height` when it dropped
block bodies below a depth. Peers use the floor to avoid asking for blocks
the node cannot serve (`peerCanServeOurChain`, line 1946) and, from
`PRUNE_CAPABILITY_FORK_HEIGHT` 4,500,000, full nodes stop syncing from
pruned peers at all. A lite node also refuses to start unless several
peers agree the network is at least 20,160 blocks above its lite height
(`process_payload_sync_data`, line 415). None of this changes validation.

## Acceptance for this document

1. The port handshakes with `node-fin.wrkz.work:17855` and
   `node-wrkz.btipz.com:17855`, receives a peer list, and passes a back
   ping from them (requires a reachable listening port).
2. It syncs the chain from genesis to the tip through
   `NOTIFY_REQUEST_CHAIN` / `NOTIFY_REQUEST_GET_OBJECTS` alone, validating
   every block, and its top hash matches `/info` on the seed node.
3. It receives and relays lite blocks and transactions at the tip, and a
   C++ node connected to it stays in state `normal` (visible in the C++
   node's `print_cn` console output).
4. A fuzzed Levin stream (bad signature, oversized `m_cb`, truncated KV
   payload, unknown command) never crashes the node and produces the
   documented close or error reply.
