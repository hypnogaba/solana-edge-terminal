//! Shreds in, transaction signatures out.
//!
//! The decoding itself is not ours: `solana-stream-sdk` (Apache-2.0) already
//! implements the shred wire format, the FEC buffer, Reed-Solomon recovery and
//! deshredding into entries, and it exposes those as separate steps rather than
//! only as a socket loop. That matters here, because DoubleZero delivers shreds
//! over a GRE tunnel where the SDK's own `UdpShredReceiver` would receive
//! nothing (see capture.rs). So we own the packets and borrow the decoding.
//!
//! What comes out is the earliest moment this host could possibly know a
//! transaction exists: shreds carry it while the block is still being built,
//! before any RPC has a confirmed block to serve.

use std::time::Instant;

use solana_stream_sdk::shreds_udp::{
    decode_udp_datagram, deshred_shreds_to_entries, insert_shred, DeshredPolicy,
    ShredInsertOutcome, ShredsUdpConfig, ShredsUdpState, UdpDatagram,
};

use crate::capture::CapturedPacket;

/// A transaction observed on some feed, stamped on arrival.
#[derive(Debug, Clone)]
pub struct SeenTx {
    pub signature: String,
    pub slot: u64,
    pub at: Instant,
    /// Static account keys, for matching against watched pools. Keys behind an
    /// address lookup table are not here: the RPC lane has none at all, which
    /// is why the bot only ever triggers off the shred lane.
    pub account_keys: Vec<solana_pubkey::Pubkey>,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct PipelineCounts {
    pub packets: u64,
    pub shreds: u64,
    pub batches_ready: u64,
    pub deshred_errors: u64,
    pub entries: u64,
    pub transactions: u64,
    /// Packets whose decoding panicked. Shreds are hostile input from a
    /// network, and a panic here would otherwise take the whole terminal down
    /// mid-stream.
    pub panics: u64,
}

pub struct ShredPipeline {
    state: ShredsUdpState,
    cfg: ShredsUdpConfig,
    policy: DeshredPolicy,
    counts: PipelineCounts,
}

impl ShredPipeline {
    pub fn new(cfg: ShredsUdpConfig) -> Self {
        let policy = DeshredPolicy { require_code_match: cfg.require_code_match };
        let state = ShredsUdpState::new(&cfg);
        Self { state, cfg, policy, counts: PipelineCounts::default() }
    }

    pub fn counts(&self) -> PipelineCounts {
        self.counts
    }

    /// Feed one captured packet in, and never let it kill the process.
    ///
    /// A shred is bytes from a network. The decoding below is third-party code
    /// walking those bytes, and a panic in it would abort this task, the loop
    /// that owns it, and the terminal, on air. A panicking packet is counted
    /// and dropped instead.
    pub async fn on_packet(&mut self, packet: &CapturedPacket) -> Vec<SeenTx> {
        match futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(
            self.decode_packet(packet),
        ))
        .await
        {
            Ok(seen) => seen,
            Err(_) => {
                self.counts.panics += 1;
                tracing::error!(
                    from = %packet.from,
                    bytes = packet.payload.len(),
                    "shred decoding panicked; packet dropped"
                );
                Vec::new()
            }
        }
    }

