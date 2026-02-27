// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::unwrap_used)]

use ironcat::utils::is_routable;

#[test]
fn is_routable_rejects_private() {
	assert!(!is_routable("10.0.0.1".parse().unwrap()));
	assert!(!is_routable("172.16.0.1".parse().unwrap()));
	assert!(!is_routable("192.168.1.1".parse().unwrap()));
}

#[test]
fn is_routable_rejects_loopback() {
	assert!(!is_routable("127.0.0.1".parse().unwrap()));
	assert!(!is_routable("::1".parse().unwrap()));
}

#[test]
fn is_routable_rejects_link_local() {
	assert!(!is_routable("169.254.1.1".parse().unwrap()));
}

#[test]
fn is_routable_accepts_public() {
	assert!(is_routable("8.8.8.8".parse().unwrap()));
	assert!(is_routable("1.1.1.1".parse().unwrap()));
}

#[test]
fn is_routable_rejects_unspecified() {
	assert!(!is_routable("0.0.0.0".parse().unwrap()));
	assert!(!is_routable("::".parse().unwrap()));
}
