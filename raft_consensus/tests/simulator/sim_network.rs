// TODO: Network simulation is overly complicated
// Instead of trying to implement the transport layer for the entire network
// Create one for each server process, can take the outgoing messages and put on
// simulator queue. Can handle replies inside of the transport object, when
// the simulator delivers an incoming request at that point we can
// create the oneshot channel for the reply, can have a function that gets
// polled by the simulator to check for replies that are ready to be sent
// Also maybe we can use Futures to make this easier? Well maybe not easier
// but make the interface make a little more sense, encapsulate the
// channels a little better

use std::{
    collections::{HashMap, HashSet},
    sync::mpsc,
    time::Duration,
};

use mock_instant::MockClock;
use raft_consensus::{rpc_messages::RpcMessage, LogCommand, ServerId};
use rand_chacha::ChaCha8Rng;
use rand_distr::{Bernoulli, Distribution, LogNormal};
use tracing::trace;

use super::{
    common::{ClockAdvance, SimLogCommand, SimTime},
    sim_transport::SimNetworkRaftTransport,
};

use rand_distr::num_traits::ToPrimitive;

#[derive(Debug, Clone)]
pub(crate) struct PacketLossProbability(pub(crate) f64);
#[derive(Debug, Clone)]
pub(crate) struct LatencyMean(pub(crate) f64);
#[derive(Debug, Clone)]
pub(crate) struct LatencyStdDev(pub(crate) f64);

pub(crate) struct NetworkConnectionQuality {
    /// Probability that a message is dropped
    packet_loss: Bernoulli,
    /// Latency is calculated with a log-normal distribution
    latency: LogNormal<f64>,
}

struct NetworkNode<C: LogCommand> {
    maybe_unclaimed_transport: Option<SimNetworkRaftTransport>,
    incoming_message_tx: mpsc::Sender<RpcMessage<C>>,
}

/// Models a network with packet loss and latency, uses Bernoulli distribution for packet loss and log-normal distribution for latency
pub(crate) struct SimNetwork {
    pub(crate) server_ids: HashSet<ServerId>,
    /// Servers in network, map of IDs to network nodes (which contain the transport and incoming message channel)
    servers: HashMap<ServerId, NetworkNode<SimLogCommand>>,
    /// Map of server IDs to probability of packet loss, mean latency, std dev for latency ((server_id, server_id) -> (probability of message being dropped, mean latency, standard deviation)
    connections: HashMap<(ServerId, ServerId), NetworkConnectionQuality>,
    /// Receiver side of channel that receives outgoing messages from the server processes
    outbound_message_rx: mpsc::Receiver<RpcMessage<SimLogCommand>>,
    /// Vec with oneshot channel receivers to listen for replies to messages delivered to the server processes
    maybe_timer_rx: Option<mpsc::Receiver<ClockAdvance>>,
}

impl SimNetwork {
    /// Creates a new network with the given connections
    /// `network_connections` - Map of server IDs to probability of packet loss, mean latency, std dev for latency ((server_id, server_id) -> (probability of message being dropped, mean latency, standard deviation)
    pub(crate) fn new(
        network_connections: HashMap<
            (ServerId, ServerId),
            (PacketLossProbability, LatencyMean, LatencyStdDev),
        >,
    ) -> Self {
        let server_connections = network_connections
            .keys()
            .into_iter()
            .cloned()
            .collect::<HashSet<_>>();
        let network: HashMap<(ServerId, ServerId), NetworkConnectionQuality> = network_connections
            .into_iter()
            .map(|((from, to), (drop_probability, mean_latency, std_dev))| {
                assert!(
                    drop_probability.0 >= 0.0 && drop_probability.0 <= 1.0,
                    "(from={from:?}, to={to:?}): Drop probability should be between 0 and 1",
                    from=from,
                    to=to,
                );
                assert!(
                    mean_latency.0 >= 0.0,
                    "(from={from:?}, to={to:?}): Latency should be greater than or equal to 0",
                    from=from,
                    to=to,
                );
                assert!(
                    std_dev.0 >= 0.0,
                    "(from={from:?}, to={to:?}): Standard deviation should be greater than or equal to 0",
                    from=from,
                    to=to,
                );
                assert!(
                    server_connections.contains(&(to, from)),
                    "Connection (from={from:?}, to={to:?}) should be symmetric, i.e. (from={to:?}, to={from:?}) should also be present"
                );
                ((from, to), NetworkConnectionQuality {
                    packet_loss: Bernoulli::new(drop_probability.0)
                        .expect("Could not create Bernoulli distribution for packet loss"),
                    latency: LogNormal::new(mean_latency.0.ln(), std_dev.0)
                        .expect("Could not create LogNormal distribution for latency")
                })
            }).collect();

        let (outbound_message_tx, outbound_message_rx) = mpsc::channel();
        let (timer_tx, timer_rx) = mpsc::channel();

        let server_ids: HashSet<ServerId> = network.keys().map(|(from, _)| from).cloned().collect();
        let mut servers = HashMap::new();
        for server_id in &server_ids {
            let (inbound_message_tx, inbound_message_rx) = mpsc::channel();
            servers.insert(
                *server_id,
                NetworkNode {
                    maybe_unclaimed_transport: Some(SimNetworkRaftTransport::new(
                        outbound_message_tx.clone(),
                        inbound_message_rx,
                        timer_tx.clone(),
                    )),
                    incoming_message_tx: inbound_message_tx,
                },
            );
        }
        SimNetwork {
            server_ids,
            servers,
            connections: network,
            outbound_message_rx,
            maybe_timer_rx: Some(timer_rx),
        }
    }

