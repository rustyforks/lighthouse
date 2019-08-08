use crate::config::*;
use crate::discovery::Discovery;
use crate::rpc::{RPCEvent, RPCMessage, RPC};
use crate::{error, NetworkConfig};
use crate::{Topic, TopicHash};
use futures::prelude::*;
use libp2p::{
    core::identity::Keypair,
    discv5::Discv5Event,
    gossipsub::{Gossipsub, GossipsubEvent},
    identify::{Identify, IdentifyEvent},
    ping::{Ping, PingConfig, PingEvent},
    swarm::{NetworkBehaviourAction, NetworkBehaviourEventProcess},
    tokio_io::{AsyncRead, AsyncWrite},
    NetworkBehaviour, PeerId,
};
use slog::{debug, o, trace};
use std::num::NonZeroU32;
use std::time::Duration;

const MAX_IDENTIFY_ADDRESSES: usize = 20;

/// Builds the network behaviour that manages the core protocols of eth2.
/// This core behaviour is managed by `Behaviour` which adds peer management to all core
/// behaviours.
#[derive(NetworkBehaviour)]
#[behaviour(out_event = "BehaviourEvent", poll_method = "poll")]
pub struct Behaviour<TSubstream: AsyncRead + AsyncWrite> {
    /// The routing pub-sub mechanism for eth2.
    gossipsub: Gossipsub<TSubstream>,
    /// The Eth2 RPC specified in the wire-0 protocol.
    eth2_rpc: RPC<TSubstream>,
    /// Keep regular connection to peers and disconnect if absent.
    // TODO: Remove Libp2p ping in favour of discv5 ping.
    ping: Ping<TSubstream>,
    // TODO: Using id for initial interop. This will be removed by mainnet.
    /// Provides IP addresses and peer information.
    identify: Identify<TSubstream>,
    /// Discovery behaviour.
    discovery: Discovery<TSubstream>,
    #[behaviour(ignore)]
    /// The events generated by this behaviour to be consumed in the swarm poll.
    events: Vec<BehaviourEvent>,
    /// Logger for behaviour actions.
    #[behaviour(ignore)]
    log: slog::Logger,
}

impl<TSubstream: AsyncRead + AsyncWrite> Behaviour<TSubstream> {
    pub fn new(
        local_key: &Keypair,
        net_conf: &NetworkConfig,
        log: &slog::Logger,
    ) -> error::Result<Self> {
        let local_peer_id = local_key.public().clone().into_peer_id();
        let behaviour_log = log.new(o!());

        let ping_config = PingConfig::new()
            .with_timeout(Duration::from_secs(30))
            .with_interval(Duration::from_secs(20))
            .with_max_failures(NonZeroU32::new(2).expect("2 != 0"))
            .with_keep_alive(false);

        let identify = Identify::new(
            "lighthouse/libp2p".into(),
            version::version(),
            local_key.public(),
        );

        Ok(Behaviour {
            eth2_rpc: RPC::new(log),
            gossipsub: Gossipsub::new(local_peer_id.clone(), net_conf.gs_config.clone()),
            discovery: Discovery::new(local_key, net_conf, log)?,
            ping: Ping::new(ping_config),
            identify,
            events: Vec::new(),
            log: behaviour_log,
        })
    }
}

// Implement the NetworkBehaviourEventProcess trait so that we can derive NetworkBehaviour for Behaviour
impl<TSubstream: AsyncRead + AsyncWrite> NetworkBehaviourEventProcess<GossipsubEvent>
    for Behaviour<TSubstream>
{
    fn inject_event(&mut self, event: GossipsubEvent) {
        match event {
            GossipsubEvent::Message(gs_msg) => {
                trace!(self.log, "Received GossipEvent"; "msg" => format!("{:?}", gs_msg));

                let msg = PubsubMessage::from_topics(&gs_msg.topics, gs_msg.data);

                self.events.push(BehaviourEvent::GossipMessage {
                    source: gs_msg.source,
                    topics: gs_msg.topics,
                    message: msg,
                });
            }
            GossipsubEvent::Subscribed { .. } => {}
            GossipsubEvent::Unsubscribed { .. } => {}
        }
    }
}

impl<TSubstream: AsyncRead + AsyncWrite> NetworkBehaviourEventProcess<RPCMessage>
    for Behaviour<TSubstream>
{
    fn inject_event(&mut self, event: RPCMessage) {
        match event {
            RPCMessage::PeerDialed(peer_id) => {
                self.events.push(BehaviourEvent::PeerDialed(peer_id))
            }
            RPCMessage::PeerDisconnected(peer_id) => {
                self.events.push(BehaviourEvent::PeerDisconnected(peer_id))
            }
            RPCMessage::RPC(peer_id, rpc_event) => {
                self.events.push(BehaviourEvent::RPC(peer_id, rpc_event))
            }
        }
    }
}

impl<TSubstream: AsyncRead + AsyncWrite> NetworkBehaviourEventProcess<PingEvent>
    for Behaviour<TSubstream>
{
    fn inject_event(&mut self, _event: PingEvent) {
        // not interested in ping responses at the moment.
    }
}

impl<TSubstream: AsyncRead + AsyncWrite> Behaviour<TSubstream> {
    /// Consumes the events list when polled.
    fn poll<TBehaviourIn>(
        &mut self,
    ) -> Async<NetworkBehaviourAction<TBehaviourIn, BehaviourEvent>> {
        if !self.events.is_empty() {
            return Async::Ready(NetworkBehaviourAction::GenerateEvent(self.events.remove(0)));
        }

        Async::NotReady
    }
}

