use anyhow::anyhow;

use rand::prelude::SliceRandom;
use rand::{thread_rng, Rng};

use futures::StreamExt;
use tokio::net::{lookup_host, UdpSocket};
use tokio::sync::mpsc;
use tokio::time::sleep;

use log::{debug, error, info, trace, warn};

extern crate crc;
use crc::{crc32, Hasher32};

use std::convert::TryInto;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::common::ipv4_addr_src::IPV4AddrSource;
use crate::common::{Id, Node};
use crate::dht::dht_event::{DHTEvent, DHTEventType, MessageReceivedEvent};
use crate::dht::socket::DHTSocket;
use crate::dht::DHTSettings;
use crate::errors::RustyDHTError;
use crate::packets;
use crate::packets::MessageBuilder;
use crate::shutdown;
use crate::storage::node_bucket_storage::NodeStorage;
use crate::storage::node_wrapper::NodeWrapper;
use crate::storage::peer_storage::{PeerInfo, PeerStorage};
use crate::storage::throttler::Throttler;

struct DHTState {
    ip4_source: Box<dyn IPV4AddrSource + Send>,
    our_id: Id,
    buckets: Box<dyn NodeStorage + Send>,
    peer_storage: PeerStorage,
    token_secret: Vec<u8>,
    old_token_secret: Vec<u8>,
    settings: DHTSettings,
    subscribers: Vec<mpsc::Sender<DHTEvent>>,
}

/// This struct is the heart of the library - contains data structure and business logic to run a DHT node.
pub struct DHT {
    socket: Arc<DHTSocket>,

    /// Coarse-grained locking for stuff what needs it
    state: Arc<Mutex<DHTState>>,

    shutdown: shutdown::ShutdownReceiver,
}

impl DHT {
    /// Returns the current Id used by the DHT.
    pub fn get_id(&self) -> Id {
        self.state.lock().unwrap().our_id
    }

    /// Returns a full dump of all the info hashes and peers in storage.
    /// Peers that haven't announced since the provided `newer_than` can be optionally filtered.
    pub fn get_info_hashes(&self, newer_than: Option<Instant>) -> Vec<(Id, Vec<PeerInfo>)> {
        let state = self.state.lock().unwrap();
        let hashes = state.peer_storage.get_info_hashes();
        hashes
            .iter()
            .copied()
            .map(|hash| (hash, state.peer_storage.get_peers_info(&hash, newer_than)))
            .filter(|tup| tup.1.len() > 0)
            .collect()
    }

    /// Returns information about all currently-verified DHT nodes that we're "connected" with.
    pub fn get_nodes(&self) -> Vec<NodeWrapper> {
        self.state.lock().unwrap().buckets.get_all_verified()
    }

    /// Return a copy of the settings used by the DHT
    pub fn get_settings(&self) -> DHTSettings {
        self.state.lock().unwrap().settings.clone()
    }

    /// Creates a new DHT.
    ///
    /// # Arguments
    /// * `shutdown` - the DHT passes this to any sub-tasks that it spawns, and uses it to know when to stop its event own event loop.
    /// * `id` - an optional initial Id for the DHT. The DHT may change its Id if at some point its not valid for the external IPv4 address (as reported by ip4_source).
    /// * `listen_port` - the port that the DHT should bind its UDP socket on.
    /// * `ip4_source` - Some type that implements IPV4AddrSource. This object will be used by the DHT to keep up to date on its IPv4 address.
    /// * `buckets` - A function that takes an Id and returns a struct implementing NodeStorage. The NodeStorage-implementing type will be used to keep the nodes
    /// (or routing table) of the DHT.
    /// * `routers` - Array of string slices with hostname:port of DHT routers. These help us get bootstrapped onto the network.
    /// * `settings` - DHTSettings struct containing settings that DHT will use.
    pub fn new(
        shutdown: shutdown::ShutdownReceiver,
        id: Option<Id>,
        socket_addr: std::net::SocketAddr,
        ip4_source: Box<dyn IPV4AddrSource + Send>,
        mut buckets: Box<dyn NodeStorage + Send>,
        settings: DHTSettings,
    ) -> Result<DHT, RustyDHTError> {
        // If we were given a hardcoded id, use that until/unless we decide its invalid based on IP source.
        // If we weren't given a hardcoded id, try to generate one based on IP source.
        // Finally, if all else fails, generate a totally random id.
        let our_id = {
            match id {
                Some(id) => id,

                None => match ip4_source.get_best_ipv4() {
                    Some(ip) => {
                        let id = Id::from_ip(&IpAddr::V4(ip));
                        info!(target: "rustydht_lib::DHT",
                            "Our external IPv4 is {:?}. Generated id {} based on that",
                            ip, id
                        );
                        id
                    }

                    None => {
                        let id = Id::from_random(&mut thread_rng());
                        info!(target: "rustydht_lib::DHT", "No external IPv4 provided. Using random id {} for now.", id);
                        id
                    }
                },
            }
        };

        buckets.set_id(our_id);

        // Setup our UDP socket
        let socket = {
            let std_sock = std::net::UdpSocket::bind(socket_addr)
                .map_err(|e| RustyDHTError::GeneralError(e.into()))?;
            std_sock
                .set_nonblocking(true)
                .map_err(|e| RustyDHTError::GeneralError(e.into()))?;
            let tokio_sock =
                UdpSocket::from_std(std_sock).map_err(|e| RustyDHTError::GeneralError(e.into()))?;
            Arc::new(DHTSocket::new(shutdown.clone(), tokio_sock))
        };

        let token_secret = make_token_secret(settings.token_secret_size);

        let dht = DHT {
            socket: socket,
            state: Arc::new(Mutex::new(DHTState {
                ip4_source: ip4_source,
                our_id: our_id,
                buckets: buckets,
                peer_storage: PeerStorage::new(
                    settings.max_torrents,
                    settings.max_peers_per_torrent,
                ),
                token_secret: token_secret.clone(),
                old_token_secret: token_secret,
                settings: settings,
                subscribers: vec![],
            })),

            shutdown: shutdown,
        };

        Ok(dht)
    }