    /// Creates a network with the same packet loss and latency for all connections
    pub(crate) fn with_defaults(
        num_servers: u64,
        packet_loss: PacketLossProbability,
        mean_latency: LatencyMean,
        latency_std_dev: LatencyStdDev,
    ) -> Self {
        let mut network = HashMap::new();
        for from in 0..num_servers {
            for to in 0..num_servers {
                if from != to {
                    network.insert(
                        (ServerId(from), ServerId(to)),
                        (
                            packet_loss.clone(),
                            mean_latency.clone(),
                            latency_std_dev.clone(),
                        ),
                    );
                }
            }
        }
        SimNetwork::new(network)
    }

    /// Called by the simulator when it is creating server processes
    /// After the network has been initialized it uses this method
    /// to take ownership of the transport object and give it to the server process
    pub(crate) fn take_transport_for(&mut self, server_id: ServerId) -> SimNetworkRaftTransport {
        self.servers
            .get_mut(&server_id)
            .expect(
                format!(
                    "Server with ID {server_id:?} not found",
                    server_id = server_id
                )
                .as_str(),
            )
            .maybe_unclaimed_transport
            .take()
            .expect("Transport already claimed")
    }

    pub(crate) fn take_timer_rx(&mut self) -> mpsc::Receiver<ClockAdvance> {
        self.maybe_timer_rx.take().expect("Timer already taken!")
    }

    /// Used by tests to partition the network into multiple partitions, where each partition is a disjoin set of server IDs
    /// Servers in each partition are connected to each other, but servers in different partitions are not connected
    pub(crate) fn partition_network(&mut self, partitions: Vec<HashSet<ServerId>>) {
        // Validate that sets are disjoint, i.e. no server is in multiple partitions
        let mut all_servers = HashSet::new();
        for partition in &partitions {
            for server in partition {
                assert!(
                    !all_servers.contains(server),
                    "Server {server:?} is in multiple partitions",
                    server = server
                );
                all_servers.insert(server);
            }
        }
        // Validate that all servers in the network are in a partition
        for (from, to) in self.connections.keys() {
            assert!(
                all_servers.contains(from) && all_servers.contains(to),
                "Server {from:?} or server {to:?} is not in any partition",
                from = from,
                to = to
            );
        }
        // Set packet loss to 1.0 for all connections between servers in different partitions
        let keys: Vec<(ServerId, ServerId)> =
            self.connections.keys().into_iter().cloned().collect();
        for (from, to) in keys {
            let from_partition = partitions
                .iter()
                .find(|partition| partition.contains(&from))
                .unwrap();
            if !from_partition.contains(&to) {
                self.connections.get_mut(&(from, to)).unwrap().packet_loss =
                    Bernoulli::new(1.0).unwrap();
            }
        }
    }

    pub(crate) fn heal_network_partition(&mut self) {
        for connection in self.connections.values_mut() {
            connection.packet_loss = Bernoulli::new(1.0).unwrap();
        }
    }

