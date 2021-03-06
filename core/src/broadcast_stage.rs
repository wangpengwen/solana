//! A stage to broadcast data from a leader node to validators
use self::broadcast_fake_shreds_run::BroadcastFakeShredsRun;
use self::fail_entry_verification_broadcast_run::FailEntryVerificationBroadcastRun;
use self::standard_broadcast_run::StandardBroadcastRun;
use crate::cluster_info::{ClusterInfo, ClusterInfoError};
use crate::poh_recorder::WorkingBankEntry;
use crate::result::{Error, Result};
use solana_ledger::blockstore::Blockstore;
use solana_ledger::shred::Shred;
use solana_ledger::staking_utils;
use solana_metrics::{inc_new_counter_error, inc_new_counter_info};
use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvError, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, Builder, JoinHandle};
use std::time::Instant;

pub const NUM_INSERT_THREADS: usize = 2;

mod broadcast_fake_shreds_run;
pub(crate) mod broadcast_utils;
mod fail_entry_verification_broadcast_run;
mod standard_broadcast_run;

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum BroadcastStageReturnType {
    ChannelDisconnected,
}

#[derive(PartialEq, Clone, Debug)]
pub enum BroadcastStageType {
    Standard,
    FailEntryVerification,
    BroadcastFakeShreds,
}

impl BroadcastStageType {
    pub fn new_broadcast_stage(
        &self,
        sock: Vec<UdpSocket>,
        cluster_info: Arc<RwLock<ClusterInfo>>,
        receiver: Receiver<WorkingBankEntry>,
        exit_sender: &Arc<AtomicBool>,
        blockstore: &Arc<Blockstore>,
        shred_version: u16,
    ) -> BroadcastStage {
        let keypair = cluster_info.read().unwrap().keypair.clone();
        match self {
            BroadcastStageType::Standard => BroadcastStage::new(
                sock,
                cluster_info,
                receiver,
                exit_sender,
                blockstore,
                StandardBroadcastRun::new(keypair, shred_version),
            ),

            BroadcastStageType::FailEntryVerification => BroadcastStage::new(
                sock,
                cluster_info,
                receiver,
                exit_sender,
                blockstore,
                FailEntryVerificationBroadcastRun::new(keypair, shred_version),
            ),

            BroadcastStageType::BroadcastFakeShreds => BroadcastStage::new(
                sock,
                cluster_info,
                receiver,
                exit_sender,
                blockstore,
                BroadcastFakeShredsRun::new(keypair, 0, shred_version),
            ),
        }
    }
}

type TransmitShreds = (Option<Arc<HashMap<Pubkey, u64>>>, Arc<Vec<Shred>>);
trait BroadcastRun {
    fn run(
        &mut self,
        blockstore: &Arc<Blockstore>,
        receiver: &Receiver<WorkingBankEntry>,
        socket_sender: &Sender<TransmitShreds>,
        blockstore_sender: &Sender<Arc<Vec<Shred>>>,
    ) -> Result<()>;
    fn transmit(
        &self,
        receiver: &Arc<Mutex<Receiver<TransmitShreds>>>,
        cluster_info: &Arc<RwLock<ClusterInfo>>,
        sock: &UdpSocket,
    ) -> Result<()>;
    fn record(
        &self,
        receiver: &Arc<Mutex<Receiver<Arc<Vec<Shred>>>>>,
        blockstore: &Arc<Blockstore>,
    ) -> Result<()>;
}

// Implement a destructor for the BroadcastStage thread to signal it exited
// even on panics
struct Finalizer {
    exit_sender: Arc<AtomicBool>,
}

impl Finalizer {
    fn new(exit_sender: Arc<AtomicBool>) -> Self {
        Finalizer { exit_sender }
    }
}
// Implement a destructor for Finalizer.
impl Drop for Finalizer {
    fn drop(&mut self) {
        self.exit_sender.clone().store(true, Ordering::Relaxed);
    }
}

pub struct BroadcastStage {
    thread_hdls: Vec<JoinHandle<BroadcastStageReturnType>>,
}

