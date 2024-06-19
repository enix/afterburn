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

//! network abstracts away the manipulation of network device and
//! interface unit files. All that is left is to write the resulting string to
//! the necessary unit.

use anyhow::{anyhow, bail, Context, Result};
use ipnetwork::IpNetwork;
use pnet_base::MacAddr;
use std::fmt::Write;
use std::net::IpAddr;
use std::string::String;
use std::string::ToString;

pub const BONDING_MODE_BALANCE_RR: u32 = 0;
pub const BONDING_MODE_ACTIVE_BACKUP: u32 = 1;
pub const BONDING_MODE_BALANCE_XOR: u32 = 2;
pub const BONDING_MODE_BROADCAST: u32 = 3;
pub const BONDING_MODE_LACP: u32 = 4;
pub const BONDING_MODE_BALANCE_TLB: u32 = 5;
pub const BONDING_MODE_BALANCE_ALB: u32 = 6;

const BONDING_MODES: [(u32, &str); 7] = [
    (BONDING_MODE_BALANCE_RR, "balance-rr"),
    (BONDING_MODE_ACTIVE_BACKUP, "active-backup"),
    (BONDING_MODE_BALANCE_XOR, "balance-xor"),
    (BONDING_MODE_BROADCAST, "broadcast"),
    (BONDING_MODE_LACP, "802.3ad"),
    (BONDING_MODE_BALANCE_TLB, "balance-tlb"),
    (BONDING_MODE_BALANCE_ALB, "balance-alb"),
];

pub fn bonding_mode_to_string(mode: u32) -> Result<String> {
    for &(m, s) in &BONDING_MODES {
        if m == mode {
            return Ok(s.to_owned());
        }
    }
    Err(anyhow!("no such bonding mode: {}", mode))
}

/// Try to parse an IP+netmask pair into a CIDR network.
pub fn try_parse_cidr(address: IpAddr, netmask: IpAddr) -> Result<IpNetwork> {
    let prefix = ipnetwork::ip_mask_to_prefix(netmask)?;
    IpNetwork::new(address, prefix).context("failed to parse network")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NetworkRoute {
    pub destination: IpNetwork,
    pub gateway: IpAddr,
}

/// A network interface/link.
///
/// Depending on platforms, an interface may be identified by
/// name or by MAC address (at least one of those must be provided).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Interface {
    /// Interface name.
    pub name: Option<String>,
    /// Interface MAC address.
    pub mac_address: Option<MacAddr>,
    /// Path as identifier
    pub path: Option<String>,
    /// Relative priority for interface configuration.
    pub priority: u8,
    pub nameservers: Vec<IpAddr>,
    pub ip_addresses: Vec<IpNetwork>,
    // Optionally enable DHCP
    pub dhcp: Option<DhcpSetting>,
    pub routes: Vec<NetworkRoute>,
    pub bond: Option<String>,
    pub unmanaged: bool,
    /// Optional requirement setting instead of the default
    pub required_for_online: Option<String>,
}

/// A virtual network interface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VirtualNetDev {
    pub name: String,
    pub kind: NetDevKind,
    pub mac_address: MacAddr,
    pub priority: Option<u32>,
    pub sd_netdev_sections: Vec<SdSection>,
}

/// A free-form `systemd.netdev` section.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SdSection {
    pub name: String,
    pub attributes: Vec<(String, String)>,
}

/// Supported virtual network device kinds.
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NetDevKind {
    /// Parent aggregation for physically bonded devices.
    Bond,
    /// VLAN child interface for a physical device with 802.1Q.
    Vlan,
}

impl NetDevKind {
    /// Return device kind according to `systemd.netdev`.
    ///
    /// See [systemd documentation](kinds) for the full list.
    ///
    /// kinds: https://www.freedesktop.org/software/systemd/man/systemd.netdev.html#Supported%20netdev%20kinds
    fn sd_netdev_kind(&self) -> String {
        let kind = match *self {
            NetDevKind::Bond => "bond",
            NetDevKind::Vlan => "vlan",
        };
        kind.to_string()
    }
}