    /// Can be used by tests to change the probability of messages being dropped between two servers
    pub(crate) fn update_connection_packet_loss(
        &mut self,
        from: ServerId,
        to: ServerId,
        packet_loss: PacketLossProbability,
    ) {
        assert!(
            packet_loss.0 >= 0.0 && packet_loss.0 <= 1.0,
            "(from={from:?}, to={to:?}): Packet loss probability should be between 0 and 1",
            from = from,
            to = to,
        );
        let connection = self.connections.get_mut(&(from, to)).expect(&format!(
            "Should have a connection between server {from:?} and server {to:?}",
            from = from,
            to = to
        ));
        connection.packet_loss = Bernoulli::new(packet_loss.0).unwrap();
    }

    /// Can be used by tests to change the latency profile of messages sent from one server to another
    pub(crate) fn update_connection_latency(
        &mut self,
        from: ServerId,
        to: ServerId,
        mean_latency: LatencyMean,
        latency_std_dev: LatencyStdDev,
    ) {
        assert!(
            mean_latency.0 >= 0.0,
            "(from={from:?}, to={to:?}): Latency should be greater than or equal to 0",
            from = from,
            to = to,
        );
        assert!(
            latency_std_dev.0 >= 0.0,
            "(from={from:?}, to={to:?}): Standard deviation should be greater than or equal to 0",
            from = from,
            to = to,
        );
        let connection = self.connections.get_mut(&(from, to)).expect(&format!(
            "Should have a connection between server {from:?} and server {to:?}",
            from = from,
            to = to
        ));
        connection.latency = LogNormal::new(mean_latency.0.ln(), latency_std_dev.0).unwrap();
    }

    /// Looks at the what server the message is from and what server it should be delivered to and uses
    /// the network configuration to determine when and if a message should be delivered and with what latency
    /// This is called by the simulator
    fn determine_when_and_if_message_should_be_delivered(
        &self,
        message: RpcMessage<SimLogCommand>,
        rng: &mut ChaCha8Rng,
    ) -> Option<(RpcMessage<SimLogCommand>, SimTime)> {
        let to = message.to();
        let from = message.from();

        let time = MockClock::time();

        let connection = self.connections.get(&(from, to)).expect(&format!(
            "Should have a connection between server {from:?} and server {to:?}",
            from = from,
            to = to
        ));
        let drop_message = connection.packet_loss.sample(rng);
        let message_latency = connection
            .latency
            .sample(rng)
            .to_u64()
            .expect("Could not convert latency to u64");
        let message_time = time + Duration::from_millis(message_latency);
        if drop_message {
            trace!(
                "DROPPING NETWORK MESSAGE: from {from:?} to {to:?} at {time:?}ms - {message:?}",
                from = from,
                to = to,
                time = time.as_millis(),
                message = message
            );
            None
        } else {
            trace!(
                "QUEUEING NETWORK MESSAGE: from {from:?} to {to:?} at {message_time:?}ms with latency {message_latency:?} - {message:?}",
                from = from,
                to = to,
                message_time = message_time.as_millis(),
                message_latency = message_latency,
                message = message
            );
            Some((message, SimTime(message_time)))
        }
    }

    /// This is called by the simulator to get all messages that have been sent from server processes
    /// to the network that have not been queued in the simulator yet
    pub(crate) fn get_all_queued_outbound_messages(
        &mut self,
        rng: &mut ChaCha8Rng,
    ) -> Vec<(RpcMessage<SimLogCommand>, SimTime)> {
        let mut messages: Vec<(RpcMessage<SimLogCommand>, SimTime)> = Vec::new();

        while let Ok(message) = self.outbound_message_rx.try_recv() {
            if let Some(message_to_be_delivered) =
                self.determine_when_and_if_message_should_be_delivered(message, rng)
            {
                messages.push(message_to_be_delivered);
            }
        }

        messages
    }

    /// Called by the simulator to actually deliver the message to the server process once it is time to deliver it
    pub(crate) fn deliver_message(&mut self, target: ServerId, message: RpcMessage<SimLogCommand>) {
        let network_node = self.servers.get_mut(&target).expect(&format!(
            "Should have a server with ID {to:?} in the simulation",
            to = target
        ));

        network_node
            .incoming_message_tx
            .send(message)
            .expect("Could not send network message to server");
    }
}