impl<TSubstream: AsyncRead + AsyncWrite> NetworkBehaviourEventProcess<IdentifyEvent>
    for Behaviour<TSubstream>
{
    fn inject_event(&mut self, event: IdentifyEvent) {
        match event {
            IdentifyEvent::Identified {
                peer_id, mut info, ..
            } => {
                if info.listen_addrs.len() > MAX_IDENTIFY_ADDRESSES {
                    debug!(
                        self.log,
                        "More than 20 addresses have been identified, truncating"
                    );
                    info.listen_addrs.truncate(MAX_IDENTIFY_ADDRESSES);
                }
                debug!(self.log, "Identified Peer"; "Peer" => format!("{}", peer_id),
                "Protocol Version" => info.protocol_version,
                "Agent Version" => info.agent_version,
                "Listening Addresses" => format!("{:?}", info.listen_addrs),
                "Protocols" => format!("{:?}", info.protocols)
                );
            }
            IdentifyEvent::Error { .. } => {}
            IdentifyEvent::SendBack { .. } => {}
        }
    }
}

impl<TSubstream: AsyncRead + AsyncWrite> NetworkBehaviourEventProcess<Discv5Event>
    for Behaviour<TSubstream>
{
    fn inject_event(&mut self, _event: Discv5Event) {
        // discv5 has no events to inject
    }
}

/// Implements the combined behaviour for the libp2p service.
impl<TSubstream: AsyncRead + AsyncWrite> Behaviour<TSubstream> {
    /* Pubsub behaviour functions */

    /// Subscribes to a gossipsub topic.
    pub fn subscribe(&mut self, topic: Topic) -> bool {
        self.gossipsub.subscribe(topic)
    }

    /// Publishes a message on the pubsub (gossipsub) behaviour.
    pub fn publish(&mut self, topics: Vec<Topic>, message: PubsubMessage) {
        let message_data = message.to_data();
        for topic in topics {
            self.gossipsub.publish(topic, message_data.clone());
        }
    }

    /* Eth2 RPC behaviour functions */

    /// Sends an RPC Request/Response via the RPC protocol.
    pub fn send_rpc(&mut self, peer_id: PeerId, rpc_event: RPCEvent) {
        self.eth2_rpc.send_rpc(peer_id, rpc_event);
    }

    /* Discovery / Peer management functions */
    pub fn connected_peers(&self) -> usize {
        self.discovery.connected_peers()
    }
}

/// The types of events than can be obtained from polling the behaviour.
pub enum BehaviourEvent {
    RPC(PeerId, RPCEvent),
    PeerDialed(PeerId),
    PeerDisconnected(PeerId),
    GossipMessage {
        source: PeerId,
        topics: Vec<TopicHash>,
        message: PubsubMessage,
    },
}

/// Messages that are passed to and from the pubsub (Gossipsub) behaviour. These are encoded and
/// decoded upstream.
#[derive(Debug, Clone, PartialEq)]
pub enum PubsubMessage {
    /// Gossipsub message providing notification of a new block.
    Block(Vec<u8>),
    /// Gossipsub message providing notification of a new attestation.
    Attestation(Vec<u8>),
    /// Gossipsub message providing notification of a voluntary exit.
    VoluntaryExit(Vec<u8>),
    /// Gossipsub message providing notification of a new proposer slashing.
    ProposerSlashing(Vec<u8>),
    /// Gossipsub message providing notification of a new attester slashing.
    AttesterSlashing(Vec<u8>),
    /// Gossipsub message from an unknown topic.
    Unknown(Vec<u8>),
}

impl PubsubMessage {
    /* Note: This is assuming we are not hashing topics. If we choose to hash topics, these will
     * need to be modified.
     *
     * Also note that a message can be associated with many topics. As soon as one of the topics is
     * known we match. If none of the topics are known we return an unknown state.
     */
    fn from_topics(topics: &Vec<TopicHash>, data: Vec<u8>) -> Self {
        for topic in topics {
            // compare the prefix and postfix, then match on the topic
            let topic_parts: Vec<&str> = topic.as_str().split('/').collect();
            if topic_parts.len() == 4
                && topic_parts[1] == TOPIC_PREFIX
                && topic_parts[3] == TOPIC_ENCODING_POSTFIX
            {
                match topic_parts[2] {
                    BEACON_BLOCK_TOPIC => return PubsubMessage::Block(data),
                    BEACON_ATTESTATION_TOPIC => return PubsubMessage::Attestation(data),
                    VOLUNTARY_EXIT_TOPIC => return PubsubMessage::VoluntaryExit(data),
                    PROPOSER_SLASHING_TOPIC => return PubsubMessage::ProposerSlashing(data),
                    ATTESTER_SLASHING_TOPIC => return PubsubMessage::AttesterSlashing(data),
                    _ => {}
                }
            }
        }
        PubsubMessage::Unknown(data)
    }

    fn to_data(self) -> Vec<u8> {
        match self {
            PubsubMessage::Block(data)
            | PubsubMessage::Attestation(data)
            | PubsubMessage::VoluntaryExit(data)
            | PubsubMessage::ProposerSlashing(data)
            | PubsubMessage::AttesterSlashing(data)
            | PubsubMessage::Unknown(data) => data,
        }
    }
}