    /// Runs the main event loop of the DHT.
    ///
    /// It will only return if there's an error or if the DHT's ShutdownReceiver is signalled to stop the DHT.
    pub async fn run_event_loop(&self) -> Result<(), RustyDHTError> {
        match tokio::try_join!(
            // One-time
            self.ping_routers(self.shutdown.clone()),
            // Loop indefinitely
            self.accept_incoming_packets(),
            self.periodic_router_ping(self.shutdown.clone()),
            self.periodic_buddy_ping(self.shutdown.clone()),
            self.periodic_find_node(self.shutdown.clone()),
            self.periodic_ip4_maintenance(),
            self.periodic_token_rotation(),
            async {
                let to_ret: Result<(), RustyDHTError> = Err(RustyDHTError::ShutdownError(anyhow!(
                    "run_event_loop should shutdown"
                )));
                self.shutdown.clone().watch().await;
                return to_ret;
            }
        ) {
            Ok(_) => Ok(()),
            Err(e) => {
                if let RustyDHTError::ShutdownError(_) = e {
                    Ok(())
                } else {
                    Err(e)
                }
            }
        }
    }

    /// Sends a [Message](crate::packets::Message), awaits and returns a response.
    ///
    /// Note that `req` must be a request message (not a response or error message),
    /// as this method awaits a reply. DHT automatically handles sending responses for
    /// incoming requests.
    ///
    /// # Arguments
    /// * `req` - the message that should be sent
    /// * `dest` - the IP/port of the intended recipient
    /// * `dest_id` - the Id of the DHT node listening at `dest`, if known. Otherwise, `None` can be provided.
    /// * `timeout` - An optional timeout. If supplied, this function will return
    /// a [RustyDHTError::TimeoutError](crate::errors::RustyDHTError::TimeoutError) if `dest` does not reply
    /// to the message within the allotted time.
    pub async fn send_request(
        &self,
        req: packets::Message,
        dest: SocketAddr,
        dest_id: Option<Id>,
        timeout: Option<Duration>,
    ) -> Result<packets::Message, RustyDHTError> {
        match timeout {
            Some(timeout) => match tokio::time::timeout(
                timeout,
                DHT::common_send_and_handle_response(
                    self.state.clone(),
                    self.socket.clone(),
                    req.clone(),
                    dest,
                    dest_id,
                ),
            )
            .await
            {
                Ok(result) => result,
                Err(_) => Err(RustyDHTError::TimeoutError(anyhow!(
                    "Timed out after {:?} waiting for {} to respond to {:?}",
                    timeout,
                    dest,
                    req
                ))),
            },
            None => {
                DHT::common_send_and_handle_response(
                    self.state.clone(),
                    self.socket.clone(),
                    req.clone(),
                    dest,
                    dest_id,
                )
                .await
            }
        }
    }

    /// Subscribe to DHTEvent notifications from the DHT.
    ///
    /// When you're sick of receiving events from the DHT, just drop the receiver.
    pub fn subscribe(&self) -> mpsc::Receiver<DHTEvent> {
        let (tx, rx) = mpsc::channel(32);
        let mut state = self.state.lock().unwrap();
        state.subscribers.push(tx);
        rx
    }
}

impl DHT {
    async fn accept_incoming_packets(&self) -> Result<(), RustyDHTError> {
        let mut throttler = Throttler::<32>::new(
            10,
            Duration::from_secs(6),
            Duration::from_secs(60),
            Duration::from_secs(86400),
        );
        let read_only = self.state.lock().unwrap().settings.read_only;
        loop {
            match async {
                let (msg, addr) = self.socket.recv_from().await?;

                // Drop the packet if the IP has been throttled.
                if throttler.check_throttle(addr.ip(), None, None) {
                    return Ok(());
                }

                // Filter out packets sent from port 0. We can't reply to these.
                if addr.port() == 0 {
                    warn!(target: "rustydht_lib::DHT", "{} has invalid port - dropping packet", addr);
                    return Ok(());
                }

                // Respond to requests, but only if we're not read-only
                if !read_only {
                    self.accept_single_packet(msg.clone(), addr).await?;
                }

                // Send a MessageReceivedEvent to any subscribers
                self.send_packet_to_subscribers(msg, addr).await;

                Ok::<(), RustyDHTError>(())
            }.await {
                Ok(_) => continue,

                Err(err) => match err {
                    RustyDHTError::PacketParseError(internal) => {
                        warn!(target: "rustydht_lib::DHT", "Packet parsing error: {:?}", internal);
                        continue;
                    }

                    RustyDHTError::ConntrackError(e) => {
                        warn!(target: "rustydht_lib::DHT", "Connection tracking error: {:?}", e);
                        continue;
                    }

                    _ => {
                        return Err(err.into());
                    }
                },
            }
        }
    }