impl BroadcastStage {
    #[allow(clippy::too_many_arguments)]
    fn run(
        blockstore: &Arc<Blockstore>,
        receiver: &Receiver<WorkingBankEntry>,
        socket_sender: &Sender<TransmitShreds>,
        blockstore_sender: &Sender<Arc<Vec<Shred>>>,
        mut broadcast_stage_run: impl BroadcastRun,
    ) -> BroadcastStageReturnType {
        loop {
            let res =
                broadcast_stage_run.run(blockstore, receiver, socket_sender, blockstore_sender);
            let res = Self::handle_error(res);
            if let Some(res) = res {
                return res;
            }
        }
    }
    fn handle_error(r: Result<()>) -> Option<BroadcastStageReturnType> {
        if let Err(e) = r {
            match e {
                Error::RecvTimeoutError(RecvTimeoutError::Disconnected)
                | Error::SendError
                | Error::RecvError(RecvError) => {
                    return Some(BroadcastStageReturnType::ChannelDisconnected);
                }
                Error::RecvTimeoutError(RecvTimeoutError::Timeout) => (),
                Error::ClusterInfoError(ClusterInfoError::NoPeers) => (), // TODO: Why are the unit-tests throwing hundreds of these?
                _ => {
                    inc_new_counter_error!("streamer-broadcaster-error", 1, 1);
                    error!("broadcaster error: {:?}", e);
                }
            }
        }
        None
    }

    /// Service to broadcast messages from the leader to layer 1 nodes.
    /// See `cluster_info` for network layer definitions.
    /// # Arguments
    /// * `sock` - Socket to send from.
    /// * `exit` - Boolean to signal system exit.
    /// * `cluster_info` - ClusterInfo structure
    /// * `window` - Cache of Shreds that we have broadcast
    /// * `receiver` - Receive channel for Shreds to be retransmitted to all the layer 1 nodes.
    /// * `exit_sender` - Set to true when this service exits, allows rest of Tpu to exit cleanly.
    /// Otherwise, when a Tpu closes, it only closes the stages that come after it. The stages
    /// that come before could be blocked on a receive, and never notice that they need to
    /// exit. Now, if any stage of the Tpu closes, it will lead to closing the WriteStage (b/c
    /// WriteStage is the last stage in the pipeline), which will then close Broadcast service,
    /// which will then close FetchStage in the Tpu, and then the rest of the Tpu,
    /// completing the cycle.
    #[allow(clippy::too_many_arguments)]
    fn new(
        socks: Vec<UdpSocket>,
        cluster_info: Arc<RwLock<ClusterInfo>>,
        receiver: Receiver<WorkingBankEntry>,
        exit_sender: &Arc<AtomicBool>,
        blockstore: &Arc<Blockstore>,
        broadcast_stage_run: impl BroadcastRun + Send + 'static + Clone,
    ) -> Self {
        let btree = blockstore.clone();
        let exit = exit_sender.clone();
        let (socket_sender, socket_receiver) = channel();
        let (blockstore_sender, blockstore_receiver) = channel();
        let bs_run = broadcast_stage_run.clone();
        let thread_hdl = Builder::new()
            .name("solana-broadcaster".to_string())
            .spawn(move || {
                let _finalizer = Finalizer::new(exit);
                Self::run(
                    &btree,
                    &receiver,
                    &socket_sender,
                    &blockstore_sender,
                    bs_run,
                )
            })
            .unwrap();
        let mut thread_hdls = vec![thread_hdl];
        let socket_receiver = Arc::new(Mutex::new(socket_receiver));
        for sock in socks.into_iter() {
            let socket_receiver = socket_receiver.clone();
            let bs_transmit = broadcast_stage_run.clone();
            let cluster_info = cluster_info.clone();
            let t = Builder::new()
                .name("solana-broadcaster-transmit".to_string())
                .spawn(move || loop {
                    let res = bs_transmit.transmit(&socket_receiver, &cluster_info, &sock);
                    let res = Self::handle_error(res);
                    if let Some(res) = res {
                        return res;
                    }
                })
                .unwrap();
            thread_hdls.push(t);
        }
        let blockstore_receiver = Arc::new(Mutex::new(blockstore_receiver));
        for _ in 0..NUM_INSERT_THREADS {
            let blockstore_receiver = blockstore_receiver.clone();
            let bs_record = broadcast_stage_run.clone();
            let btree = blockstore.clone();
            let t = Builder::new()
                .name("solana-broadcaster-record".to_string())
                .spawn(move || loop {
                    let res = bs_record.record(&blockstore_receiver, &btree);
                    let res = Self::handle_error(res);
                    if let Some(res) = res {
                        return res;
                    }
                })
                .unwrap();
            thread_hdls.push(t);
        }

        Self { thread_hdls }
    }

