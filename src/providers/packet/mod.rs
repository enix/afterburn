// Copyright 2017 CoreOS, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Metadata fetcher for Packet.net.
//!
//! Metadata JSON schema is described in their
//! [knowledge base](https://help.packet.net/article/37-metadata).

use std::collections::HashMap;
use std::fs::File;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::str::FromStr;

use anyhow::{anyhow, bail, Context, Result};
use openssh_keys::PublicKey;
use pnet_base::MacAddr;
use serde::Deserialize;
use slog_scope::warn;

use crate::network::{self, Interface, NetworkRoute};
use crate::providers::MetadataProvider;
use crate::retry;
use crate::util;

use ipnetwork::{self, IpNetwork, Ipv4Network, Ipv6Network};

#[cfg(test)]
mod mock_tests;

#[derive(Clone, Debug, Deserialize)]
#[allow(dead_code)]
struct PacketData {
    id: String,
    hostname: String,
    iqn: String,
    plan: String,
    facility: String,
    tags: Vec<String>,
    ssh_keys: Vec<String>,
    network: PacketNetworkInfo,

    error: Option<String>,
    phone_home_url: String,
}

#[derive(Clone, Debug, Deserialize)]
#[allow(dead_code)]
struct PacketNetworkInfo {
    interfaces: Vec<PacketInterfaceInfo>,
    addresses: Vec<PacketAddressInfo>,
    bonding: PacketBondingMode,
}

#[derive(Clone, Debug, Deserialize)]
#[allow(dead_code)]
struct PacketBondingMode {
    mode: u32,
}

#[derive(Clone, Debug, Deserialize)]
#[allow(dead_code)]
struct PacketInterfaceInfo {
    name: String,
    mac: String,
    bond: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[allow(dead_code)]
struct PacketAddressInfo {
    id: String,
    address_family: i32,
    public: bool,
    management: bool,
    address: IpAddr,
    netmask: IpAddr,
    gateway: IpAddr,
}

#[derive(Clone, Debug)]
pub struct PacketProvider {
    client: retry::Client,
    data: PacketData,
}

impl PacketProvider {
    /// Try to build a new provider client.
    ///
    /// This internally tries to fetch and cache the metadata content.
    pub fn try_new() -> Result<Self> {
        Self::fetch_content(None)
    }

    /// Fetch metadata content from Packet metadata endpoint.
    pub(crate) fn fetch_content(client: Option<retry::Client>) -> Result<Self> {
        let client = match client {
            Some(c) => c,
            None => retry::Client::try_new()?,
        };

        let data: PacketData = client
            .get(retry::Json, Self::endpoint_for("metadata"))
            .send()?
            .ok_or_else(|| anyhow!("metadata endpoint unreachable"))?;

        Ok(Self { client, data })
    }

    fn endpoint_for(name: &str) -> String {
        format!("https://metadata.packet.net/{name}")
    }

    fn get_attrs(&self) -> Vec<(String, String)> {
        let mut attrs = Vec::new();
        let mut v4_public_counter = 0;
        let mut v4_private_counter = 0;
        let mut v6_public_counter = 0;
        let mut v6_private_counter = 0;
        for a in self.data.network.addresses.clone() {
            match (a.address, a.public) {
                (IpAddr::V4(ip), true) => {
                    attrs.push((
                        format!("PACKET_IPV4_PUBLIC_{v4_public_counter}"),
                        format!("{ip}"),
                    ));
                    attrs.push((
                        format!("PACKET_IPV4_PUBLIC_GATEWAY_{v4_public_counter}"),
                        format!("{}", a.gateway),
                    ));
                    v4_public_counter += 1;
                }
                (IpAddr::V4(ip), false) => {
                    attrs.push((
                        format!("PACKET_IPV4_PRIVATE_{v4_private_counter}"),
                        format!("{ip}"),
                    ));
                    attrs.push((
                        format!("PACKET_IPV4_PRIVATE_GATEWAY_{v4_private_counter}"),
                        format!("{}", a.gateway),
                    ));
                    v4_private_counter += 1;
                }
                (IpAddr::V6(ip), true) => {
                    attrs.push((
                        format!("PACKET_IPV6_PUBLIC_{v6_public_counter}"),
                        format!("{ip}"),
                    ));
                    attrs.push((
                        format!("PACKET_IPV6_PUBLIC_GATEWAY_{v6_public_counter}"),
                        format!("{}", a.gateway),
                    ));
                    v6_public_counter += 1;
                }
                (IpAddr::V6(ip), false) => {
                    attrs.push((
                        format!("PACKET_IPV6_PRIVATE_{v6_private_counter}"),
                        format!("{ip}"),
                    ));
                    attrs.push((
                        format!("PACKET_IPV6_PRIVATE_GATEWAY_{v6_private_counter}"),
                        format!("{}", a.gateway),
                    ));
                    v6_private_counter += 1;
                }
            }
        }
        attrs.push(("PACKET_HOSTNAME".to_owned(), self.data.hostname.clone()));
        attrs.push((
            "PACKET_PHONE_HOME_URL".to_owned(),
            self.data.phone_home_url.clone(),
        ));
        attrs.push(("PACKET_PLAN".to_owned(), self.data.plan.clone()));
        attrs
    }

