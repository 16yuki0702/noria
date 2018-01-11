use buf_redux::BufWriter;
use buf_redux::strategy::WhenFull;

use serde_json;

use std::fs;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::mem;
use std::path::PathBuf;
use std::time;
use std::collections::HashMap;
use std::net::SocketAddr;

use debug::DebugEventType;
use domain;
use prelude::*;
use transactions;
use channel::TcpSender;

/// Indicates to what degree updates should be persisted.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum DurabilityMode {
    /// Don't do any durability
    MemoryOnly,
    /// Delete any log files on exit. Useful mainly for tests.
    DeleteOnExit,
    /// Persist updates to disk, and don't delete them later.
    Permanent,
}

/// Parameters to control the operation of GroupCommitQueue.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Parameters {
    /// Number of elements to buffer before flushing.
    pub queue_capacity: usize,
    /// Amount of time to wait before flushing despite not reaching `queue_capacity`.
    pub flush_timeout: time::Duration,
    /// Whether the output files should be deleted when the GroupCommitQueue is dropped.
    pub mode: DurabilityMode,
    /// Filename prefix for persistent log entries.
    pub log_prefix: String,
}

impl Default for Parameters {
    fn default() -> Self {
        Self {
            queue_capacity: 256,
            flush_timeout: time::Duration::from_millis(1),
            mode: DurabilityMode::MemoryOnly,
            log_prefix: String::from("soup"),
        }
    }
}

impl Parameters {
    /// Parameters to control the persistence mode, and parameters related to persistence.
    ///
    /// Three modes are available:
    ///
    ///  1. `DurabilityMode::Permanent`: all writes to base nodes should be written to disk.
    ///  2. `DurabilityMode::DeleteOnExit`: all writes are written to disk, but the log is
    ///     deleted once the `Blender` is dropped. Useful for tests.
    ///  3. `DurabilityMode::MemoryOnly`: no writes to disk, store all writes in memory.
    ///     Useful for baseline numbers.
    ///
    /// `queue_capacity` indicates the number of packets that should be buffered until
    /// flushing, and `flush_timeout` indicates the length of time to wait before flushing
    /// anyway.
    pub fn new(
        mode: DurabilityMode,
        queue_capacity: usize,
        flush_timeout: time::Duration,
        log_prefix: Option<String>,
    ) -> Self {
        Self {
            queue_capacity,
            flush_timeout,
            mode,
            log_prefix: log_prefix.unwrap_or(String::from("soup")),
        }
    }

    /// The path that would be used for the given domain/shard pair's logs.
    pub fn log_path(
        &self,
        node: &LocalNodeIndex,
        domain_index: domain::Index,
        domain_shard: usize,
    ) -> PathBuf {
        let filename = format!(
            "{}-log-{}_{}-{}.json",
            self.log_prefix,
            domain_index.index(),
            domain_shard,
            node.id()
        );

        PathBuf::from(&filename)
    }
}

pub struct GroupCommitQueueSet {
    /// Packets that are queued to be persisted.
    pending_packets: Map<Vec<Box<Packet>>>,

    /// Time when the first packet was inserted into pending_packets, or none if pending_packets is
    /// empty. A flush should occur on or before wait_start + timeout.
    wait_start: Map<time::Instant>,

    /// Name of, and handle to the files that packets should be persisted to.
    files: Map<(PathBuf, BufWriter<File, WhenFull>)>,

    transaction_reply_txs: HashMap<SocketAddr, TcpSender<Result<i64, ()>>>,

    domain_index: domain::Index,
    domain_shard: usize,

    params: Parameters,
}

impl GroupCommitQueueSet {
    /// Create a new `GroupCommitQueue`.
    pub fn new(domain_index: domain::Index, domain_shard: usize, params: &Parameters) -> Self {
        assert!(params.queue_capacity > 0);

        Self {
            pending_packets: Map::default(),
            wait_start: Map::default(),
            files: Map::default(),

            domain_index,
            domain_shard,
            params: params.clone(),
            transaction_reply_txs: HashMap::new(),
        }
    }

    fn get_or_create_file(&self, node: &LocalNodeIndex) -> (PathBuf, BufWriter<File, WhenFull>) {
        let path = self.params
            .log_path(node, self.domain_index, self.domain_shard);
        let file = OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)
            .unwrap();