    /// Carries out some common tasks for each incoming request
    ///
    /// 1. Determines if the requester's id is valid for their IP
    /// 2. Makes sure they have a chance to join the routing table
    fn common_request_handling(
        &self,
        remote_addr: SocketAddr,
        msg: &packets::Message,
    ) -> Result<(), RustyDHTError> {
        let sender_id = match msg.get_author_id() {
            Some(sender_id) => sender_id,
            None => {
                return Err(RustyDHTError::PacketParseError(anyhow!(
                    "Failed to extract sender's id"
                )));
            }
        };

        // Is id valid for IP?
        let is_id_valid = sender_id.is_valid_for_ip(&remote_addr.ip());
        let read_only = match msg.read_only {
            Some(ro) => ro,
            _ => false,
        };
        if is_id_valid && !read_only {
            self.state
                .lock()
                .unwrap()
                .buckets
                .add_or_update(Node::new(sender_id, remote_addr), false);
        }
        Ok(())
    }

    async fn accept_single_packet(
        &self,
        msg: packets::Message,
        addr: SocketAddr,
    ) -> Result<(), RustyDHTError> {
        match &msg.message_type {
            packets::MessageType::Request(request_variant) => {
                match request_variant {
                    packets::RequestSpecific::PingRequest(arguments) => {
                        self.common_request_handling(addr, &msg)?;

                        // Build a ping reply
                        let reply = MessageBuilder::new_ping_response()
                            .sender_id(self.state.lock().unwrap().our_id)
                            .transaction_id(msg.transaction_id.clone())
                            .requester_ip(addr)
                            .build()?;
                        self.socket
                            .send_to(reply, addr, Some(arguments.requester_id))
                            .await?;
                    }

                    packets::RequestSpecific::GetPeersRequest(arguments) => {
                        self.common_request_handling(addr, &msg)?;
                        let reply = {
                            let state = self.state.lock().unwrap();

                            // First, see if we have any peers for their info_hash
                            let peers = {
                                let newer_than = Instant::now().checked_sub(Duration::from_secs(
                                    state.settings.get_peers_freshness_secs,
                                ));
                                let mut peers = state
                                    .peer_storage
                                    .get_peers(&arguments.info_hash, newer_than);
                                peers.truncate(state.settings.max_peers_response);
                                peers
                            };
                            let token = calculate_token(&addr, state.token_secret.clone());

                            let reply = match peers.len() {
                                0 => {
                                    let nearest = state.buckets.get_nearest_nodes(
                                        &arguments.info_hash,
                                        Some(&arguments.requester_id),
                                    );

                                    MessageBuilder::new_get_peers_response()
                                        .sender_id(state.our_id.clone())
                                        .transaction_id(msg.transaction_id)
                                        .requester_ip(addr)
                                        .token(token.to_vec())
                                        .nodes(nearest)
                                        .build()?
                                }

                                _ => MessageBuilder::new_get_peers_response()
                                    .sender_id(state.our_id.clone())
                                    .transaction_id(msg.transaction_id)
                                    .requester_ip(addr)
                                    .token(token.to_vec())
                                    .peers(peers)
                                    .build()?,
                            };
                            reply
                        };
                        self.socket
                            .send_to(reply, addr, Some(arguments.requester_id))
                            .await?;
                    }

                    packets::RequestSpecific::FindNodeRequest(arguments) => {
                        self.common_request_handling(addr, &msg)?;
                        let reply = {
                            let state = self.state.lock().unwrap();
                            let nearest = state.buckets.get_nearest_nodes(
                                &arguments.target,
                                Some(&arguments.requester_id),
                            );
                            MessageBuilder::new_find_node_response()
                                .sender_id(state.our_id.clone())
                                .transaction_id(msg.transaction_id)
                                .requester_ip(addr)
                                .nodes(nearest)
                                .build()?
                        };

                        self.socket
                            .send_to(reply, addr, Some(arguments.requester_id))
                            .await?;
                    }

                    packets::RequestSpecific::AnnouncePeerRequest(arguments) => {
                        self.common_request_handling(addr, &msg)?;
                        let reply = {
                            let mut state = self.state.lock().unwrap();

                            let is_token_valid = arguments.token
                                == calculate_token(&addr, state.token_secret.clone())
                                || arguments.token
                                    == calculate_token(&addr, state.old_token_secret.clone());

                            if is_token_valid {
                                let sockaddr = match arguments.implied_port {
                                    Some(implied_port) if implied_port == true => addr,

                                    _ => {
                                        let mut tmp = addr.clone();
                                        tmp.set_port(arguments.port);
                                        tmp
                                    }
                                };

                                state
                                    .peer_storage
                                    .announce_peer(arguments.info_hash, sockaddr);

                                Some(
                                    MessageBuilder::new_announce_peer_response()
                                        .sender_id(state.our_id)
                                        .transaction_id(msg.transaction_id.clone())
                                        .requester_ip(addr)
                                        .build()?,
                                )
                            } else {
                                None
                            }
                        };

                        if let Some(reply) = reply {
                            self.socket
                                .send_to(reply, addr, Some(arguments.requester_id))
                                .await?;
                        }
                    }

                    packets::RequestSpecific::SampleInfoHashesRequest(arguments) => {
                        self.common_request_handling(addr, &msg)?;
                        let reply = {
                            let state = self.state.lock().unwrap();

                            let nearest = state.buckets.get_nearest_nodes(
                                &arguments.target,
                                Some(&arguments.requester_id),
                            );

                            let (info_hashes, total_info_hashes) = {
                                let info_hashes = state.peer_storage.get_info_hashes();
                                let total_info_hashes = info_hashes.len();
                                let info_hashes = {
                                    let mut rng = thread_rng();
                                    state
                                        .peer_storage
                                        .get_info_hashes()
                                        .as_mut_slice()
                                        .partial_shuffle(
                                            &mut rng,
                                            state.settings.max_sample_response,
                                        )
                                        .0
                                        .to_vec()
                                };
                                (info_hashes, total_info_hashes)
                            };

                            MessageBuilder::new_sample_infohashes_response()
                                .sender_id(state.our_id)
                                .transaction_id(msg.transaction_id)
                                .requester_ip(addr)
                                .interval(Duration::from_secs(
                                    state.settings.min_sample_interval_secs.try_into().unwrap(),
                                ))
                                .nodes(nearest)
                                .samples(info_hashes)
                                .num_infohashes(total_info_hashes)
                                .build()?
                        };

                        self.socket
                            .send_to(reply, addr, Some(arguments.requester_id))
                            .await?;
                    }
                }
            }

            packets::MessageType::Response(_) => { /*Responses should be handled by the sender via notification channel.*/
            }

            _ => {
                warn!(target: "rustydht_lib::DHT",
                    "Received unsupported/unexpected KRPCMessage variant from {:?}: {:?}",
                    addr, msg
                );
            }
        }

        return Ok(());
    }

