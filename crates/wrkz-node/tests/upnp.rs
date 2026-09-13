// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! UPnP port mapping against a fake gateway on loopback: an SSDP responder on
//! a UDP port and an HTTP server that serves a real-shaped description and
//! answers the SOAP actions miniupnpc sends. Discovery is pointed at the
//! responder instead of the multicast group, so nothing leaves this machine.

use std::collections::HashMap;
use std::io::{BufReader, Write};
use std::net::{SocketAddr, TcpListener, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use wrkz_node::upnp::{self, IgdStatus, PortMapper, UpnpConfig, MAPPING_DESCRIPTION};
use wrkz_rpc::http::{read_request, HttpLimits};

const SERVICE: &str = "urn:schemas-upnp-org:service:WANIPConnection:1";

/// One mapping, as the gateway holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Mapping {
    client: String,
    internal_port: String,
    description: String,
    protocol: String,
    lease: String,
    remote_host: String,
}

#[derive(Default)]
struct Gateway {
    external_ip: String,
    status: String,
    mappings: HashMap<String, Mapping>,
    /// Every SOAP action asked, in order.
    actions: Vec<String>,
}

struct FakeIgd {
    ssdp: SocketAddr,
    http: SocketAddr,
    state: Arc<Mutex<Gateway>>,
    stop: Arc<AtomicBool>,
}

impl Drop for FakeIgd {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn element<'a>(body: &'a str, name: &str) -> &'a str {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    body.split_once(&open).and_then(|(_, rest)| rest.split_once(&close)).map_or("", |(value, _)| value)
}

fn envelope(inner: &str) -> String {
    format!(
        "<?xml version=\"1.0\"?>\r\n<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body>{inner}</s:Body></s:Envelope>\r\n"
    )
}

fn fault(code: u32, description: &str) -> String {
    envelope(&format!(
        "<s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring><detail>\
         <UPnPError xmlns=\"urn:schemas-upnp-org:control-1-0\"><errorCode>{code}</errorCode>\
         <errorDescription>{description}</errorDescription></UPnPError></detail></s:Fault>"
    ))
}

fn description() -> String {
    format!(
        "<?xml version=\"1.0\"?>\r\n<root xmlns=\"urn:schemas-upnp-org:device-1-0\"><specVersion><major>1</major>\
         <minor>0</minor></specVersion><device><deviceType>urn:schemas-upnp-org:device:InternetGatewayDevice:1\
         </deviceType><friendlyName>fake</friendlyName><deviceList><device><deviceType>\
         urn:schemas-upnp-org:device:WANDevice:1</deviceType><serviceList><service><serviceType>\
         urn:schemas-upnp-org:service:WANCommonInterfaceConfig:1</serviceType><serviceId>\
         urn:upnp-org:serviceId:WANCommonIFC1</serviceId><controlURL>/ctl/CmnIfCfg</controlURL><eventSubURL>\
         /evt/CmnIfCfg</eventSubURL><SCPDURL>/WANCfg.xml</SCPDURL></service></serviceList><deviceList><device>\
         <deviceType>urn:schemas-upnp-org:device:WANConnectionDevice:1</deviceType><serviceList><service>\
         <serviceType>{SERVICE}</serviceType><serviceId>urn:upnp-org:serviceId:WANIPConn1</serviceId>\
         <controlURL>ctl/IPConn</controlURL><eventSubURL>/evt/IPConn</eventSubURL><SCPDURL>/WANIPCn.xml</SCPDURL>\
         </service></serviceList></device></deviceList></device></deviceList></device></root>"
    )
}

/// One SOAP action against the gateway's table: `(status, body)`.
fn act(state: &Mutex<Gateway>, action: &str, body: &str) -> (u16, String) {
    let mut g = state.lock().unwrap();
    g.actions.push(action.to_string());
    let respond = |inner: String| {
        (200, envelope(&format!("<u:{action}Response xmlns:u=\"{SERVICE}\">{inner}</u:{action}Response>")))
    };
    let port = element(body, "NewExternalPort").to_string();
    match action {
        "GetStatusInfo" => respond(format!(
            "<NewConnectionStatus>{}</NewConnectionStatus><NewLastConnectionError>ERROR_NONE\
             </NewLastConnectionError><NewUptime>4242</NewUptime>",
            g.status
        )),
        "GetExternalIPAddress" => respond(format!("<NewExternalIPAddress>{}</NewExternalIPAddress>", g.external_ip)),
        "AddPortMapping" => {
            let mapping = Mapping {
                client: element(body, "NewInternalClient").to_string(),
                internal_port: element(body, "NewInternalPort").to_string(),
                description: element(body, "NewPortMappingDescription").to_string(),
                protocol: element(body, "NewProtocol").to_string(),
                lease: element(body, "NewLeaseDuration").to_string(),
                remote_host: element(body, "NewRemoteHost").to_string(),
            };
            match g.mappings.get(&port) {
                Some(existing) if existing.client != mapping.client => (500, fault(718, "ConflictInMappingEntry")),
                _ => {
                    g.mappings.insert(port, mapping);
                    respond(String::new())
                }
            }
        }
        "GetSpecificPortMappingEntry" => match g.mappings.get(&port) {
            Some(m) => respond(format!(
                "<NewInternalPort>{}</NewInternalPort><NewInternalClient>{}</NewInternalClient><NewEnabled>1\
                 </NewEnabled><NewPortMappingDescription>{}</NewPortMappingDescription><NewLeaseDuration>0\
                 </NewLeaseDuration>",
                m.internal_port, m.client, m.description
            )),
            None => (500, fault(714, "NoSuchEntryInArray")),
        },
        "DeletePortMapping" => match g.mappings.remove(&port) {
            Some(_) => respond(String::new()),
            None => (500, fault(714, "NoSuchEntryInArray")),
        },
        _ => (500, fault(401, "Invalid Action")),
    }
}