    fn get_dns_servers() -> Result<Vec<IpAddr>> {
        let f = File::open("/run/systemd/netif/state")
            .context("failed to open /run/systemd/netif/state")?;
        let ip_strings = util::key_lookup('=', "DNS", f)
            .context("failed to parse /run/systemd/netif/state")?
            .ok_or_else(|| anyhow!("DNS not found in netif state file"))?;
        let mut addrs = Vec::new();
        for ip_string in ip_strings.split(' ') {
            addrs.push(IpAddr::from_str(ip_string).context("failed to parse IP address")?);
        }
        if addrs.is_empty() {
            bail!("no DNS servers in /run/systemd/netif/state");
        }
        Ok(addrs)
    }

    fn parse_network(&self) -> Result<(Vec<Interface>, Vec<network::VirtualNetDev>)> {
        let netinfo = &self.data.network;
        let mut interfaces = Vec::new();
        let mut bonds = Vec::new();
        let dns_servers = PacketProvider::get_dns_servers()?;
        for i in netinfo.interfaces.clone() {
            let mac = MacAddr::from_str(&i.mac)
                .with_context(|| format!("failed to parse mac address: '{}'", i.mac))?;
            interfaces.push(Interface {
                mac_address: Some(mac),
                bond: i.bond.clone(),
                name: None,
                path: None,
                priority: 10,
                nameservers: Vec::new(),
                ip_addresses: Vec::new(),
                dhcp: None,
                routes: Vec::new(),
                // the interface should be unmanaged if it doesn't have a bond
                // section
                unmanaged: i.bond.is_none(),
                required_for_online: if i.bond.is_none() {
                    // use the default requirement
                    None
                } else {
                    // We care about the state of the bond interface and accept if any of the bonded
                    // interfaces are down. Actually the desired minimal state is "no-carrier" but
                    // systemd-networkd-wait-online does not work well with it currently, thus "no".
                    Some("no".to_owned())
                },
            });

            // if there is a bond key, make sure we have a bond device for it
            if let Some(ref bond_name) = i.bond {
                let bond = Interface {
                    name: Some(bond_name.clone()),
                    priority: 5,
                    nameservers: dns_servers.clone(),
                    mac_address: None,
                    path: None,
                    bond: None,
                    ip_addresses: Vec::new(),
                    dhcp: None,
                    routes: Vec::new(),
                    unmanaged: false,
                    required_for_online: Some("degraded-carrier".to_owned()),
                };
                if !bonds.iter().any(|(_, b): &(MacAddr, Interface)| &bond == b) {
                    bonds.push((mac, bond));
                }
            }
        }

        // According to the folks from packet, all the addresses given to us in the
        // network section should be attached to the first bond we find in the list
        // of interfaces. We should always have at least one bond listed, but if we
        // don't find any, we just print out a scary warning and don't attach the
        // addresses to anything.
        if let Some((_mac, ref mut first_bond)) = bonds.get_mut(0) {
            for a in netinfo.addresses.clone() {
                let prefix =
                    ipnetwork::ip_mask_to_prefix(a.netmask).context("invalid network mask")?;
                first_bond.ip_addresses.push(
                    IpNetwork::new(a.address, prefix).context("invalid IP address or prefix")?,
                );
                let dest = match (a.public, a.address) {
                    (false, IpAddr::V4(_)) => {
                        IpNetwork::V4(Ipv4Network::new(Ipv4Addr::new(10, 0, 0, 0), 8).unwrap())
                    }
                    (true, IpAddr::V4(_)) => {
                        IpNetwork::V4(Ipv4Network::new(Ipv4Addr::new(0, 0, 0, 0), 0).unwrap())
                    }
                    (_, IpAddr::V6(_)) => IpNetwork::V6(
                        Ipv6Network::new(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 0), 0).unwrap(),
                    ),
                };
                first_bond.routes.push(NetworkRoute {
                    destination: dest,
                    gateway: a.gateway,
                });
            }
        } else {
            warn!("no bond interfaces. addresses are left unassigned.");
            // the rest of the function operates on bonds, so just return
            return Ok((interfaces, vec![]));
        }