    async fn send_packet_to_subscribers(&self, msg: packets::Message, _addr: SocketAddr) {
        // Notify any subscribers about the event
        let event = DHTEvent {
            event_type: DHTEventType::MessageReceived(MessageReceivedEvent { message: msg }),
        };
        let mut state = self.state.lock().unwrap();
        state.subscribers.retain(|sub| {
            eprintln!("Gotta do notifications for {:?}", event);
            match sub.try_send(event.clone()) {
                Ok(()) => true,
                Err(e) => match e {
                    tokio::sync::mpsc::error::TrySendError::Closed(_) => {
                        // Remove the sender from the subscriptions since they hung up on us (rude)
                        trace!(target: "rustydht_lib::DHT", "Removing channel for closed DHTEvent subscriber");
                        false
                    }
                    tokio::sync::mpsc::error::TrySendError::Full(_) => {
                        warn!(target: "rustydht_lib::DHT", "DHTEvent subscriber channel is full - can't send event {:?}", event);
                        true
                    }
                }
            }
        });
    }

    async fn periodic_buddy_ping(
        &self,
        shutdown: shutdown::ShutdownReceiver,
    ) -> Result<(), RustyDHTError> {
        loop {
            let ping_check_interval_secs =
                self.state.lock().unwrap().settings.ping_check_interval_secs;
            sleep(Duration::from_secs(ping_check_interval_secs)).await;

            // Package things that need state into a block so that Rust will not complain about MutexGuard kept past .await
            let reverify_interval_secs = {
                let mut state = self.state.lock().unwrap();
                let count = state.buckets.count();
                debug!(target: "rustydht_lib::DHT",
                    "Pruning node buckets. Storage has {} unverified, {} verified",
                    count.0,
                    count.1,
                );
                let reverify_grace_period_secs = state.settings.reverify_grace_period_secs;
                let verify_grace_period_secs = state.settings.verify_grace_period_secs;
                state.buckets.prune(
                    Duration::from_secs(reverify_grace_period_secs),
                    Duration::from_secs(verify_grace_period_secs),
                );

                state.settings.reverify_interval_secs
            };
            match Instant::now().checked_sub(Duration::from_secs(reverify_interval_secs)) {
                None => {
                    debug!(target: "rustydht_lib::DHT", "Monotonic clock underflow - skipping this round of pings");
                }

                Some(ping_if_older_than) => {
                    debug!(target: "rustydht_lib::DHT", "Sending pings to all nodes that have never verified or haven't been verified in a while");
                    let (unverified, verified) = {
                        let state = self.state.lock().unwrap();
                        (
                            state.buckets.get_all_unverified(),
                            state.buckets.get_all_verified(),
                        )
                    };
                    // Ping everybody we haven't verified
                    for wrapper in unverified {
                        // Some things in here are actually verified... don't bother them too often
                        if let Some(last_verified) = wrapper.last_verified {
                            if last_verified >= ping_if_older_than {
                                continue;
                            }
                            trace!(target: "rustydht_lib::DHT", "Sending ping to reverify backup {:?}", wrapper.node);
                        } else {
                            trace!(target: "rustydht_lib::DHT",
                                "Sending ping to verify {:?} (last seen {} seconds ago)",
                                wrapper.node,
                                (Instant::now() - wrapper.last_seen).as_secs()
                            );
                        }
                        self.ping_internal(
                            shutdown.clone(),
                            wrapper.node.address,
                            Some(wrapper.node.id),
                        )
                        .await?;
                    }

                    // Reverify those who haven't been verified recently
                    for wrapper in verified {
                        if let Some(last_verified) = wrapper.last_verified {
                            if last_verified >= ping_if_older_than {
                                continue;
                            }
                        }
                        trace!(target: "rustydht_lib::DHT", "Sending ping to reverify {:?}", wrapper.node);
                        self.ping_internal(
                            shutdown.clone(),
                            wrapper.node.address,
                            Some(wrapper.node.id),
                        )
                        .await?;
                    }
                }
            }
        }
    }

    async fn periodic_find_node(
        &self,
        shutdown: shutdown::ShutdownReceiver,
    ) -> Result<(), RustyDHTError> {
        loop {
            let find_node_interval_secs =
                self.state.lock().unwrap().settings.find_nodes_interval_secs;
            sleep(Duration::from_secs(find_node_interval_secs)).await;

            let (count_unverified, count_verified) = self.state.lock().unwrap().buckets.count();

            // If we don't know anybody, force a router ping.
            // This is helpful if we've been asleep for a while and lost all peers
            if count_verified <= 0 {
                self.ping_routers(shutdown.clone()).await?;
            }

            // Package things that need state into this block to avoid issues with MutexGuard kept over .await
            let (nearest_nodes, id_near_us) = {
                let state = self.state.lock().unwrap();
                if count_unverified > state.settings.find_nodes_skip_count {
                    debug!(target: "rustydht_lib::DHT", "Skipping find_node as we already have enough unverified");
                    continue;
                }

                let id_near_us = state.our_id.make_mutant(4).unwrap();

                // Find the closest nodes to ask
                (
                    state.buckets.get_nearest_nodes(&id_near_us, None),
                    id_near_us,
                )
            };
            trace!(
                target: "rustydht_lib::DHT",
                "Sending find_node to {} nodes about {:?}",
                nearest_nodes.len(),
                id_near_us
            );
            for node in nearest_nodes {
                self.find_node_internal(shutdown.clone(), node.address, Some(node.id), id_near_us)
                    .await?;
            }
        }
    }