impl FakeIgd {
    fn start(external_ip: &str) -> Self {
        let state = Arc::new(Mutex::new(Gateway {
            external_ip: external_ip.to_string(),
            status: "Connected".to_string(),
            ..Default::default()
        }));
        let stop = Arc::new(AtomicBool::new(false));

        let http = TcpListener::bind("127.0.0.1:0").unwrap();
        let http_addr = http.local_addr().unwrap();
        let (st, sp) = (Arc::clone(&state), Arc::clone(&stop));
        std::thread::spawn(move || {
            for stream in http.incoming() {
                if sp.load(Ordering::SeqCst) {
                    return;
                }
                let Ok(mut stream) = stream else { continue };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let Ok(clone) = stream.try_clone() else { continue };
                let Ok(request) = read_request(&mut BufReader::new(clone), &HttpLimits::default()) else { continue };
                let (status, body, kind) = match (request.method.as_str(), request.path.as_str()) {
                    ("GET", "/rootDesc.xml") => (200, description(), "text/xml"),
                    ("POST", "/ctl/IPConn") => {
                        let action = request.header("SOAPAction").unwrap_or("").trim_matches('"');
                        let action = action.rsplit_once('#').map_or("", |(service, action)| {
                            assert_eq!(service, SERVICE, "the SOAPAction names the service");
                            action
                        });
                        let (status, body) = act(&st, action, &String::from_utf8_lossy(&request.body));
                        (status, body, "text/xml; charset=\"utf-8\"")
                    }
                    _ => (404, String::new(), "text/plain"),
                };
                let reason = if status == 200 { "OK" } else { "Error" };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });

        let ssdp = UdpSocket::bind("127.0.0.1:0").unwrap();
        let ssdp_addr = ssdp.local_addr().unwrap();
        ssdp.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
        let sp = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut buffer = [0u8; 2048];
            while !sp.load(Ordering::SeqCst) {
                let Ok((n, from)) = ssdp.recv_from(&mut buffer) else { continue };
                let search = String::from_utf8_lossy(&buffer[..n]);
                // A gateway answers the search for itself, and not for the
                // other three targets, which is what makes discovery stop at
                // the first.
                let target = "urn:schemas-upnp-org:device:InternetGatewayDevice:1";
                if search.starts_with("M-SEARCH") && search.contains(&format!("ST: {target}\r\n")) {
                    let answer = format!(
                        "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=120\r\nST: {target}\r\nUSN: uuid:fake::{target}\r\n\
                         EXT:\r\nSERVER: fake UPnP/1.1\r\nLOCATION: http://{http_addr}/rootDesc.xml\r\n\r\n"
                    );
                    let _ = ssdp.send_to(answer.as_bytes(), from);
                }
            }
        });
        Self { ssdp: ssdp_addr, http: http_addr, state, stop }
    }

    fn config(&self) -> UpnpConfig {
        UpnpConfig { ssdp_targets: vec![self.ssdp], search_wait: Duration::from_millis(400), ..UpnpConfig::default() }
    }

    fn mapping(&self, port: u16) -> Option<Mapping> {
        self.state.lock().unwrap().mappings.get(&port.to_string()).cloned()
    }

    fn actions(&self) -> Vec<String> {
        self.state.lock().unwrap().actions.clone()
    }
}

const PORT: u16 = 17855;

fn any() -> std::net::IpAddr {
    "0.0.0.0".parse().unwrap()
}

