use rustconsole_protocol::wire::PhysicalLinkKind;
use std::net::IpAddr;
use windows::Win32::NetworkManagement::IpHelper::{GetBestInterfaceEx, GetIfEntry2, MIB_IF_ROW2};
use windows::Win32::Networking::WinSock::{
    AF_INET, AF_INET6, IN_ADDR, IN_ADDR_0, IN_ADDR_0_0, IN6_ADDR, IN6_ADDR_0, SOCKADDR,
    SOCKADDR_IN, SOCKADDR_IN6,
};

/// Inspect the route selected by Windows. For a Tailscale virtual route,
/// report the default internet route as context instead of mislabeling the
/// virtual adapter as a physical Ethernet connection.
pub fn host_link(peer: IpAddr) -> PhysicalLinkKind {
    let route = physical_route(peer);
    if route == PhysicalLinkKind::Other && is_tailscale_address(peer) {
        physical_route("1.1.1.1".parse().unwrap())
    } else {
        route
    }
}

fn is_tailscale_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => {
            let [first, second, ..] = address.octets();
            first == 100 && (64..128).contains(&second)
        }
        IpAddr::V6(address) => address.octets()[..6] == [0xfd, 0x7a, 0x11, 0x5c, 0xa1, 0xe0],
    }
}

fn physical_route(peer: IpAddr) -> PhysicalLinkKind {
    let mut interface_index = 0;
    let result = match peer {
        IpAddr::V4(address) => {
            let destination = SOCKADDR_IN {
                sin_family: AF_INET,
                sin_addr: IN_ADDR {
                    S_un: IN_ADDR_0 {
                        S_un_b: IN_ADDR_0_0 {
                            s_b1: address.octets()[0],
                            s_b2: address.octets()[1],
                            s_b3: address.octets()[2],
                            s_b4: address.octets()[3],
                        },
                    },
                },
                ..Default::default()
            };
            // SAFETY: destination and output index remain valid during this call.
            unsafe {
                GetBestInterfaceEx(
                    &destination as *const _ as *const SOCKADDR,
                    &mut interface_index,
                )
            }
        }
        IpAddr::V6(address) => {
            let destination = SOCKADDR_IN6 {
                sin6_family: AF_INET6,
                sin6_addr: IN6_ADDR {
                    u: IN6_ADDR_0 {
                        Byte: address.octets(),
                    },
                },
                ..Default::default()
            };
            // SAFETY: destination and output index remain valid during this call.
            unsafe {
                GetBestInterfaceEx(
                    &destination as *const _ as *const SOCKADDR,
                    &mut interface_index,
                )
            }
        }
    };
    if result != 0 || interface_index == 0 {
        return PhysicalLinkKind::Unknown;
    }
    let mut row = MIB_IF_ROW2 {
        InterfaceIndex: interface_index,
        ..Default::default()
    };
    // SAFETY: the row is initialized with the interface index and writable.
    if unsafe { GetIfEntry2(&mut row) }.0 != 0 {
        return PhysicalLinkKind::Unknown;
    }
    match row.Type {
        6 => PhysicalLinkKind::Ethernet, // IF_TYPE_ETHERNET_CSMACD
        71 => PhysicalLinkKind::Wifi,    // IF_TYPE_IEEE80211
        _ => PhysicalLinkKind::Other,
    }
}