    async fn periodic_ip4_maintenance(&self) -> Result<(), RustyDHTError> {
        loop {
            sleep(Duration::from_secs(10)).await;

            let mut state = self.state.lock().unwrap();
            state.ip4_source.decay();

            if let Some(ip) = state.ip4_source.get_best_ipv4() {
                let ip = IpAddr::V4(ip);
                if !state.our_id.is_valid_for_ip(&ip) {
                    let new_id = Id::from_ip(&ip);
                    info!(target: "rustydht_lib::DHT",
                        "Our current id {} is not valid for IP {}. Using new id {}",
                        state.our_id,
                        ip,
                        new_id
                    );
                    state.our_id = new_id;
                    state.buckets.set_id(new_id);
                }
            }
        }
    }

    async fn periodic_router_ping(
        &self,
        shutdown: shutdown::ShutdownReceiver,
    ) -> Result<(), RustyDHTError> {
        loop {
            let router_ping_interval_secs = self
                .state
                .lock()
                .unwrap()
                .settings
                .router_ping_interval_secs;
            sleep(Duration::from_secs(router_ping_interval_secs)).await;
            debug!(target: "rustydht_lib::DHT", "Pinging routers");
            let shutdown_clone = shutdown.clone();
            self.ping_routers(shutdown_clone).await?;
        }
    }

    async fn periodic_token_rotation(&self) -> Result<(), RustyDHTError> {
        loop {
            sleep(Duration::from_secs(300)).await;
            self.rotate_token_secrets();
        }
    }

    /// Build and send a ping to a target. Doesn't wait for a response
    async fn ping_internal(
        &self,
        shutdown: shutdown::ShutdownReceiver,
        target: SocketAddr,
        target_id: Option<Id>,
    ) -> Result<(), RustyDHTError> {
        let state = self.state.clone();
        let socket = self.socket.clone();
        shutdown::ShutdownReceiver::spawn_with_shutdown(
            shutdown,
            async move {
                let req = {
                    let state = state.lock().unwrap();
                    MessageBuilder::new_ping_request()
                        .sender_id(state.our_id)
                        .read_only(state.settings.read_only)
                        .build()
                        .expect("Failed to build ping packet")
                };

                if let Err(e) =
                    DHT::common_send_and_handle_response(state, socket, req, target, target_id)
                        .await
                {
                    match e {
                        RustyDHTError::TimeoutError(e) => {
                            debug!(target: "rustydht_lib::DHT", "Ping timed out: {}", e);
                        }

                        _ => {
                            error!(target: "rustydht_lib::DHT", "Error during ping: {}", e);
                        }
                    }
                }
            },
            format!("ping to {}", target),
            Some(Duration::from_secs(5)),
        );
        Ok(())
    }

    /// Send a request and await on the notification channel for a response.
    /// Then handle the response by adding the responder to routing tables,
    /// letting them "vote" on our IPv4 address, etc.
    ///
    /// Note that DHTSocket guarantees that we'll only see responses to requests that we
    /// actually sent - "spurious" or "extraneous" responses will be dropped in DHTSocket
    /// before we see them.
    async fn common_send_and_handle_response(
        state: Arc<Mutex<DHTState>>,
        socket: Arc<DHTSocket>,
        msg: packets::Message,
        target: SocketAddr,
        target_id: Option<Id>,
    ) -> Result<packets::Message, RustyDHTError> {
        if !matches!(msg.message_type, packets::MessageType::Request(_)) {
            return Err(RustyDHTError::GeneralError(anyhow!(
                "This method is only for sending requests"
            )));
        }

        let maybe_receiver = socket.send_to(msg.clone(), target, target_id).await?;
        match maybe_receiver {
            Some(mut receiver) => match receiver.recv().await {
                Some(reply) => match &reply.message_type {
                    packets::MessageType::Response(response_variant) => {
                        // Get the id of the sender - safe to expect because all Response variants are guaranteed
                        // to have an Id (only error doesn't)
                        let their_id =
                            reply.get_author_id().expect("response doesn't have Id!?");
                        let id_is_valid = their_id.is_valid_for_ip(&target.ip());

                        // Node is fit to be in our routing buckets and vote on our IPv4 only
                        // if its id is valid for its IP.
                        if id_is_valid {
                            let mut state = state.lock().unwrap();
                            DHT::ip4_vote_helper(&mut state, &target, &reply);
                            state
                                .buckets
                                .add_or_update(Node::new(their_id, target), true);
                        }

                        match response_variant {
                            // Special handling for find_node responses
                            // Add the nodes we got back as "seen" (even though we haven't necessarily seen them directly yet).
                            // They will be pinged later in an attempt to verify them.
                            packets::ResponseSpecific::FindNodeResponse(args) => {
                                let mut state = state.lock().unwrap();
                                for node in &args.nodes {
                                    if node.id.is_valid_for_ip(&node.address.ip()) {
                                        state.buckets.add_or_update(node.clone(), false);
                                    }
                                }
                            }
                            _ => {}
                        }

                        Ok(reply)
                    }

                    _ => Err(RustyDHTError::GeneralError(anyhow!("Received wrong Message type as response from {}. {:?}", target, reply)))
                },

                None => Err(RustyDHTError::TimeoutError(anyhow!("Response channel was cleaned up while we were waiting for a reply from {}. Message we sent: {:?}", target, msg)))
            },

            None => Err(RustyDHTError::GeneralError(anyhow!("Didn't get a response channel after sending a request to {}. We sent: {:?}", target, msg)))
        }
    }