mod tests {
    use std::time::Duration;

    use raft_consensus::rpc_messages::RpcMessage;
    use raft_consensus::{
        rpc_messages::Request, rpc_messages::RequestVote, transport::RaftTransportBridge, LogIndex,
        ServerId, TermIndex,
    };
    use rand::RngCore;
    use rand::SeedableRng;
    use rand_chacha::ChaCha8Rng;
    use tracing::info;
    use uuid::Uuid;

    use super::{LatencyMean, LatencyStdDev, PacketLossProbability, SimNetwork};

    fn new_rng(maybe_seed: Option<u64>) -> ChaCha8Rng {
        match maybe_seed {
            Some(seed) => ChaCha8Rng::seed_from_u64(seed),
            None => {
                let mut rng = ChaCha8Rng::from_entropy();
                let seed = rng.next_u64();
                info!("====================================");
                info!("RNG SEED FOR TESTS: {seed}", seed = seed);
                info!("====================================");
                ChaCha8Rng::seed_from_u64(seed)
            }
        }
    }

    #[test]
    fn it_should_return_a_vec_with_all_queued_outbound_messages_from_servers() {
        let mut rng = new_rng(None);

        let mut network = SimNetwork::with_defaults(
            2,
            PacketLossProbability(0.0),
            LatencyMean(0.0),
            LatencyStdDev(0.0),
        );

        let mut originating_server_transport = network.take_transport_for(ServerId(0));

        let outgoing_message = Request::RequestVote(RequestVote {
            request_id: Uuid::new_v4(),
            from: ServerId(0),
            to: ServerId(1),
            term: TermIndex(1),
            last_log_index: LogIndex(0),
            last_log_term: TermIndex(0),
        });
        let expected_message = outgoing_message.clone();

        originating_server_transport.enqueue_outgoing_request(outgoing_message);

        let messages = network.get_all_queued_outbound_messages(&mut rng);
        assert_eq!(messages.len(), 1);

        let (message, _) = messages.get(0).unwrap();
        match message {
            RpcMessage::Request(request) => {
                assert_eq!(request, &expected_message);
            }
            _ => panic!("Expected a request from node"),
        }
    }

    #[test]
    fn it_should_drop_messages_if_servers_are_in_different_network_partitions() {
        let mut rng = new_rng(None);

        let mut network = SimNetwork::with_defaults(
            2,
            PacketLossProbability(1.0),
            LatencyMean(0.0),
            LatencyStdDev(0.0),
        );

        let mut originating_server_transport = network.take_transport_for(ServerId(0));

        let outgoing_message = Request::RequestVote(RequestVote {
            request_id: Uuid::new_v4(),
            from: ServerId(0),
            to: ServerId(1),
            term: TermIndex(1),
            last_log_index: LogIndex(0),
            last_log_term: TermIndex(0),
        });

        originating_server_transport.enqueue_outgoing_request(outgoing_message);

        let messages = network.get_all_queued_outbound_messages(&mut rng);
        assert_eq!(messages.len(), 0);
    }

    #[test]
    fn it_should_deliver_message_to_transport_for_server() {
        let mut network = SimNetwork::with_defaults(
            2,
            PacketLossProbability(0.0),
            LatencyMean(0.0),
            LatencyStdDev(0.0),
        );

        let mut dest_server_transport = network.take_transport_for(ServerId(0));

        let incoming_message = Request::RequestVote(RequestVote {
            request_id: Uuid::new_v4(),
            from: ServerId(1),
            to: ServerId(0),
            term: TermIndex(1),
            last_log_index: LogIndex(0),
            last_log_term: TermIndex(0),
        });
        let expected_message = incoming_message.clone();

        let dest_server_thread = std::thread::spawn(move || {
            dest_server_transport
                .wait_for_next_incoming_message(Duration::from_secs(1))
                .unwrap()
        });

        network.deliver_message(ServerId(0), RpcMessage::Request(incoming_message));

        let message = dest_server_thread.join().unwrap();
        match message {
            RpcMessage::Request(request) => {
                assert_eq!(expected_message, request);
            }
            _ => panic!("Expected a request from node"),
        }
    }
}