    pub fn join(self) -> thread::Result<BroadcastStageReturnType> {
        for thread_hdl in self.thread_hdls.into_iter() {
            let _ = thread_hdl.join();
        }
        Ok(BroadcastStageReturnType::ChannelDisconnected)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::cluster_info::{ClusterInfo, Node};
    use crate::genesis_utils::{create_genesis_config, GenesisConfigInfo};
    use solana_ledger::entry::create_ticks;
    use solana_ledger::{blockstore::Blockstore, get_tmp_ledger_path};
    use solana_runtime::bank::Bank;
    use solana_sdk::hash::Hash;
    use solana_sdk::pubkey::Pubkey;
    use solana_sdk::signature::{Keypair, KeypairUtil};
    use std::path::Path;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc::channel;
    use std::sync::{Arc, RwLock};
    use std::thread::sleep;
    use std::time::Duration;

    struct MockBroadcastStage {
        blockstore: Arc<Blockstore>,
        broadcast_service: BroadcastStage,
        bank: Arc<Bank>,
    }

    fn setup_dummy_broadcast_service(
        leader_pubkey: &Pubkey,
        ledger_path: &Path,
        entry_receiver: Receiver<WorkingBankEntry>,
    ) -> MockBroadcastStage {
        // Make the database ledger
        let blockstore = Arc::new(Blockstore::open(ledger_path).unwrap());

        // Make the leader node and scheduler
        let leader_info = Node::new_localhost_with_pubkey(leader_pubkey);

        // Make a node to broadcast to
        let buddy_keypair = Keypair::new();
        let broadcast_buddy = Node::new_localhost_with_pubkey(&buddy_keypair.pubkey());

        // Fill the cluster_info with the buddy's info
        let mut cluster_info = ClusterInfo::new_with_invalid_keypair(leader_info.info.clone());
        cluster_info.insert_info(broadcast_buddy.info);
        let cluster_info = Arc::new(RwLock::new(cluster_info));

        let exit_sender = Arc::new(AtomicBool::new(false));

        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(10_000);
        let bank = Arc::new(Bank::new(&genesis_config));

        let leader_keypair = cluster_info.read().unwrap().keypair.clone();
        // Start up the broadcast stage
        let broadcast_service = BroadcastStage::new(
            leader_info.sockets.broadcast,
            cluster_info,
            entry_receiver,
            &exit_sender,
            &blockstore,
            StandardBroadcastRun::new(leader_keypair, 0),
        );

        MockBroadcastStage {
            blockstore,
            broadcast_service,
            bank,
        }
    }

    #[test]
    fn test_broadcast_ledger() {
        solana_logger::setup();
        let ledger_path = get_tmp_ledger_path!();

        {
            // Create the leader scheduler
            let leader_keypair = Keypair::new();

            let (entry_sender, entry_receiver) = channel();
            let broadcast_service = setup_dummy_broadcast_service(
                &leader_keypair.pubkey(),
                &ledger_path,
                entry_receiver,
            );
            let start_tick_height;
            let max_tick_height;
            let ticks_per_slot;
            let slot;
            {
                let bank = broadcast_service.bank.clone();
                start_tick_height = bank.tick_height();
                max_tick_height = bank.max_tick_height();
                ticks_per_slot = bank.ticks_per_slot();
                slot = bank.slot();
                let ticks = create_ticks(max_tick_height - start_tick_height, 0, Hash::default());
                for (i, tick) in ticks.into_iter().enumerate() {
                    entry_sender
                        .send((bank.clone(), (tick, i as u64 + 1)))
                        .expect("Expect successful send to broadcast service");
                }
            }
            sleep(Duration::from_millis(2000));

            trace!(
                "[broadcast_ledger] max_tick_height: {}, start_tick_height: {}, ticks_per_slot: {}",
                max_tick_height,
                start_tick_height,
                ticks_per_slot,
            );

            let blockstore = broadcast_service.blockstore;
            let (entries, _, _) = blockstore
                .get_slot_entries_with_shred_info(slot, 0)
                .expect("Expect entries to be present");
            assert_eq!(entries.len(), max_tick_height as usize);

            drop(entry_sender);
            broadcast_service
                .broadcast_service
                .join()
                .expect("Expect successful join of broadcast service");
        }

        Blockstore::destroy(&ledger_path).expect("Expected successful database destruction");
    }
}