    async fn decode_packet(&mut self, packet: &CapturedPacket) -> Vec<SeenTx> {
        self.counts.packets += 1;
        let datagram = UdpDatagram {
            payload: packet.payload.clone(),
            received_at: packet.at,
            from: packet.from,
        };
        let Some(decoded) = decode_udp_datagram(&datagram, &self.state, &self.cfg).await else {
            return Vec::new();
        };
        self.counts.shreds += 1;

        let outcome =
            insert_shred(decoded, &datagram, &self.state, &self.cfg, &self.policy).await;
        let ShredInsertOutcome::Ready(batch) = outcome else {
            return Vec::new();
        };
        self.counts.batches_ready += 1;

        let entries = match deshred_shreds_to_entries(&batch.shreds) {
            Ok(entries) => entries,
            Err(err) => {
                self.counts.deshred_errors += 1;
                tracing::debug!(slot = batch.key.slot, %err, "deshred failed");
                return Vec::new();
            }
        };
        self.counts.entries += entries.len() as u64;

        // The arrival time is the packet's, not now: the work above is ours and
        // charging the feed for it would flatter the public side of the race.
        let mut seen = Vec::new();
        for entry in &entries {
            for transaction in &entry.transactions {
                let Some(signature) = transaction.signatures.first() else {
                    continue;
                };
                self.counts.transactions += 1;
                seen.push(SeenTx {
                    signature: signature.to_string(),
                    slot: batch.key.slot,
                    at: packet.at,
                    account_keys: transaction.message.static_account_keys().to_vec(),
                });
            }
        }
        seen
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use solana_entry::entry::Entry;
    use solana_hash::Hash;
    use solana_keypair::Keypair;
    use solana_ledger::shred::{ProcessShredsStats, ReedSolomonCache, Shred, Shredder};
    use solana_message::{Message, VersionedMessage};
    use solana_signature::Signature;
    use solana_transaction::versioned::VersionedTransaction;

    use super::*;

    /// A slot far enough along to look like today's mainnet, so the SDK's slot
    /// window does not treat the test data as implausible.
    const SLOT: u64 = 443_957_126;

    fn transaction(seed: u8) -> VersionedTransaction {
        VersionedTransaction {
            signatures: vec![Signature::from([seed; 64])],
            message: VersionedMessage::Legacy(Message::default()),
        }
    }

    fn shreds_for(transactions: Vec<VersionedTransaction>) -> Vec<Shred> {
        let entries = vec![Entry {
            num_hashes: 1,
            hash: Hash::default(),
            transactions,
        }];
        let shredder = Shredder::new(SLOT, SLOT - 1, 0, 0).expect("shredder");
        shredder
            .make_merkle_shreds_from_entries(
                &Keypair::new(),
                &entries,
                true,
                Hash::default(),
                0,
                0,
                &ReedSolomonCache::default(),
                &mut ProcessShredsStats::default(),
            )
            .collect()
    }

    fn packet(payload: Vec<u8>) -> CapturedPacket {
        CapturedPacket {
            payload,
            at: Instant::now(),
            from: "10.0.0.1:7733".parse::<SocketAddr>().unwrap(),
            dst_port: 7733,
        }
    }

    async fn run(shreds: Vec<Shred>) -> (Vec<SeenTx>, PipelineCounts) {
        let mut pipeline = ShredPipeline::new(ShredsUdpConfig::defaults());
        let mut seen = Vec::new();
        for shred in &shreds {
            seen.extend(pipeline.on_packet(&packet(shred.payload().to_vec())).await);
        }
        (seen, pipeline.counts())
    }

    #[tokio::test]
    async fn a_transaction_shredded_and_captured_comes_back_out() {
        // The whole point of the feed, exercised without the feed: entries are
        // turned into real shreds by the same code a validator uses, fed in as
        // captured packets, and must come back out as the signature we put in.
        let expected = Signature::from([7u8; 64]).to_string();
        let (seen, counts) = run(shreds_for(vec![transaction(7)])).await;

        assert!(
            seen.iter().any(|tx| tx.signature == expected),
            "signature not recovered; counts = {counts:?}"
        );
        assert!(seen.iter().all(|tx| tx.slot == SLOT));
        assert_eq!(counts.deshred_errors, 0);
    }

    #[tokio::test]
    async fn every_transaction_in_the_entry_is_reported() {
        let (seen, _) = run(shreds_for((1..=4).map(transaction).collect())).await;
        for seed in 1..=4u8 {
            let signature = Signature::from([seed; 64]).to_string();
            assert!(seen.iter().any(|tx| tx.signature == signature), "missing seed {seed}");
        }
    }

    #[tokio::test]
    async fn arrival_is_the_packet_time_not_the_time_we_finished_decoding() {
        // Charging the feed for our own decoding would flatter every other lane
        // in the race by exactly the cost of this pipeline.
        let shreds = shreds_for(vec![transaction(3)]);
        let mut pipeline = ShredPipeline::new(ShredsUdpConfig::defaults());
        let mut stamps = Vec::new();
        for shred in &shreds {
            let packet = packet(shred.payload().to_vec());
            let sent_at = packet.at;
            for tx in pipeline.on_packet(&packet).await {
                stamps.push((tx.at, sent_at));
            }
        }
        assert!(!stamps.is_empty(), "nothing decoded");
        for (reported, packet_at) in stamps {
            assert_eq!(reported, packet_at);
        }
    }

    #[tokio::test]
    async fn junk_packets_are_counted_and_dropped() {
        let mut pipeline = ShredPipeline::new(ShredsUdpConfig::defaults());
        assert!(pipeline.on_packet(&packet(vec![0u8; 64])).await.is_empty());
        assert!(pipeline.on_packet(&packet(Vec::new())).await.is_empty());
        assert_eq!(pipeline.counts().packets, 2);
        assert_eq!(pipeline.counts().transactions, 0);
    }
}

#[cfg(test)]
mod panic_tests {
    use std::net::SocketAddr;
    use std::time::Instant;

    use super::*;

    fn packet(payload: Vec<u8>) -> CapturedPacket {
        CapturedPacket {
            payload,
            at: Instant::now(),
            from: "10.0.0.1:7733".parse::<SocketAddr>().unwrap(),
            dst_port: 7733,
        }
    }

    #[tokio::test]
    async fn a_stream_of_hostile_bytes_never_takes_the_process_down() {
        // Every length and shape the wire can produce, including the ones that
        // walk a decoder off the end of a buffer. The terminal has to survive
        // all of it: a panic here ends the stream, live.
        let mut pipeline = ShredPipeline::new(ShredsUdpConfig::defaults());
        for len in [0usize, 1, 2, 63, 64, 1203, 1228, 1280, 65_000] {
            for fill in [0x00u8, 0xFF, 0xA5] {
                let seen = pipeline.on_packet(&packet(vec![fill; len])).await;
                assert!(seen.is_empty(), "junk must not decode to a transaction");
            }
        }
        assert_eq!(pipeline.counts().transactions, 0);
    }
}
