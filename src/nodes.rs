// SPDX-License-Identifier: Apache-2.0

use anyhow::{anyhow, Result};
use dashmap::DashMap;
use std::{
    ffi::CStr,
    net::IpAddr,
    str::FromStr,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{self, AsyncWriteExt},
    net::TcpStream,
};

use crate::network::{
    message_version::MessageVersion, Message, NetworkAddress, NetworkCommand, NetworkQueue,
    ServiceMask,
};

/// Represents a unique identifier for a node in the network
#[derive(Debug, Hash, PartialEq, Eq, Clone)]
pub struct NodeEndpoint {
    address: IpAddr,
    port: u16,
}

/// Represents a node in the Catcoin network
struct Node {
    endpoint: NodeEndpoint,

    /// Node has sent the verack message
    ver_ack: bool,

    /// Unix timestamp
    last_seen: u64,

    connected: bool,

    version: u32,

    services: ServiceMask,

    timestamp: i64,

    user_agent: String,

    height: i32,

    relay: bool,

    /// Set if the node does something that shouldn't
    not_good: bool,
}

/// Manages a collection of nodes in the network
pub struct NodeManager {
    // Using DashMap for concurrent access without needing explicit locking
    nodes: DashMap<NodeEndpoint, Node>,
}

impl NodeManager {
    pub fn new() -> Self {
        NodeManager {
            nodes: DashMap::new(),
        }
    }

    /// Inserts a new node into the manager if it doesn't already exist
    ///
    /// Returns true if the node was inserted, false if it already existed
    pub fn insert(&self, address: IpAddr, port: u16) -> bool {
        let endpoint = NodeEndpoint { address, port };

        // Check if the node already exists
        if self.nodes.contains_key(&endpoint) {
            return false;
        }

        // Create a new Node with default values
        let node = Node {
            endpoint: endpoint.clone(),
            ver_ack: false,
            last_seen: 0,
            connected: false,
            height: 0,
            relay: false,
            services: ServiceMask::empty(),
            timestamp: 0,
            user_agent: String::new(),
            version: 0,
            not_good: false,
        };

        self.nodes.insert(endpoint, node);

        true
    }

    /// Updates the last_seen timestamp for a node with the current time
    pub fn update_last_seen(&self, node_endpoint: &NodeEndpoint) {
        if let Some(mut node) = self.nodes.get_mut(node_endpoint) {
            node.last_seen = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("Time went backwards")
                .as_secs();
        }
    }
}

/// Inserts a new node into the NodeManager and spawns a task to handle the connection
pub async fn insert_node(node_manager: Arc<NodeManager>, ip: &str, port: u16) {
    let address = IpAddr::from_str(ip).expect("Invalid IP address provided");

    let inserted = node_manager.insert(address, port);

    if !inserted {
        return; // Node already exists, no need to proceed
    }

    let node_manager_clone = Arc::clone(&node_manager);
    tokio::spawn(async move {
        handle_node_connection(node_manager_clone, address, port).await;
    });
}

/// Handles the connection to a node
async fn handle_node_connection(node_manager: Arc<NodeManager>, address: IpAddr, port: u16) {
    let node_endpoint = NodeEndpoint { address, port };

    // Attempt to establish a TCP connection
    let mut tcp_stream = match TcpStream::connect((address, port)).await {
        Ok(stream) => {
            println!("Connected to {:?}", node_endpoint);

            stream
        }
        Err(e) => {
            println!("Failed to connect to {:?}: {}", node_endpoint, e);
            return;
        }
    };

    // Once the connection is established, the first step is to present ourselves
    // by sending a MessageVersion packet

    let network_address = NetworkAddress::new(address, port);
    let version = MessageVersion::new(network_address);
    let packet = Message::new("version", version.to_bytes()).unwrap();

    tcp_stream.write_all(&packet.to_bytes()).await.unwrap();

    let mut incoming_queue = NetworkQueue::new();

    'main: loop {
        // Wait for the socket to be readable
        if tcp_stream.readable().await.is_err() {
            println!("Error in waiting readable()");
            break;
        }

        let mut buf = [0; 4096];

        match tcp_stream.try_read(&mut buf) {
            Ok(0) => break, // Connection closed
            Ok(n) => {
                // Process the incoming data
                incoming_queue.process_incoming_data(&buf[..n]).unwrap();
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                continue; // No data available, try again
            }
            Err(e) => {
                println!("Error in try_read(): {:?}", e);
                break;
            }
        }

        // Process all messages in the queue
        while let Some(message) = incoming_queue.get_next_message() {
            node_manager.update_last_seen(&node_endpoint);

            if let Err(e) = parse_incoming_message(
                Arc::clone(&node_manager),
                &node_endpoint,
                &mut tcp_stream,
                message,
            )
            .await
            {
                println!("Error parsing message: {:?}", e);

                // Close the connection
                tcp_stream.shutdown().await.unwrap(); // TODO: Check safety
                break 'main;
            }
        }
    }

    println!("Exiting handle_node_connection for {:?}", node_endpoint);
}

/// Parses and handles an incoming message from a node
async fn parse_incoming_message(
    node_manager: Arc<NodeManager>,
    node_endpoint: &NodeEndpoint,
    tcp_stream: &mut TcpStream,
    message: Message,
) -> Result<()> {
    // Extract the command from the message
    let command = match CStr::from_bytes_until_nul(&message.command) {
        Ok(str) => str.to_str().unwrap(),
        Err(_) => {
            return Err(anyhow!(
                "Error parsing incoming message, 'command' field is malformed"
            ))
        }
    };

    let command = NetworkCommand::from_str(command).unwrap();

    println!("Received message: {:?}", command);

    match command {
        NetworkCommand::Version => {
            let mut node = node_manager.nodes.get_mut(node_endpoint).unwrap();

            // Nodes can send only one version command
            if node.version > 0 {
                node.not_good = true;

                return Err(anyhow!("Node sent version command twice"));
            }

            let version = MessageVersion::from_bytes(&message.payload).unwrap();

            // Save node data
            node.endpoint = node_endpoint.clone();
            node.services = version.services;
            node.timestamp = version.timestamp;
            node.user_agent = version.user_agent;
            node.height = version.start_height;
            node.relay = version.relay;

            // We must answer with a verack
            let packet = Message::new("verack", Vec::new()).unwrap();
            tcp_stream.write_all(&packet.to_bytes()).await.unwrap();
        }

        NetworkCommand::Verack => {
            let mut node = node_manager.nodes.get_mut(node_endpoint).unwrap();
            node.ver_ack = true;

            // If we already received the version command from the node
            // and the node already received the version from us (with this verack),
            // then the connection is ready
            if node.version > 0 {
                node.connected = true;
            }
        }

        NetworkCommand::Unknown(str) => {
            println!("Received unknown network command: {}", str);
        }
        _ => {
            println!("Received unhandled message: {:?}", command);
        }
    };

    Ok(())
}