#[test]
fn a_port_is_mapped_through_the_gateway_and_removed_on_shutdown() {
    let igd = FakeIgd::start("93.184.216.34");
    let mapper = PortMapper::spawn(PORT, any(), igd.config());
    assert!(mapper.wait(Duration::from_secs(20)), "the attempt never ended");
    let (gateway, port) = mapper.mapping().expect("a mapping");
    assert_eq!(port, PORT);
    // `ctl/IPConn` in the description, resolved against its authority.
    assert_eq!(gateway.control_url, format!("http://{}/ctl/IPConn", igd.http));
    assert_eq!(gateway.description_url, format!("http://{}/rootDesc.xml", igd.http));
    assert_eq!(gateway.lan_addr.to_string(), "127.0.0.1");
    assert_eq!(gateway.external_addr.as_deref(), Some("93.184.216.34"));
    assert_eq!(
        igd.mapping(PORT),
        Some(Mapping {
            client: "127.0.0.1".into(),
            internal_port: PORT.to_string(),
            description: MAPPING_DESCRIPTION.into(),
            protocol: "TCP".into(),
            lease: "0".into(),
            remote_host: String::new(),
        })
    );
    mapper.shutdown(Duration::from_secs(1));
    assert_eq!(igd.mapping(PORT), None, "removed on shutdown");
    assert_eq!(
        igd.actions(),
        ["GetStatusInfo", "GetExternalIPAddress", "AddPortMapping", "GetSpecificPortMappingEntry", "DeletePortMapping"]
    );
}

#[test]
fn behind_a_reserved_address_nothing_is_mapped() {
    let igd = FakeIgd::start("10.20.30.40");
    let cfg = igd.config();
    let stop = AtomicBool::new(false);
    let devices = upnp::discover(&cfg, &stop).unwrap();
    assert_eq!(devices.len(), 1, "{devices:?}");
    let (status, gateway) = upnp::find_gateway(&devices, &cfg, &stop);
    assert_eq!(status, IgdStatus::PrivateIp);
    assert_eq!(gateway.unwrap().external_addr.as_deref(), Some("10.20.30.40"));

    let mapper = PortMapper::spawn(PORT, any(), cfg);
    assert!(mapper.wait(Duration::from_secs(20)));
    assert!(mapper.mapping().is_none());
    assert!(!igd.actions().iter().any(|a| a == "AddPortMapping"), "{:?}", igd.actions());
}

#[test]
fn a_gateway_that_is_not_connected_is_reported_as_such() {
    let igd = FakeIgd::start("93.184.216.34");
    igd.state.lock().unwrap().status = "Disconnected".into();
    let cfg = igd.config();
    let stop = AtomicBool::new(false);
    let devices = upnp::discover(&cfg, &stop).unwrap();
    assert_eq!(upnp::find_gateway(&devices, &cfg, &stop).0, IgdStatus::Disconnected);
}

#[test]
fn a_mapping_another_host_has_taken_is_left_in_place() {
    let igd = FakeIgd::start("93.184.216.34");
    let mapper = PortMapper::spawn(PORT, any(), igd.config());
    assert!(mapper.wait(Duration::from_secs(20)));
    assert!(mapper.mapping().is_some());
    igd.state.lock().unwrap().mappings.get_mut(&PORT.to_string()).unwrap().client = "192.168.1.77".into();
    mapper.shutdown(Duration::from_secs(1));
    assert_eq!(igd.mapping(PORT).map(|m| m.client), Some("192.168.1.77".to_string()));
    assert!(!igd.actions().iter().any(|a| a == "DeletePortMapping"), "{:?}", igd.actions());
}

#[test]
fn a_refused_mapping_is_not_recorded() {
    let igd = FakeIgd::start("93.184.216.34");
    igd.state.lock().unwrap().mappings.insert(
        PORT.to_string(),
        Mapping {
            client: "192.168.1.77".into(),
            internal_port: PORT.to_string(),
            description: "someone else".into(),
            protocol: "TCP".into(),
            lease: "0".into(),
            remote_host: String::new(),
        },
    );
    let mapper = PortMapper::spawn(PORT, any(), igd.config());
    assert!(mapper.wait(Duration::from_secs(20)));
    assert!(mapper.mapping().is_none(), "718 ConflictInMappingEntry");
    mapper.shutdown(Duration::from_secs(1));
    assert_eq!(igd.mapping(PORT).map(|m| m.client), Some("192.168.1.77".to_string()));
}

#[test]
fn a_listener_bound_elsewhere_is_not_mapped() {
    let igd = FakeIgd::start("93.184.216.34");
    let mapper = PortMapper::spawn(PORT, "192.168.99.99".parse().unwrap(), igd.config());
    assert!(mapper.wait(Duration::from_secs(20)));
    assert!(mapper.mapping().is_none());
    assert!(!igd.actions().iter().any(|a| a == "AddPortMapping"));
}

#[test]
fn silence_is_no_gateway() {
    let quiet = UdpSocket::bind("127.0.0.1:0").unwrap();
    let cfg = UpnpConfig {
        ssdp_targets: vec![quiet.local_addr().unwrap()],
        search_wait: Duration::from_millis(50),
        ..UpnpConfig::default()
    };
    let stop = AtomicBool::new(false);
    let devices = upnp::discover(&cfg, &stop).unwrap();
    assert!(devices.is_empty());
    assert_eq!(upnp::find_gateway(&devices, &cfg, &stop), (IgdStatus::NoIgd, None));
}
