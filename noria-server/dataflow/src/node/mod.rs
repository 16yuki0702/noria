use domain;
use ops;
use petgraph;
use prelude::*;
use fnv::FnvHashMap;
use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::{Deref, DerefMut};

mod process;
pub use self::process::materialize;

pub mod special;
pub use self::special::StreamUpdate;

mod ntype;
pub use self::ntype::NodeType;

mod replica;
pub use self::replica::ReplicaType;

mod debug;

#[derive(Clone, Serialize, Deserialize)]
pub struct Node {
    name: String,
    index: Option<IndexPair>,
    domain: Option<domain::Index>,

    fields: Vec<String>,
    children: Vec<LocalNodeIndex>,
    inner: NodeType,
    taken: bool,

    sharded_by: Sharding,
    replica: Option<ReplicaType>,
    /// The last packet received and processed from each parent
    pub last_packet_received: HashMap<NodeIndex, usize>,
    /// The next packet to send to each child, where the key DNE if waiting for a ResumeAt
    pub next_packet_to_send: HashMap<NodeIndex, usize>,
    /// The packet buffer with the payload and list of to-nodes, starts at 1
    buffer: Vec<(Box<Packet>, HashSet<NodeIndex>)>,
}

// constructors
impl Node {
    pub fn new<S1, FS, S2, NT>(name: S1, fields: FS, inner: NT) -> Node
    where
        S1: ToString,
        S2: ToString,
        FS: IntoIterator<Item = S2>,
        NT: Into<NodeType>,
    {
        Node {
            name: name.to_string(),
            index: None,
            domain: None,

            fields: fields.into_iter().map(|s| s.to_string()).collect(),
            children: Vec::new(),
            inner: inner.into(),
            taken: false,

            sharded_by: Sharding::None,
            replica: None,
            last_packet_received: HashMap::new(),
            next_packet_to_send: HashMap::new(),
            buffer: Vec::new(),
        }
    }

    fn new_with_replica<S1, FS, S2, NT>(
        name: S1,
        fields: FS,
        inner: NT,
        replica: &Option<ReplicaType>,
    ) -> Node
    where
        S1: ToString,
        S2: ToString,
        FS: IntoIterator<Item = S2>,
        NT: Into<NodeType>,
    {
        let mut n = Self::new(name, fields, inner);
        n.replica = replica.clone();
        n
    }

    pub fn mirror<NT: Into<NodeType>>(&self, n: NT) -> Node {
        Self::new_with_replica(&*self.name, &self.fields, n, &self.replica)
    }

    pub fn named_mirror<NT: Into<NodeType>>(&self, n: NT, name: String) -> Node {
        Self::new_with_replica(name, &self.fields, n, &self.replica)
    }
}

#[must_use]
pub struct DanglingDomainNode(Node);

impl DanglingDomainNode {
    pub fn finalize(self, graph: &Graph) -> Node {
        let mut n = self.0;
        let ni = n.global_addr();
        let dm = n.domain();
        n.children = graph
            .neighbors_directed(ni, petgraph::EdgeDirection::Outgoing)
            .filter(|&c| graph[c].domain() == dm)
            .map(|ni| graph[ni].local_addr())
            .collect();

        let all_children = graph
            .neighbors_directed(ni, petgraph::EdgeDirection::Outgoing)
            .map(|ni| graph[ni].global_addr());
        for ni in all_children {
            n.next_packet_to_send.insert(ni, 1);
        }

        n
    }
}

// events
impl Node {
    pub fn take(&mut self) -> DanglingDomainNode {
        assert!(!self.taken);
        assert!(
            (!self.is_internal() && !self.is_base()) || self.domain.is_some(),
            "tried to take unassigned node"
        );

        let inner = self.inner.take();
        let mut n = self.mirror(inner);
        n.index = self.index;
        n.domain = self.domain;
        self.taken = true;

        DanglingDomainNode(n)
    }

    pub fn remove(&mut self) {
        self.inner = NodeType::Dropped;
    }