/// Optional use of DHCP.
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DhcpSetting {
    Both,
    V4,
    V6,
}

impl DhcpSetting {
    /// Return DHCP setting according to `systemd.network`
    ///
    /// See [systemd documentation](dhcp) for the full list.
    ///
    /// dhcp: https://www.freedesktop.org/software/systemd/man/latest/systemd.network.html#DHCP=
    fn sd_dhcp_setting(&self) -> String {
        let setting = match *self {
            DhcpSetting::Both => "yes",
            DhcpSetting::V4 => "ipv4",
            DhcpSetting::V6 => "ipv6",
        };
        setting.to_string()
    }
}

impl Interface {
    /// Return a deterministic `systemd.network` unit name for this device.
    pub fn sd_network_unit_name(&self) -> Result<String> {
        let iface_name = match (&self.name, &self.mac_address, &self.path) {
            (Some(ref name), _, _) => name.clone(),
            (None, Some(ref addr), _) => addr.to_string(),
            (None, None, Some(ref path)) => path.to_string(),
            (None, None, None) => bail!("network interface without name, MAC address, or path"),
        };
        let unit_name = format!("{:02}-{}.network", self.priority, iface_name);
        Ok(unit_name)
    }

    pub fn config(&self) -> String {
        let mut config = String::new();

        // [Match] section
        writeln!(config, "[Match]").unwrap();
        if let Some(name) = self.name.clone() {
            writeln!(config, "Name={name}").unwrap();
        }
        if let Some(mac) = self.mac_address {
            writeln!(config, "MACAddress={mac}").unwrap();
        }
        if let Some(path) = &self.path {
            writeln!(config, "Path={path}").unwrap();
        }

        // [Network] section
        writeln!(config, "\n[Network]").unwrap();
        if let Some(dhcp) = &self.dhcp {
            writeln!(config, "DHCP={}", dhcp.sd_dhcp_setting()).unwrap();
        }
        for ns in &self.nameservers {
            writeln!(config, "DNS={ns}").unwrap()
        }
        if let Some(bond) = self.bond.clone() {
            writeln!(config, "Bond={bond}").unwrap();
        }

        // [Link] section
        if self.unmanaged || self.required_for_online.is_some() {
            writeln!(config, "\n[Link]").unwrap();
        }
        if self.unmanaged {
            writeln!(config, "Unmanaged=yes").unwrap();
        }
        if let Some(operational_state) = &self.required_for_online {
            writeln!(config, "RequiredForOnline={operational_state}").unwrap();
        }

        // [Address] sections
        for addr in &self.ip_addresses {
            writeln!(config, "\n[Address]\nAddress={addr}").unwrap();
        }

        // [Route] sections
        for route in &self.routes {
            writeln!(
                config,
                "\n[Route]\nDestination={}\nGateway={}",
                route.destination, route.gateway
            )
            .unwrap();
        }

        config
    }
}

impl VirtualNetDev {
    /// Return a deterministic netdev unit name for this device.
    pub fn netdev_unit_name(&self) -> String {
        format!("{:02}-{}.netdev", self.priority.unwrap_or(10), self.name)
    }