    async fn ping_router<G: AsRef<str>>(
        &self,
        shutdown: shutdown::ShutdownReceiver,
        hostname: G,
    ) -> Result<(), RustyDHTError> {
        let hostname = hostname.as_ref();
        // Resolve and add to request storage
        let resolve = lookup_host(hostname).await;
        if let Err(err) = resolve {
            // Used to only eat the specific errors corresponding to a failure to resolve,
            // but they vary by platform and it's a pain. For now, we'll eat all host
            // resolution errors.
            warn!(
                target: "rustydht_lib::DHT",
                "Failed to resolve host {} due to error {:#?}. Try again later.",
                hostname, err
            );
            return Ok(());
        }

        for socket_addr in resolve.unwrap() {
            if socket_addr.is_ipv4() {
                let shutdown_clone = shutdown.clone();
                self.ping_internal(shutdown_clone, socket_addr, None)
                    .await?;
                break;
            }
        }
        Ok(())
    }

    /// Pings some bittorrent routers
    async fn ping_routers(
        &self,
        shutdown: shutdown::ShutdownReceiver,
    ) -> Result<(), RustyDHTError> {
        let mut futures = futures::stream::FuturesUnordered::new();
        let routers = self.state.lock().unwrap().settings.routers.clone();
        for hostname in routers {
            let shutdown_clone = shutdown.clone();
            futures.push(self.ping_router(shutdown_clone, hostname));
        }
        while let Some(result) = futures.next().await {
            result?;
        }
        Ok(())
    }

    fn rotate_token_secrets(&self) {
        let mut state = self.state.lock().unwrap();
        let new_token_secret = make_token_secret(state.settings.token_secret_size);

        state.old_token_secret = state.token_secret.clone();
        state.token_secret = new_token_secret;
        debug!(
            target: "rustydht_lib::DHT",
            "Rotating token secret. New secret is {:?}, old secret is {:?}",
            state.token_secret,
            state.old_token_secret
        );
    }

    async fn find_node_internal(
        &self,
        shutdown: shutdown::ShutdownReceiver,
        dest: SocketAddr,
        dest_id: Option<Id>,
        target: Id,
    ) -> Result<(), RustyDHTError> {
        let state = self.state.clone();
        let socket = self.socket.clone();
        shutdown::ShutdownReceiver::spawn_with_shutdown(
            shutdown,
            async move {
                let req = {
                    let state = state.lock().unwrap();
                    MessageBuilder::new_find_node_request()
                        .sender_id(state.our_id)
                        .read_only(state.settings.read_only)
                        .target(target)
                        .build()
                        .expect("Failed to build ping packet")
                };

                if let Err(e) =
                    DHT::common_send_and_handle_response(state, socket, req, dest, dest_id).await
                {
                    match e {
                        RustyDHTError::TimeoutError(e) => {
                            debug!(target: "rustydht_lib::DHT", "find_node timed out: {}", e);
                        }

                        _ => {
                            error!(target: "rustydht_lib::DHT", "Error during find_node: {}", e);
                        }
                    }
                }
            },
            format!("find_node to {} for {}", dest, target),
            Some(Duration::from_secs(5)),
        );
        Ok(())
    }

    /// Adds a 'vote' for whatever IP address the sender says we have.
    fn ip4_vote_helper(state: &mut DHTState, addr: &SocketAddr, msg: &packets::Message) {
        if let IpAddr::V4(their_ip) = addr.ip() {
            if let Some(they_claim_our_sockaddr) = &msg.requester_ip {
                if let SocketAddr::V4(they_claim_our_sockaddr) = they_claim_our_sockaddr {
                    state
                        .ip4_source
                        .add_vote(their_ip, they_claim_our_sockaddr.ip().clone());
                }
            }
        }
    }
}

/// Calculates a peer announce token based on a sockaddr and some secret.
/// Pretty positive this isn't cryptographically safe but I'm not too worried.
/// If we care about that later we can use a proper HMAC or something.
fn calculate_token<T: AsRef<[u8]>>(remote: &SocketAddr, secret: T) -> [u8; 4] {
    let secret = secret.as_ref();
    let mut digest = crc32::Digest::new(crc32::CASTAGNOLI);
    // digest.write(&crate::packets::sockaddr_to_bytes(remote));
    let octets = match remote.ip() {
        std::net::IpAddr::V4(v4) => v4.octets().to_vec(),
        std::net::IpAddr::V6(v6) => v6.octets().to_vec(),
    };
    digest.write(&octets);
    digest.write(secret);
    let checksum: u32 = digest.sum32();

    return checksum.to_be_bytes();
}