    /// Set this node's sharding property.
    pub fn shard_by(&mut self, s: Sharding) {
        self.sharded_by = s;
    }

    pub fn on_commit(&mut self, remap: &HashMap<NodeIndex, IndexPair>) {
        // this is *only* overwritten for these asserts.
        assert!(!self.taken);
        if let NodeType::Internal(ref mut i) = self.inner {
            i.on_commit(self.index.unwrap().as_global(), remap)
        }
    }
}

// derefs
impl Node {
    pub fn with_sharder_mut<F>(&mut self, f: F)
    where
        F: FnOnce(&mut special::Sharder),
    {
        match self.inner {
            NodeType::Sharder(ref mut s) => f(s),
            _ => unreachable!(),
        }
    }

    pub fn with_sharder<'a, F, R>(&'a self, f: F) -> Option<R>
    where
        F: FnOnce(&'a special::Sharder) -> R,
        R: 'a,
    {
        match self.inner {
            NodeType::Sharder(ref s) => Some(f(s)),
            _ => None,
        }
    }

    pub fn with_egress_mut<F>(&mut self, f: F)
    where
        F: FnOnce(&mut special::Egress),
    {
        match self.inner {
            NodeType::Egress(Some(ref mut e)) => f(e),
            _ => unreachable!(),
        }
    }

    pub fn with_reader_mut<'a, F, R>(&'a mut self, f: F) -> Result<R, ()>
    where
        F: FnOnce(&'a mut special::Reader) -> R,
        R: 'a,
    {
        match self.inner {
            NodeType::Reader(ref mut r) => Ok(f(r)),
            _ => Err(()),
        }
    }

    pub fn with_reader<'a, F, R>(&'a self, f: F) -> Result<R, ()>
    where
        F: FnOnce(&'a special::Reader) -> R,
        R: 'a,
    {
        match self.inner {
            NodeType::Reader(ref r) => Ok(f(r)),
            _ => Err(()),
        }
    }

    pub fn suggest_indexes(&self, n: NodeIndex) -> HashMap<NodeIndex, (Vec<usize>, bool)> {
        match self.inner {
            NodeType::Internal(ref i) => i.suggest_indexes(n),
            NodeType::Base(ref b) => b.suggest_indexes(n),
            _ => HashMap::new(),
        }
    }
}

impl Deref for Node {
    type Target = ops::NodeOperator;
    fn deref(&self) -> &Self::Target {
        match self.inner {
            NodeType::Internal(ref i) => i,
            _ => unreachable!(),
        }
    }
}

impl DerefMut for Node {
    fn deref_mut(&mut self) -> &mut Self::Target {
        assert!(!self.taken);
        match self.inner {
            NodeType::Internal(ref mut i) => i,
            _ => unreachable!(),
        }
    }
}

// children
impl Node {
    pub fn children(&self) -> &[LocalNodeIndex] {
        &self.children[..]
    }

    pub fn child(&self, i: usize) -> &LocalNodeIndex {
        &self.children[i]
    }

    pub fn has_children(&self) -> bool {
        !self.children.is_empty()
    }

    pub fn nchildren(&self) -> usize {
        self.children.len()
    }
}

// attributes
impl Node {
    pub fn sharded_by(&self) -> Sharding {
        self.sharded_by
    }

    pub fn add_child(&mut self, child: LocalNodeIndex) {
        self.children.push(child);
    }

    pub fn try_remove_child(&mut self, child: LocalNodeIndex) -> bool {
        for i in 0..self.children.len() {
            if self.children[i] == child {
                self.children.swap_remove(i);
                return true;
            }
        }
        false
    }

    pub fn add_column(&mut self, field: &str) -> usize {
        self.fields.push(field.to_string());
        self.fields.len() - 1
    }

    pub fn name(&self) -> &str {
        &*self.name
    }

    pub fn fields(&self) -> &[String] {
        &self.fields[..]
    }

    pub fn has_domain(&self) -> bool {
        self.domain.is_some()
    }

