use std::{
    io::{Read, Write},
    net::TcpStream,
};

use network::NetworkMessage;

mod network;

fn main() {
    match TcpStream::connect("localhost:9933") {
        Ok(mut stream) => {
            println!("Successfully connected to server in port 3333");

            let message =
                NetworkMessage::new("command", [0xFB, 0xC0, 0xB6, 0xDB].to_vec()).unwrap();

            stream.write_all(&message.to_bytes()).unwrap();
            println!("Sent Hello, awaiting reply...");

            let mut data = [0_u8; 1024]; // using 6 byte buffer
            match stream.read(&mut data) {
                Ok(xd) => {
                    println!("Unexpected reply: {}", xd);
                }
                Err(e) => {
                    println!("Failed to receive data: {}", e);
                }
            }
        }
        Err(e) => {
            println!("Failed to connect: {}", e);
        }
    }
    println!("Terminated.");
}