        let mut attrs = vec![
            ("TransmitHashPolicy".to_owned(), "layer3+4".to_owned()),
            ("MIIMonitorSec".to_owned(), ".1".to_owned()),
            ("UpDelaySec".to_owned(), ".2".to_owned()),
            ("DownDelaySec".to_owned(), ".2".to_owned()),
            (
                "Mode".to_owned(),
                network::bonding_mode_to_string(netinfo.bonding.mode)?,
            ),
        ];
        if netinfo.bonding.mode == network::BONDING_MODE_LACP {
            attrs.push(("LACPTransmitRate".to_owned(), "fast".to_owned()));
        }

        let mut network_devices = Vec::with_capacity(bonds.len());
        for (mac, bond) in bonds {
            let name = bond
                .name
                .clone()
                .ok_or_else(|| anyhow!("invalid bond interface: bond does not have a name"))?;
            let bond_netdev = network::VirtualNetDev {
                name,
                kind: network::NetDevKind::Bond,
                mac_address: mac,
                priority: Some(5),
                sd_netdev_sections: vec![network::SdSection {
                    name: "Bond".to_owned(),
                    attributes: attrs.clone(),
                }],
            };
            network_devices.push(bond_netdev);

            // finally, make sure the bond interfaces are in the interface list
            interfaces.push(bond)
        }

        // Create a fallback rule for all physical NICs that haven't been configured
        // because otherwise systemd-networkd-wait-online will wait for them and even if told
        // to only wait for bond0 this won't work with systemd 246 because the bond0 interface
        // never leaves the "configuring" phase when the other NICs are also still configuring.
        let fallback = Interface {
            path: Some("pci-*".to_owned()),
            unmanaged: true,
            priority: 80,
            name: None,
            mac_address: None,
            bond: None,
            nameservers: Vec::new(),
            ip_addresses: Vec::new(),
            dhcp: None,
            routes: Vec::new(),
            required_for_online: None,
        };
        interfaces.push(fallback);

        Ok((interfaces, network_devices))
    }
}

impl MetadataProvider for PacketProvider {
    fn attributes(&self) -> Result<HashMap<String, String>> {
        Ok(self.get_attrs().into_iter().collect())
    }

    fn hostname(&self) -> Result<Option<String>> {
        Ok(Some(self.data.hostname.clone()))
    }

    fn ssh_keys(&self) -> Result<Vec<PublicKey>> {
        let mut out = Vec::new();

        for key in &self.data.ssh_keys {
            let key = PublicKey::parse(key)?;
            out.push(key);
        }

        Ok(out)
    }

    fn networks(&self) -> Result<Vec<network::Interface>> {
        let (interfaces, _devices) = self.parse_network()?;

        Ok(interfaces)
    }

    fn virtual_network_devices(&self) -> Result<Vec<network::VirtualNetDev>> {
        let (_interfaces, devices) = self.parse_network()?;

        Ok(devices)
    }

    fn boot_checkin(&self) -> Result<()> {
        let url = self.data.phone_home_url.clone();
        self.client.post(retry::Json, url, None).dispatch_post()?;
        Ok(())
    }
}