        (
            path,
            BufWriter::with_capacity(self.params.queue_capacity * 1024, file),
        )
    }

    /// Returns None for packet types not relevant to persistence, and the node the packet was
    /// directed to otherwise.
    fn packet_destination(p: &Box<Packet>) -> Option<LocalNodeIndex> {
        match **p {
            Packet::VtMessage { ref link, .. } => Some(link.dst),
            _ => None,
        }
    }

    /// Returns whether the given packet should be persisted.
    pub fn should_append(&self, p: &Box<Packet>, nodes: &DomainNodes) -> bool {
        match Self::packet_destination(p) {
            Some(n) => {
                let node = &nodes[&n].borrow();
                node.is_internal() && node.get_base().is_some()
            }
            None => false,
        }
    }

    /// Find the first queue that has timed out waiting for more packets, and flush it to disk.
    pub fn flush_if_necessary(
        &mut self,
        nodes: &DomainNodes,
        transaction_state: &mut transactions::DomainState,
    ) -> Option<Box<Packet>> {
        let mut needs_flush = None;
        for (node, wait_start) in self.wait_start.iter() {
            if wait_start.elapsed() >= self.params.flush_timeout {
                needs_flush = Some(node);
                break;
            }
        }

        needs_flush.and_then(|node| self.flush_internal(&node, nodes, transaction_state))
    }

    /// Flush any pending packets for node to disk (if applicable), and return a merged packet.
    fn flush_internal(
        &mut self,
        node: &LocalNodeIndex,
        nodes: &DomainNodes,
        transaction_state: &mut transactions::DomainState,
    ) -> Option<Box<Packet>> {
        match self.params.mode {
            DurabilityMode::DeleteOnExit | DurabilityMode::Permanent => {
                if !self.files.contains_key(node) {
                    let file = self.get_or_create_file(node);
                    self.files.insert(node.clone(), file);
                }

                let mut file = &mut self.files[node].1;
                {
                    let data_to_flush: Vec<_> = self.pending_packets[&node]
                        .iter()
                        .map(|p| match **p {
                            Packet::VtMessage { ref data, .. } => data,
                            _ => unreachable!(),
                        })
                        .collect();
                    serde_json::to_writer(&mut file, &data_to_flush).unwrap();
                    // Separate log flushes with a newline so that the
                    // file can be easily parsed later on:
                    writeln!(&mut file, "").unwrap();
                }

                file.flush().unwrap();
                file.get_mut().sync_data().unwrap();
            }
            DurabilityMode::MemoryOnly => {}
        }

        self.wait_start.remove(node);
        Self::merge_packets(
            mem::replace(&mut self.pending_packets[node], Vec::new()),
            nodes,
            transaction_state,
        )
    }

    /// Add a new packet to be persisted, and if this triggered a flush return an iterator over the
    /// packets that were written.
    pub fn append<'a>(
        &mut self,
        p: Box<Packet>,
        nodes: &DomainNodes,
        transaction_state: &mut transactions::DomainState,
    ) -> Option<Box<Packet>> {
        let node = Self::packet_destination(&p).unwrap();
        if !self.pending_packets.contains_key(&node) {
            self.pending_packets
                .insert(node.clone(), Vec::with_capacity(self.params.queue_capacity));
        }

        self.pending_packets[&node].push(p);
        if self.pending_packets[&node].len() >= self.params.queue_capacity {
            return self.flush_internal(&node, nodes, transaction_state);
        } else if !self.wait_start.contains_key(&node) {
            self.wait_start.insert(node, time::Instant::now());
        }
        None
    }

    /// Returns how long until a flush should occur.
    pub fn duration_until_flush(&self) -> Option<time::Duration> {
        self.wait_start
            .values()
            .map(|i| {
                self.params
                    .flush_timeout
                    .checked_sub(i.elapsed())
                    .unwrap_or(time::Duration::from_millis(0))
            })
            .min()
    }

    /// Merge the contents of packets into a single packet.
    fn merge_packets(
        packets: Vec<Box<Packet>>,
        nodes: &DomainNodes,
        transaction_state: &mut transactions::DomainState,
    ) -> Option<Box<Packet>> {
        if packets.is_empty() {
            return None;
        }

        let base = if let box Packet::VtMessage { ref link, .. } = packets[0] {
            nodes[&link.dst].borrow().global_addr()
        } else {
            unreachable!()
        };

        let assignment = transaction_state.assign_time(base);

        let mut packets = packets.into_iter().peekable();
        let merged_link = match **packets.peek().as_mut().unwrap() {
            box Packet::VtMessage { ref link, .. } => link.clone(),
            _ => unreachable!(),
        };
        let mut merged_tracer: Tracer = None;
        let merged_data = packets.fold(Records::default(), |mut acc, p| {
            match (p,) {
                (box Packet::VtMessage {
                    ref link,
                    ref mut data,
                    ref mut tracer,
                    ..
                },) => {
                    assert_eq!(merged_link, *link);
                    acc.append(data);

                    match (&merged_tracer, tracer) {
                        (&Some((mtag, _)), &mut Some((tag, Some(ref sender)))) => {
                            sender
                                .send(DebugEvent {
                                    instant: time::Instant::now(),
                                    event: DebugEventType::PacketEvent(
                                        PacketEvent::Merged(mtag),
                                        tag,
                                    ),
                                })
                                .unwrap();
                        }
                        (_, tracer @ &mut Some(_)) => {
                            merged_tracer = tracer.take();
                        }
                        _ => {}
                    }
                }
                _ => unreachable!(),
            }
            acc
        });

        Some(Box::new(Packet::VtMessage {
            link: merged_link,
            data: merged_data,
            tracer: merged_tracer,
            state: TransactionState::VtCommitted {
                at: (assignment.time, assignment.source),
                prev: assignment.prev,
                base,
            },
        }))
    }
}

impl Drop for GroupCommitQueueSet {
    fn drop(&mut self) {
        if let DurabilityMode::DeleteOnExit = self.params.mode {
            for &(ref filename, _) in self.files.values() {
                fs::remove_file(filename).unwrap();
            }
        }
    }
}
