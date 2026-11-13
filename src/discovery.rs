use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};

use anyhow::Context;
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use tokio::sync::watch;

/// The service type that Sendspin servers advertise for connections that a
/// client starts ("Client Initiated Connections" in the Sendspin spec).
/// Clients advertise `_sendspin._tcp.local.` instead, so that servers can
/// dial them.
const SERVICE_TYPE: &str = "_sendspin-server._tcp.local.";

/// The service type that Music Assistant advertises. `base_url` in its TXT
/// record is the web UI. The Sendspin messages and TXT records name no web
/// UI, so this record is the only source.
const MUSIC_ASSISTANT_TYPE: &str = "_mass._tcp.local.";

/// One discovered Sendspin server on the network.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Server {
    pub label: String,
    pub url: String,
}

/// Maps a discovered service onto a `Server`. `Err` means the service cannot be
/// used, for example it has no usable address or the TXT record lacks a path.
fn server_from_service(info: &ServiceInfo) -> anyhow::Result<Server> {
    // The spec says to skip loopback and unspecified addresses. `IpAddr` sorts
    // every IPv4 address before every IPv6 address, so `min` prefers IPv4 and
    // picks the same address on each resolve.
    // ponytail: link-local IPv6 needs a zone ID, which mdns-sd 0.14 does not
    // report and the url crate rejects. Accept it once both carry zone IDs.
    let address = info
        .get_addresses()
        .iter()
        .copied()
        .filter(|ip| {
            !ip.is_loopback()
                && !ip.is_unspecified()
                && !matches!(ip, IpAddr::V6(v6) if v6.is_unicast_link_local())
        })
        .min()
        .ok_or_else(|| anyhow::anyhow!("no usable address for {}", info.get_fullname()))?;
    let path = info
        .get_property_val_str("path")
        .filter(|path| path.starts_with('/'))
        .ok_or_else(|| anyhow::anyhow!("no path in TXT record for {}", info.get_fullname()))?;
    let name = info.get_property_val_str("name").unwrap_or_default();
    let label = if name.is_empty() {
        instance_name(info.get_fullname())
    } else {
        name.to_string()
    };
    // The spec requires plain ws:// for advertised addresses. The TXT record
    // has no key for TLS.
    let url = format!("ws://{}{path}", SocketAddr::new(address, info.get_port()));
    // A path with a fragment fails the URL check, so spun could not switch
    // to the server.
    crate::config::check_server_url(&url)
        .with_context(|| format!("unusable URL for {}", info.get_fullname()))?;
    Ok(Server { label, url })
}

/// `MyServer._sendspin-server._tcp.local.` -> `MyServer`.
fn instance_name(fullname: &str) -> String {
    fullname.split('.').next().unwrap_or(fullname).to_string()
}

/// The web UI of a discovered Music Assistant service, from `base_url` in
/// its TXT record. `Err` means that the record has no `http://` or
/// `https://` URL.
fn web_ui_from_service(info: &ServiceInfo) -> anyhow::Result<String> {
    // The browser opens this URL. A file URL or another scheme can start
    // other programs, so only http and https pass.
    info.get_property_val_str("base_url")
        .and_then(|url| url.parse::<http::Uri>().ok())
        .filter(|url| matches!(url.scheme_str(), Some("http" | "https")))
        .map(|url| url.to_string())
        .ok_or_else(|| anyhow::anyhow!("no web UI in TXT record for {}", info.get_fullname()))
}

/// The web UI for the Sendspin server at `server_url`. Music Assistant
/// advertises both on the same host, so this is the web UI on the host of
/// `server_url`. A `base_url` that names a reverse proxy has another host.
/// Then a lone Music Assistant server on the LAN is the best guess.
// ponytail: compares the host text. Two spellings of one IPv6 address do not
// match, and then only a lone server is found. Parse the hosts as IP
// addresses if that matters.
pub fn web_ui_url<'a>(server_url: &str, web_uis: &'a [String]) -> Option<&'a String> {
    let host = |url: &str| {
        let url: http::Uri = url.parse().ok()?;
        url.host().map(str::to_ascii_lowercase)
    };
    let server = host(server_url)?;
    let same_host = web_uis
        .iter()
        .find(|web_ui| host(web_ui).as_ref() == Some(&server));
    let lone = match web_uis {
        [web_ui] => Some(web_ui),
        _ => None,
    };
    same_host.or(lone)
}