fn make_token_secret(size: usize) -> Vec<u8> {
    let mut token_secret = vec![0; size];
    token_secret.fill_with(|| thread_rng().gen());
    token_secret
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::common::ipv4_addr_src::StaticIPV4AddrSource;
    use crate::dht::DHTBuilder;
    use crate::dht::DHTSettingsBuilder;
    use anyhow::anyhow;
    use std::boxed::Box;
    use std::net::{Ipv4Addr, SocketAddrV4};

    async fn make_test_dht(
        port: u16,
    ) -> (DHT, shutdown::ShutdownSender, shutdown::ShutdownReceiver) {
        let ipv4 = Ipv4Addr::new(1, 2, 3, 4);
        let phony_ip4 = Box::new(StaticIPV4AddrSource::new(ipv4));
        let (tx, rx) = shutdown::create_shutdown();
        (
            DHTBuilder::new()
                .initial_id(get_dht_id())
                .listen_addr(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
                .ip_source(phony_ip4)
                .build(rx.clone())
                .unwrap(),
            tx,
            rx,
        )
    }

    #[tokio::test]
    async fn test_responds_to_ping() -> Result<(), RustyDHTError> {
        let requester_id = Id::from_random(&mut thread_rng());
        let ping_request = MessageBuilder::new_ping_request()
            .sender_id(requester_id)
            .build()?;

        let port = 1948;
        let (dht, mut shutdown_tx, shutdown_rx) = make_test_dht(port).await;
        shutdown::ShutdownReceiver::spawn_with_shutdown(
            shutdown_rx,
            async move {
                dht.run_event_loop().await.unwrap();
            },
            "Test DHT",
            Some(Duration::from_secs(10)),
        );

        let res = send_and_receive(ping_request.clone(), port).await.unwrap();

        assert_eq!(res.transaction_id, ping_request.transaction_id);
        assert_eq!(
            res.message_type,
            packets::MessageType::Response(packets::ResponseSpecific::PingResponse(
                packets::PingResponseArguments {
                    responder_id: get_dht_id()
                }
            ))
        );

        shutdown_tx.shutdown().await;

        Ok(())
    }

    #[tokio::test]
    async fn test_responds_to_get_peers() -> Result<(), RustyDHTError> {
        let requester_id = Id::from_random(&mut thread_rng());
        let desired_info_hash = Id::from_random(&mut thread_rng());
        let request = MessageBuilder::new_get_peers_request()
            .sender_id(requester_id)
            .target(desired_info_hash)
            .build()?;

        let port = 1974;
        let (dht, mut shutdown_tx, shutdown_rx) = make_test_dht(port).await;
        shutdown::ShutdownReceiver::spawn_with_shutdown(
            shutdown_rx,
            async move {
                dht.run_event_loop().await.unwrap();
            },
            "Test DHT",
            Some(Duration::from_secs(10)),
        );

        let res = send_and_receive(request.clone(), port).await.unwrap();

        assert_eq!(res.transaction_id, request.transaction_id);
        assert!(matches!(
            res.message_type,
            packets::MessageType::Response(packets::ResponseSpecific::GetPeersResponse(
                packets::GetPeersResponseArguments { .. }
            ))
        ));

        shutdown_tx.shutdown().await;

        Ok(())
    }

    #[tokio::test]
    async fn test_responds_to_find_node() -> Result<(), RustyDHTError> {
        let port = 1995;
        let (dht, mut shutdown_tx, shutdown_rx) = make_test_dht(port).await;
        shutdown::ShutdownReceiver::spawn_with_shutdown(
            shutdown_rx,
            async move {
                dht.run_event_loop().await.unwrap();
            },
            "Test DHT",
            Some(Duration::from_secs(10)),
        );

        let requester_id = Id::from_random(&mut thread_rng());
        let target = Id::from_random(&mut thread_rng());
        let request = MessageBuilder::new_find_node_request()
            .sender_id(requester_id)
            .target(target)
            .build()?;
        let res = send_and_receive(request.clone(), port).await.unwrap();

        assert_eq!(res.transaction_id, request.transaction_id);
        assert!(matches!(
            res.message_type,
            packets::MessageType::Response(packets::ResponseSpecific::FindNodeResponse(
                packets::FindNodeResponseArguments { .. }
            ))
        ));

        shutdown_tx.shutdown().await;

        Ok(())
    }

    #[tokio::test]
    async fn test_responds_to_announce_peer() -> Result<(), RustyDHTError> {
        let requester_id = Id::from_random(&mut thread_rng());
        let info_hash = Id::from_random(&mut thread_rng());
        let port = 2014;
        let (dht, mut shutdown_tx, shutdown_rx) = make_test_dht(port).await;
        shutdown::ShutdownReceiver::spawn_with_shutdown(
            shutdown_rx,
            async move {
                dht.run_event_loop().await.unwrap();
            },
            "Test DHT",
            Some(Duration::from_secs(10)),
        );

        // Send a get_peers request and get the response
        let reply = send_and_receive(
            MessageBuilder::new_get_peers_request()
                .sender_id(requester_id)
                .target(info_hash)
                .build()?,
            port,
        )
        .await
        .unwrap();

        // Extract the token from the get_peers response
        let token = {
            if let packets::MessageType::Response(packets::ResponseSpecific::GetPeersResponse(
                packets::GetPeersResponseArguments { token, .. },
            )) = reply.message_type
            {
                token
            } else {
                return Err(RustyDHTError::GeneralError(anyhow!("Didn't get token")));
            }
        };

        // Send an announce_peer request and get the response
        let reply = send_and_receive(
            MessageBuilder::new_announce_peer_request()
                .sender_id(requester_id)
                .target(info_hash)
                .port(1234)
                .token(token)
                .build()?,
            port,
        )
        .await
        .unwrap();

        // The response must be a ping response
        assert!(matches!(
            reply.message_type,
            packets::MessageType::Response(packets::ResponseSpecific::PingResponse(
                packets::PingResponseArguments { .. }
            ))
        ));

        // Send get peers again - this time we'll get a peer back (ourselves)
        let reply = send_and_receive(
            MessageBuilder::new_get_peers_request()
                .sender_id(requester_id)
                .target(info_hash)
                .build()?,
            port,
        )
        .await
        .unwrap();

        eprintln!("Received {:?}", reply);

        // Make sure we got a peer back
        let peers = {
            if let packets::MessageType::Response(packets::ResponseSpecific::GetPeersResponse(
                packets::GetPeersResponseArguments {
                    values: packets::GetPeersResponseValues::Peers(p),
                    ..
                },
            )) = reply.message_type
            {
                p
            } else {
                return Err(RustyDHTError::GeneralError(anyhow!("Didn't get peers")));
            }
        };
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].port(), 1234);
        eprintln!("all good!");
        shutdown_tx.shutdown().await;

        Ok(())
    }

    #[tokio::test]
    async fn test_responds_to_sample_infohashes() -> Result<(), RustyDHTError> {
        let requester_id = Id::from_random(&mut thread_rng());
        let target = Id::from_random(&mut thread_rng());
        let request = MessageBuilder::new_sample_infohashes_request()
            .sender_id(requester_id)
            .target(target)
            .build()?;

        let port = 2037;
        let (dht, mut shutdown_tx, shutdown_rx) = make_test_dht(port).await;
        shutdown::ShutdownReceiver::spawn_with_shutdown(
            shutdown_rx,
            async move {
                dht.run_event_loop().await.unwrap();
            },
            "Test DHT",
            Some(Duration::from_secs(10)),
        );

        let res = send_and_receive(request.clone(), port).await.unwrap();

        assert_eq!(res.transaction_id, request.transaction_id);
        assert!(matches!(
            res.message_type,
            packets::MessageType::Response(packets::ResponseSpecific::SampleInfoHashesResponse(
                packets::SampleInfoHashesResponseArguments { num: 0, .. }
            ))
        ));

        shutdown_tx.shutdown().await;

        Ok(())
    }

    #[tokio::test]
    async fn test_event_loop_pings_routers() {
        let (mut shutdown_tx, shutdown_rx) = shutdown::create_shutdown();
        let port1 = 2171;
        let dht1 = Arc::new(
            DHTBuilder::new()
                .initial_id(get_dht_id())
                .listen_addr(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port1))
                .ip_source(Box::new(StaticIPV4AddrSource::new(Ipv4Addr::new(
                    1, 2, 3, 4,
                ))))
                .settings(DHTSettingsBuilder::new().routers(vec![]).build())
                .build(shutdown_rx.clone())
                .unwrap(),
        );

        let dht2 = Arc::new(
            DHTBuilder::new()
                .initial_id(get_dht_id().make_mutant(4).unwrap())
                .listen_addr(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
                .ip_source(Box::new(StaticIPV4AddrSource::new(Ipv4Addr::new(
                    1, 2, 3, 4,
                ))))
                .settings(
                    DHTSettingsBuilder::new()
                        .router_ping_interval_secs(1)
                        .routers(vec![format!("127.0.0.1:{}", port1)])
                        .build(),
                )
                .build(shutdown_rx.clone())
                .unwrap(),
        );

        let mut receiver = dht2.subscribe();

        shutdown::ShutdownReceiver::spawn_with_shutdown(
            shutdown_rx.clone(),
            async move {
                dht1.run_event_loop().await.unwrap();
            },
            "DHT1",
            None,
        );

        let dht2_clone = dht2.clone();
        shutdown::ShutdownReceiver::spawn_with_shutdown(
            shutdown_rx,
            async move { dht2_clone.run_event_loop().await.unwrap() },
            "DHT2",
            None,
        );

        receiver.recv().await;
        let (unverified, verified) = dht2.state.lock().unwrap().buckets.count();

        // Must drop dht2 as it contains a ShutdownReceiver channel which will block shutdown
        drop(dht2);

        shutdown_tx.shutdown().await;
        assert_eq!(unverified, 0);
        assert_eq!(verified, 1);
    }

    #[tokio::test]
    async fn test_token_secret_rotation() {
        let ipv4 = Ipv4Addr::new(1, 2, 3, 4);
        let phony_ip4 = Box::new(StaticIPV4AddrSource::new(ipv4));
        let port = 2244;

        let dht = DHTBuilder::new()
            .initial_id(get_dht_id())
            .listen_addr(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
            .ip_source(phony_ip4)
            .settings(DHTSettingsBuilder::new().routers(vec![]).build())
            .build(shutdown::create_shutdown().1)
            .unwrap();

        assert_eq!(
            dht.state.lock().unwrap().token_secret.len(),
            DHTSettings::default().token_secret_size
        );

        dht.rotate_token_secrets();
        assert_eq!(
            dht.state.lock().unwrap().old_token_secret.len(),
            DHTSettings::default().token_secret_size
        );
        assert_eq!(
            dht.state.lock().unwrap().token_secret.len(),
            DHTSettings::default().token_secret_size
        );

        let state = dht.state.lock().unwrap();
        assert_ne!(state.old_token_secret, state.token_secret);
    }

    // Dumb helper function because we can't declare a const or static Id
    fn get_dht_id() -> Id {
        Id::from_hex("0011223344556677889900112233445566778899").unwrap()
    }

    // Helper function that sends a single packet to the test DHT and then returns the response
    async fn send_and_receive(
        msg: packets::Message,
        port: u16,
    ) -> Result<packets::Message, RustyDHTError> {
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        sock.send_to(
            &msg.clone().to_bytes().unwrap(),
            format!("127.0.0.1:{}", port),
        )
        .await
        .unwrap();
        let mut recv_buf = [0; 2048];
        let num_read = sock.recv_from(&mut recv_buf).await.unwrap().0;
        packets::Message::from_bytes(&recv_buf[..num_read])
    }
}
