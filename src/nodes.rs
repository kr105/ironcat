use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    str::FromStr,
    sync::Arc,
};
use tokio::{
    io::{self, AsyncWriteExt},
    net::TcpStream,
    sync::Mutex,
};

use crate::network::{
    message_version::MessageVersion, NetworkAddress, NetworkMessage, NetworkQueue, ServiceMask,
};

#[derive(Debug, Hash, PartialEq, Eq)]
struct NodeEndpoint {
    address: IpAddr,
    port: u16,
}

struct Node {
    address: NetworkAddress,
    /// Unix timestamp
    last_seen: u64,
    connected: bool,
}

/// Manages a collection of nodes in the network
pub struct NodeManager {
    nodes: HashMap<NodeEndpoint, Node>,
}

impl NodeManager {
    pub fn new() -> Self {
        NodeManager {
            nodes: HashMap::new(),
        }
    }

    /// Inserts a new node into the manager if it doesn't already exist
    ///
    /// Returns true if the node was inserted, false if it already existed
    pub fn insert(&mut self, address: IpAddr, port: u16) -> bool {
        let endpoint = NodeEndpoint { address, port };

        if self.nodes.contains_key(&endpoint) {
            return false;
        }

        let addr = NetworkAddress {
            address: endpoint.address,
            port: endpoint.port,
            services: ServiceMask::empty(),
        };

        let node = Node {
            address: addr,
            last_seen: 0,
            connected: false,
        };

        self.nodes.insert(endpoint, node);

        true
    }
}

pub async fn insert_node(node_manager: Arc<Mutex<NodeManager>>, ip: &str, port: u16) {
    let address = IpAddr::from_str(ip).expect("Invalid IP address provided");

    let inserted = {
        let mut node_manager_lock = node_manager.lock().await;
        node_manager_lock.insert(address, port)
    };

    if !inserted {
        return;
    }

    tokio::spawn(handle_node_connection(address, port));
}

async fn handle_node_connection(address: IpAddr, port: u16) {
    let socket_addr = SocketAddr::new(address, port);

    let mut tcp_stream = match TcpStream::connect(socket_addr).await {
        Ok(stream) => {
            println!("Connected to {}", socket_addr);

            stream
        }
        Err(e) => {
            println!("Failed to connect to {}: {}", socket_addr, e);
            return;
        }
    };

    // Once the connection is established, the first step is to present ourselves
    // by sending a MessageVersion packet

    let network_address = NetworkAddress::new(address, port);
    let version = MessageVersion::new(network_address);
    let packet = NetworkMessage::new("version", version.to_bytes()).unwrap();

    tcp_stream.write_all(&packet.to_bytes()).await.unwrap();

    let mut incoming_queue = NetworkQueue::new();

    loop {
        // Wait for the socket to be readable
        if tcp_stream.readable().await.is_err() {
            println!("Error in waiting readable()");
            break;
        }

        // Creating the buffer **after** the `await` prevents it from
        // being stored in the async task
        let mut buf = [0; 4096];

        // Try to read data, this may still fail with `WouldBlock`
        // if the readiness event is a false positive
        match tcp_stream.try_read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                incoming_queue.process_incoming_data(&buf[..n]).unwrap();
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                continue;
            }
            Err(e) => {
                println!("Error in try_read(): {:?}", e);
                break;
            }
        }

        while let Some(message) = incoming_queue.get_next_message() {
            println!(
                "Mensaje recibido: {:?}",
                String::from_utf8(message.command.to_vec())
            );
        }
    }

    println!("Exiting handle_node_connection for {}", socket_addr);
}