/// Browses the LAN for Sendspin servers and Music Assistant web UIs, and
/// publishes each current list into its channel. Without multicast, spun
/// logs a warning and runs without discovery.
pub fn spawn(servers: watch::Sender<Vec<Server>>, web_uis: watch::Sender<Vec<String>>) {
    let browsing = ServiceDaemon::new().and_then(|daemon| {
        browse(&daemon, SERVICE_TYPE, server_from_service, servers)?;
        browse(&daemon, MUSIC_ASSISTANT_TYPE, web_ui_from_service, web_uis)
    });
    if let Err(error) = browsing {
        log::warn!("mDNS discovery unavailable: {error}");
    }
}

/// Browses for `service_type` on a background thread. After each change,
/// the services that `parse` accepts go to `tx` as a sorted list. The thread
/// ends when every receiver is gone.
// ponytail: one thread per browse blocks on its mDNS channel. flume's
// `recv_async` can move the browse onto the runtime if the threads matter.
fn browse<T>(
    daemon: &ServiceDaemon,
    service_type: &str,
    parse: fn(&ServiceInfo) -> anyhow::Result<T>,
    tx: watch::Sender<Vec<T>>,
) -> mdns_sd::Result<()>
where
    T: Clone + Ord + Send + Sync + 'static,
{
    let events = daemon.browse(service_type)?;
    let daemon = daemon.clone();
    std::thread::spawn(move || {
        let _daemon = daemon;
        // Keyed by the mDNS full name, which `ServiceRemoved` reports.
        let mut found = HashMap::new();
        for event in events {
            if !apply_event(&mut found, event, parse) {
                continue;
            }
            let mut list: Vec<T> = found.values().cloned().collect();
            list.sort();
            if tx.send(list).is_err() {
                return;
            }
        }
    });
    Ok(())
}