    /// Return the `systemd.netdev` configuration fragment for this device.
    pub fn sd_netdev_config(&self) -> String {
        let mut config = String::new();

        // [NetDev] section
        writeln!(config, "[NetDev]").unwrap();
        writeln!(config, "Name={}", self.name).unwrap();
        writeln!(config, "Kind={}", self.kind.sd_netdev_kind()).unwrap();
        writeln!(config, "MACAddress={}", self.mac_address).unwrap();

        // Custom sections.
        for section in &self.sd_netdev_sections {
            writeln!(config, "\n[{}]", section.name).unwrap();
            for attr in &section.attributes {
                writeln!(config, "{}={}", attr.0, attr.1).unwrap();
            }
        }

        config
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ipnetwork::{Ipv4Network, Ipv6Network};
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn mac_addr_display() {
        let m = MacAddr(0xf4, 0x00, 0x34, 0x09, 0x73, 0xee);
        assert_eq!(m.to_string(), "f4:00:34:09:73:ee");
    }

    #[test]
    fn interface_unit_name() {
        let cases = vec![
            (
                Interface {
                    name: Some(String::from("lo")),
                    mac_address: Some(MacAddr(0, 0, 0, 0, 0, 0)),
                    path: None,
                    priority: 20,
                    nameservers: vec![],
                    ip_addresses: vec![],
                    dhcp: None,
                    routes: vec![],
                    bond: None,
                    unmanaged: false,
                    required_for_online: None,
                },
                "20-lo.network",
            ),
            (
                Interface {
                    name: Some(String::from("lo")),
                    mac_address: Some(MacAddr(0, 0, 0, 0, 0, 0)),
                    path: None,
                    priority: 10,
                    nameservers: vec![],
                    ip_addresses: vec![],
                    dhcp: None,
                    routes: vec![],
                    bond: None,
                    unmanaged: false,
                    required_for_online: None,
                },
                "10-lo.network",
            ),
            (
                Interface {
                    name: None,
                    mac_address: Some(MacAddr(0, 0, 0, 0, 0, 0)),
                    path: None,
                    priority: 20,
                    nameservers: vec![],
                    ip_addresses: vec![],
                    dhcp: None,
                    routes: vec![],
                    bond: None,
                    unmanaged: false,
                    required_for_online: None,
                },
                "20-00:00:00:00:00:00.network",
            ),
            (
                Interface {
                    name: Some(String::from("lo")),
                    mac_address: None,
                    path: None,
                    priority: 20,
                    nameservers: vec![],
                    ip_addresses: vec![],
                    dhcp: None,
                    routes: vec![],
                    bond: None,
                    unmanaged: false,
                    required_for_online: None,
                },
                "20-lo.network",
            ),
            (
                Interface {
                    name: None,
                    mac_address: None,
                    path: Some("pci-*".to_owned()),
                    priority: 20,
                    nameservers: vec![],
                    ip_addresses: vec![],
                    dhcp: None,
                    routes: vec![],
                    bond: None,
                    unmanaged: false,
                    required_for_online: None,
                },
                "20-pci-*.network",
            ),
        ];

        for (iface, expected) in cases {
            let unit_name = iface.sd_network_unit_name().unwrap();
            assert_eq!(unit_name, expected);
        }
    }

    #[test]
    fn interface_unit_name_no_name_no_mac() {
        let i = Interface {
            name: None,
            mac_address: None,
            path: None,
            priority: 20,
            nameservers: vec![],
            ip_addresses: vec![],
            dhcp: None,
            routes: vec![],
            bond: None,
            unmanaged: false,
            required_for_online: None,
        };
        i.sd_network_unit_name().unwrap_err();
    }

    #[test]
    fn virtual_netdev_unit_name() {
        let ds = vec![
            (
                VirtualNetDev {
                    name: String::from("vlan0"),
                    kind: NetDevKind::Vlan,
                    mac_address: MacAddr(0, 0, 0, 0, 0, 0),
                    priority: Some(20),
                    sd_netdev_sections: vec![],
                },
                "20-vlan0.netdev",
            ),
            (
                VirtualNetDev {
                    name: String::from("vlan0"),
                    kind: NetDevKind::Vlan,
                    mac_address: MacAddr(0, 0, 0, 0, 0, 0),
                    priority: None,
                    sd_netdev_sections: vec![],
                },
                "10-vlan0.netdev",
            ),
        ];

        for (d, s) in ds {
            assert_eq!(d.netdev_unit_name(), s);
        }
    }

    #[test]
    fn interface_config() {
        let is = vec![
            (
                Interface {
                    name: Some(String::from("lo")),
                    mac_address: Some(MacAddr(0, 0, 0, 0, 0, 0)),
                    path: None,
                    priority: 20,
                    nameservers: vec![
                        IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
                        IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 1)),
                    ],
                    ip_addresses: vec![
                        IpNetwork::V4(Ipv4Network::new(Ipv4Addr::new(127, 0, 0, 1), 8).unwrap()),
                        IpNetwork::V6(
                            Ipv6Network::new(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 1), 128).unwrap(),
                        ),
                    ],
                    dhcp: None,
                    routes: vec![NetworkRoute {
                        destination: IpNetwork::V4(
                            Ipv4Network::new(Ipv4Addr::new(127, 0, 0, 1), 8).unwrap(),
                        ),
                        gateway: IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
                    }],
                    bond: Some(String::from("james")),
                    unmanaged: false,
                    required_for_online: None,
                },
                "[Match]
Name=lo
MACAddress=00:00:00:00:00:00

[Network]
DNS=127.0.0.1
DNS=::1
Bond=james

[Address]
Address=127.0.0.1/8

[Address]
Address=::1/128

[Route]
Destination=127.0.0.1/8
Gateway=127.0.0.1
",
            ),
            // this isn't really a valid interface object, but it's testing
            // the minimum possible configuration for all peices at the same
            // time, so I'll allow it. (sdemos)
            (
                Interface {
                    name: None,
                    mac_address: None,
                    path: None,
                    priority: 10,
                    nameservers: vec![],
                    ip_addresses: vec![],
                    dhcp: None,
                    routes: vec![],
                    bond: None,
                    unmanaged: false,
                    required_for_online: None,
                },
                "[Match]

[Network]
",
            ),
            // test the path and required_for_online settings
            (
                Interface {
                    name: None,
                    mac_address: None,
                    path: Some("pci-*".to_owned()),
                    priority: 10,
                    nameservers: vec![],
                    ip_addresses: vec![],
                    dhcp: None,
                    routes: vec![],
                    bond: None,
                    unmanaged: false,
                    required_for_online: Some("no".to_owned()),
                },
                "[Match]
Path=pci-*

[Network]

[Link]
RequiredForOnline=no
",
            ),
            // test the unmanaged setting
            (
                Interface {
                    name: Some("*".to_owned()),
                    mac_address: None,
                    path: None,
                    priority: 10,
                    nameservers: vec![],
                    ip_addresses: vec![],
                    dhcp: None,
                    routes: vec![],
                    bond: None,
                    unmanaged: true,
                    required_for_online: None,
                },
                "[Match]
Name=*

[Network]

[Link]
Unmanaged=yes
",
            ),
            // test the DHCP setting
            (
                Interface {
                    name: Some("*".to_owned()),
                    mac_address: None,
                    path: None,
                    priority: 10,
                    nameservers: vec![],
                    ip_addresses: vec![],
                    dhcp: Some(DhcpSetting::V4),
                    routes: vec![],
                    bond: None,
                    unmanaged: false,
                    required_for_online: None,
                },
                "[Match]
Name=*

[Network]
DHCP=ipv4
",
            ),
        ];

        for (i, s) in is {
            assert_eq!(i.config(), s);
        }
    }

    #[test]
    fn virtual_netdev_config() {
        let ds = vec![
            (
                VirtualNetDev {
                    name: String::from("vlan0"),
                    kind: NetDevKind::Vlan,
                    mac_address: MacAddr(0, 0, 0, 0, 0, 0),
                    priority: Some(20),
                    sd_netdev_sections: vec![
                        SdSection {
                            name: String::from("Test"),
                            attributes: vec![
                                (String::from("foo"), String::from("bar")),
                                (String::from("oingo"), String::from("boingo")),
                            ],
                        },
                        SdSection {
                            name: String::from("Empty"),
                            attributes: vec![],
                        },
                    ],
                },
                "[NetDev]
Name=vlan0
Kind=vlan
MACAddress=00:00:00:00:00:00

[Test]
foo=bar
oingo=boingo

[Empty]
",
            ),
            (
                VirtualNetDev {
                    name: String::from("vlan0"),
                    kind: NetDevKind::Vlan,
                    mac_address: MacAddr(0, 0, 0, 0, 0, 0),
                    priority: Some(20),
                    sd_netdev_sections: vec![],
                },
                "[NetDev]
Name=vlan0
Kind=vlan
MACAddress=00:00:00:00:00:00
",
            ),
        ];

        for (d, s) in ds {
            assert_eq!(d.sd_netdev_config(), s);
        }
    }
}