    pub fn domain(&self) -> domain::Index {
        match self.domain {
            Some(domain) => domain,
            None => {
                unreachable!(
                    "asked for unset domain for {:?} {}",
                    self,
                    self.global_addr().index()
                );
            }
        }
    }

    pub fn local_addr(&self) -> LocalNodeIndex {
        match self.index {
            Some(idx) if idx.has_local() => *idx,
            Some(_) | None => unreachable!("asked for unset addr for {:?}", self),
        }
    }

    pub fn global_addr(&self) -> NodeIndex {
        match self.index {
            Some(ref index) => index.as_global(),
            None => {
                unreachable!("asked for unset index for {:?}", self);
            }
        }
    }

    pub fn get_index(&self) -> &IndexPair {
        self.index.as_ref().unwrap()
    }

    pub fn get_base(&self) -> Option<&special::Base> {
        if let NodeType::Base(ref b) = self.inner {
            Some(b)
        } else {
            None
        }
    }

    pub fn get_base_mut(&mut self) -> Option<&mut special::Base> {
        if let NodeType::Base(ref mut b) = self.inner {
            Some(b)
        } else {
            None
        }
    }

    pub fn is_base(&self) -> bool {
        if let NodeType::Base(..) = self.inner {
            true
        } else {
            false
        }
    }

    pub fn is_localized(&self) -> bool {
        self.index
            .as_ref()
            .map(|idx| idx.has_local())
            .unwrap_or(false)
            && self.domain.is_some()
    }

    pub fn add_to(&mut self, domain: domain::Index) {
        assert_eq!(self.domain, None);
        assert!(!self.is_dropped());
        self.domain = Some(domain);
    }

    pub fn set_finalized_addr(&mut self, addr: IndexPair) {
        self.index = Some(addr);
    }
}

// replication
impl Node {
    pub fn replica_type(&self) -> Option<ReplicaType> {
        self.replica.clone()
    }

    pub fn set_replica_type(&mut self, rt: ReplicaType) {
        self.replica = Some(rt);
    }

    /// Receive a packet, keeping track of the latest packet received from each parent. If the
    /// parent crashes, we can tell the parent's replacement where to resume sending messages.
    pub fn receive_packet(&mut self, m: &Box<Packet>) {
        let (from, label) = match m {
            box Packet::Input { .. } => { return; },  // ignore inputs from clients
            box Packet::Message { id, .. } => (id.from(), id.label()),
            box Packet::ReplayPiece { id, .. } => (id.from(), id.label()),
            _ => unreachable!(),
        };

        println!( "{} RECEIVE #{} from {:?}", self.global_addr().index(), label, from);
        let old_label = self.last_packet_received.insert(from, label);

        // labels are not necessarily sequential, but must be increasing
        assert!(label > old_label.unwrap_or(0));
    }

    /// Stores the packet payload and who the packet is for in the buffer. We only send nodes to
    /// our children. Returns whether we should actually send the packet -- if not a success, we
    /// are probably waiting for a ResumeAt message from that node.
    ///
    /// Note that it's ok for next packet to send to be ahead of the packets that have actually
    /// been sent. Either this information is nulled in anticipation of a ResumeAt message, or
    /// it is lost anyway on crash.
    pub fn send_external_packet(&mut self, m: &Box<Packet>, to: NodeIndex) -> bool {
        assert_eq!(m.get_id().from(), self.global_addr());

        // push the packet payload and target to-nodes to the buffer
        let label = m.get_id().label();
        if label > self.buffer.len() {
            let mut to_nodes = HashSet::new();
            to_nodes.insert(to);
            assert_eq!(label, self.buffer.len() + 1, "outgoing labels increase sequentially");
            self.buffer.push((box m.clone_data(), to_nodes));
        } else {
            self.buffer.get_mut(label - 1).unwrap().1.insert(to);
        }

        // update internal state if we should send the packet
        if let Some(old_label) = self.next_packet_to_send.get(&to) {
            // any skipped packets from [old_label, label) shouldn't have been sent to ni anyway
            for i in *old_label..label {
                assert!(!self.buffer.get(i - 1).unwrap().1.contains(&to));
            }

            println!("{} SEND #{} to {:?}", self.global_addr().index(), label, to);
            self.next_packet_to_send.insert(to, label + 1);
            true
        } else {
            false
        }
    }