/// Updates `found`, keyed by mDNS full name, and returns true when it
/// changed. `parse` maps a resolved service; a service that it rejects is
/// skipped.
fn apply_event<T: Clone + PartialEq>(
    found: &mut HashMap<String, T>,
    event: ServiceEvent,
    parse: fn(&ServiceInfo) -> anyhow::Result<T>,
) -> bool {
    match event {
        ServiceEvent::ServiceResolved(info) => match parse(&info) {
            Ok(item) => found.insert(info.get_fullname().to_string(), item.clone()) != Some(item),
            Err(error) => {
                log::debug!("skipping discovered service: {error}");
                false
            }
        },
        ServiceEvent::ServiceRemoved(_, fullname) => found.remove(&fullname).is_some(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(label: &str, url: &str) -> Server {
        Server {
            label: label.to_string(),
            url: url.to_string(),
        }
    }

    #[test]
    fn resolve_and_remove_events_track_the_server_list() {
        let mut found = HashMap::new();
        let info = service_info("Kitchen", "192.168.1.20", &[("path", "/sendspin")]);
        let fullname = info.get_fullname().to_string();
        assert!(apply_event(
            &mut found,
            ServiceEvent::ServiceResolved(info.clone()),
            server_from_service
        ));
        assert!(!apply_event(
            &mut found,
            ServiceEvent::ServiceResolved(info),
            server_from_service
        ));
        assert_eq!(found.len(), 1);
        let removed = ServiceEvent::ServiceRemoved(SERVICE_TYPE.into(), fullname);
        assert!(apply_event(&mut found, removed, server_from_service));
        assert!(found.is_empty());
    }

    #[test]
    fn music_assistant_services_need_an_http_web_ui() {
        let web_ui = |base_url: &str| {
            let info = service_info("ma", "192.168.1.20", &[("base_url", base_url)]);
            web_ui_from_service(&info).ok()
        };
        assert_eq!(
            web_ui("http://192.168.1.20:8095").as_deref(),
            Some("http://192.168.1.20:8095/")
        );
        assert_eq!(
            web_ui("https://music.example.org/ma").as_deref(),
            Some("https://music.example.org/ma")
        );
        // The browser opens the URL, so a file URL or another scheme fails.
        let bad = [
            "",
            "not a url",
            "file:///etc/passwd",
            "smb://nas/share",
            "javascript:alert(1)",
            "192.168.1.20:8095",
            "--help",
        ];
        for bad in bad {
            assert_eq!(web_ui(bad), None, "{bad:?}");
        }
        let info = service_info("ma", "192.168.1.20", &[]);
        assert!(web_ui_from_service(&info).is_err());
    }

    #[test]
    fn the_web_ui_is_on_the_host_of_the_sendspin_server() {
        let kitchen = "http://192.168.1.20:8095/".to_string();
        let attic = "http://[2001:DB8::21]:8095/".to_string();
        let both = [kitchen.clone(), attic.clone()];
        let ipv4 = "ws://192.168.1.20:8927/sendspin";
        assert_eq!(web_ui_url(ipv4, &both), Some(&kitchen));
        let ipv6 = "ws://[2001:db8::21]:8927/sendspin";
        assert_eq!(web_ui_url(ipv6, &both), Some(&attic));
        // Without a host match, only a lone Music Assistant server is a guess.
        let named = "ws://music.lan:8927/sendspin";
        assert_eq!(web_ui_url(named, &both), None);
        assert_eq!(web_ui_url(named, &both[..1]), Some(&kitchen));
        assert_eq!(web_ui_url(ipv4, &[]), None);
    }

    #[test]
    fn instance_name_stops_at_the_first_dot() {
        assert_eq!(
            instance_name("Kitchen._sendspin-server._tcp.local."),
            "Kitchen"
        );
        assert_eq!(instance_name("a.b._sendspin-server._tcp.local."), "a");
        assert_eq!(instance_name("bare"), "bare");
    }

    #[test]
    fn discovered_services_without_a_usable_path_are_rejected() {
        let info = service_info("NoPath", "192.168.1.30", &[("name", "NoPath")]);
        assert!(server_from_service(&info).is_err());
        let info = service_info("Hash", "192.168.1.30", &[("path", "/sendspin#x")]);
        assert!(server_from_service(&info).is_err());
    }

    #[test]
    fn resolved_services_become_servers() {
        let info = service_info(
            "Kitchen",
            "192.168.1.20",
            &[("path", "/sendspin"), ("name", "Kitchen")],
        );
        assert_eq!(
            server_from_service(&info).unwrap(),
            server("Kitchen", "ws://192.168.1.20:8927/sendspin")
        );
    }

    #[test]
    fn resolved_services_without_a_name_fall_back_to_the_instance() {
        let info = service_info("Attic", "192.168.1.21", &[("path", "/sendspin")]);
        assert_eq!(
            server_from_service(&info).unwrap(),
            server("Attic", "ws://192.168.1.21:8927/sendspin")
        );
    }

    #[test]
    fn resolved_services_without_a_usable_address_are_rejected() {
        // No address, loopback, unspecified, and link-local IPv6.
        for addresses in ["", "127.0.0.1,::1", "0.0.0.0,::", "fe80::20"] {
            let info = service_info("NoAddr", addresses, &[("path", "/sendspin")]);
            assert!(server_from_service(&info).is_err(), "{addresses:?}");
        }
    }

    #[test]
    fn usable_ipv4_addresses_win_over_ipv6() {
        let addresses = "127.0.0.1,2001:db8::20,192.168.1.20";
        let info = service_info("Kitchen", addresses, &[("path", "/sendspin")]);
        assert_eq!(
            server_from_service(&info).unwrap().url,
            "ws://192.168.1.20:8927/sendspin"
        );
    }

    #[test]
    fn ipv6_only_services_get_a_bracketed_url() {
        let info = service_info("Attic", "2001:db8::20", &[("path", "/sendspin")]);
        let url = server_from_service(&info).unwrap().url;
        assert_eq!(url, "ws://[2001:db8::20]:8927/sendspin");
    }

    fn service_info(
        name: &str,
        address: &str,
        properties: &[(&str, &str)],
    ) -> mdns_sd::ServiceInfo {
        ServiceInfo::new(
            SERVICE_TYPE,
            name,
            "spun-test.local.",
            address,
            8927,
            properties,
        )
        .unwrap()
        .enable_addr_auto()
    }
}