    /// Like send_external_packet, except the packet is to a node in the same domain, and is
    /// always successful.
    pub fn send_internal_packet(
        &mut self,
        m: &Box<Packet>,
        to: LocalNodeIndex,
        nodes: &DomainNodes,
    ) {
        assert!(self.send_external_packet(m, nodes[to].borrow().global_addr()));
    }

    /// The id to be assigned to the next outgoing packet.
    pub fn next_packet_id(&self) -> PacketId {
        let me = self.global_addr();
        let label = self.buffer.len() + 1;
        PacketId::new(label, me)
    }

    /// Resume sending messages to this node at the label after getting that node up to date.
    pub fn resume_at(
        &mut self,
        node: NodeIndex,
        label: usize,
        on_shard: Option<usize>,
        output: &mut FnvHashMap<ReplicaAddr, VecDeque<Box<Packet>>>,
    ) {
        match self.inner {
            NodeType::Egress(Some(ref mut e)) => {
                let max_label = self.buffer.len() + 1;
                let to_nodes = {
                    let mut hs = HashSet::new();
                    hs.insert(node);
                    hs
                };
                for i in label..max_label {
                    // TODO(ygina): ignore to nodes, which i think only matter when a packet is sent
                    // as part of a replay from a node with multiple children
                    // let (m, to_nodes) = &self.buffer[i - 1];
                    // if to_nodes.contains(&node) {
                    //     packets.push(box m.clone_data());
                    // }
                    let (m, _) = &self.buffer[i - 1];
                    e.process(
                        &mut Some(box m.clone_data()),
                        on_shard.unwrap_or(0),
                        output,
                        &to_nodes,
                    );
                }
                self.next_packet_to_send.insert(node, max_label);
            },
            _ => unreachable!(),
        };
    }

    /// Replace an incoming connection from `old` with `new`.
    /// Returns the label of the next message expected from the new connection.
    pub fn new_incoming(&mut self, old: NodeIndex, new: NodeIndex) -> usize {
        let label = self.last_packet_received.remove(&old).unwrap_or(0);
        self.last_packet_received.insert(new, label);
        label + 1
    }
}

// is this or that?
impl Node {
    pub fn is_source(&self) -> bool {
        if let NodeType::Source { .. } = self.inner {
            true
        } else {
            false
        }
    }

    pub fn is_dropped(&self) -> bool {
        if let NodeType::Dropped = self.inner {
            true
        } else {
            false
        }
    }

    pub fn is_egress(&self) -> bool {
        if let NodeType::Egress { .. } = self.inner {
            true
        } else {
            false
        }
    }

    pub fn is_sharder(&self) -> bool {
        if let NodeType::Sharder { .. } = self.inner {
            true
        } else {
            false
        }
    }

    pub fn is_reader(&self) -> bool {
        if let NodeType::Reader { .. } = self.inner {
            true
        } else {
            false
        }
    }

    pub fn is_ingress(&self) -> bool {
        if let NodeType::Ingress = self.inner {
            true
        } else {
            false
        }
    }

    pub fn is_internal(&self) -> bool {
        if let NodeType::Internal(..) = self.inner {
            true
        } else {
            false
        }
    }

    pub fn is_sender(&self) -> bool {
        match self.inner {
            NodeType::Egress { .. } | NodeType::Sharder(..) => true,
            _ => false,
        }
    }

    pub fn is_shard_merger(&self) -> bool {
        if let NodeType::Internal(NodeOperator::Union(ref u)) = self.inner {
            u.is_shard_merger()
        } else {
            false
        }
    }

    /// A node is considered to be an output node if changes to its state are visible outside of
    /// its domain.
    pub fn is_output(&self) -> bool {
        match self.inner {
            NodeType::Egress { .. } | NodeType::Reader(..) | NodeType::Sharder(..) => true,
            _ => false,
        }
    }
}
