#[macro_use]
extern crate tracing;

use std::io::ErrorKind;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::sync::{Arc, Weak};
use std::thread::{self, panicking};
use std::time::{Duration, Instant};

use rouille::Server;
use rouille::{Request, Response};
use str0m::change::{SdpAnswer, SdpOffer, SdpPendingOffer};
use str0m::channel::{ChannelData, ChannelId};
use str0m::media::{Direction, KeyframeRequest, MediaData, Mid, Rid};
use str0m::media::{KeyframeRequestKind, MediaKind};
use str0m::net::Protocol;
use str0m::rtp::RtpPacket;
use str0m::Event;
use str0m::{net::Receive, Candidate, IceConnectionState, Input, Output, Rtc, RtcError};
use systemstat::Ipv4Addr;

mod util;

fn init_log() {
    use std::env;
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};

    if env::var("RUST_LOG").is_err() {
        env::set_var("RUST_LOG", "chat=info,str0m=info");
    }

    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env())
        .init();
}

pub fn main() {
    init_log();

    let certificate = include_bytes!("cer.pem").to_vec();
    let private_key = include_bytes!("key.pem").to_vec();

    // Figure out some public IP address, since Firefox will not accept 127.0.0.1 for WebRTC traffic.
    // let host_addr = util::select_host_address();
    let host_addr = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 81));

    let (tx, rx) = mpsc::sync_channel(1);

    // Spin up a UDP socket for the RTC. All WebRTC traffic is going to be multiplexed over this single
    // server socket. Clients are identified via their respective remote (UDP) socket address.
    let socket = UdpSocket::bind(format!("{host_addr}:0")).expect("binding a random UDP port");
    let addr = socket.local_addr().expect("a local socket adddress");
    info!("Bound UDP port: {}", addr);

    // The run loop is on a separate thread to the web server.
    thread::spawn(move || run(socket, rx));

    let server = Server::new("0.0.0.0:3000", move |request| {
        web_request(request, addr, tx.clone())
    })
    .expect("starting the web server");

    let port = server.server_addr().port();
    info!("Connect a browser to https://{:?}:{:?}", addr.ip(), port);

    server.run();
}

// Handle a web request.
fn web_request(request: &Request, addr: SocketAddr, tx: SyncSender<Rtc>) -> Response {
    if request.method() == "GET" {
        return Response::html(include_str!("chat.html"));
    }

    // Expected POST SDP Offers.
    let mut data = request.data().expect("body to be available");

    let offer: SdpOffer = serde_json::from_reader(&mut data).expect("serialized offer");
    let mut rtc = Rtc::builder()
        .set_rtp_mode(true)
        // Uncomment this to see statistics
        // .set_stats_interval(Some(Duration::from_secs(1)))
        // .set_ice_lite(true)
        .build();

    // Add the shared UDP socket as a host candidate
    let candidate = Candidate::host(addr, "udp").expect("a host candidate");
    rtc.add_local_candidate(candidate);

    // Create an SDP Answer.
    let answer = rtc
        .sdp_api()
        .accept_offer(offer)
        .expect("offer to be accepted");

    // The Rtc instance is shipped off to the main run loop.
    tx.send(rtc).expect("to send Rtc instance");

    let body = serde_json::to_vec(&answer).expect("answer to serialize");

    Response::from_data("application/json", body)
}

/// This is the "main run loop" that handles all clients, reads and writes UdpSocket traffic,
/// and forwards media data between clients.
fn run(socket: UdpSocket, rx: Receiver<Rtc>) -> Result<(), RtcError> {
    let mut clients: Vec<Client> = vec![];
    let mut buf = vec![0; 2000];

    loop {
        // Clean out disconnected clients
        clients.retain(|c| c.rtc.is_alive());

        // Spawn new incoming clients from the web server thread.
        if let Some(mut client) = spawn_new_client(&rx) {
            // Add incoming tracks present in other already connected clients.
            for track in clients.iter().flat_map(|c| c.tracks_in.iter()) {
                let weak = Arc::downgrade(&track.id);
                client.handle_track_open(weak);
            }

            clients.push(client);
        }

        // Poll all clients, and get propagated events as a result.
        let to_propagate: Vec<_> = clients
            .iter_mut()
            .map(|c| c.poll_output(&socket))
            .flatten()
            .collect();
        propagate(&mut clients, to_propagate);

        socket
            .set_read_timeout(Some(Duration::from_micros(10)))
            .expect("setting socket read timeout");

        if let Some(input) = read_socket_input(&socket, &mut buf) {
            // The rtc.accepts() call is how we demultiplex the incoming packet to know which
            // Rtc instance the traffic belongs to.
            if let Some(client) = clients.iter_mut().find(|c| c.accepts(&input)) {
                // We found the client that accepts the input.
                client.handle_input(input);
            } else {
                // This is quite common because we don't get the Rtc instance via the mpsc channel
                // quickly enough before the browser send the first STUN.
                debug!("No client accepts UDP input: {:?}", input);
            }
        }

        // Drive time forward in all clients.
        let now = Instant::now();
        for client in &mut clients {
            client.handle_input(Input::Timeout(now));
        }
    }
}

/// Receive new clients from the receiver and create new Client instances.
fn spawn_new_client(rx: &Receiver<Rtc>) -> Option<Client> {
    // try_recv here won't lock up the thread.
    match rx.try_recv() {
        Ok(rtc) => Some(Client::new(rtc)),
        Err(TryRecvError::Empty) => None,
        _ => panic!("Receiver<Rtc> disconnected"),
    }
}

fn propagate(clients: &mut [Client], to_propagate: Vec<Propagated>) {
    for propagated in to_propagate {
        let Some(client_id) = propagated.client_id() else {
            // If the event doesn't have a client id, it can't be propagated,
            // (it's either a noop or a timeout).
            continue;
        };

        for client in &mut *clients {
            if client.id == client_id {
                // Do not propagate to originating client.
                continue;
            }

            match &propagated {
                Propagated::TrackOpen(_, track_in) => client.handle_track_open(track_in.clone()),
                Propagated::MediaData(_, data) => client.handle_media_data_out(client_id, data),
                Propagated::KeyframeRequest(_, req, origin, mid_in) => {
                    // Only one origin client handles the keyframe request.
                    if *origin == client.id {
                        client.handle_keyframe_request(*req, *mid_in)
                    }
                }
                Propagated::RtpPacket(_, packet) => client.handle_packet(client_id, packet),
                Propagated::Noop | Propagated::Timeout(_) => {}
            }
        }
    }
}

fn read_socket_input<'a>(socket: &UdpSocket, buf: &'a mut Vec<u8>) -> Option<Input<'a>> {
    buf.resize(2000, 0);

    match socket.recv_from(buf) {
        Ok((n, source)) => {
            buf.truncate(n);

            // Parse data to a DatagramRecv, which help preparse network data to
            // figure out the multiplexing of all protocols on one UDP port.
            let Ok(contents) = buf.as_slice().try_into() else {
                return None;
            };

            return Some(Input::Receive(
                Instant::now(),
                Receive {
                    proto: Protocol::Udp,
                    source,
                    destination: socket.local_addr().unwrap(),
                    contents,
                },
            ));
        }

        Err(e) => match e.kind() {
            // Expected error for set_read_timeout(). One for windows, one for the rest.
            ErrorKind::WouldBlock | ErrorKind::TimedOut => None,
            _ => panic!("UdpSocket read failed: {e:?}"),
        },
    }
}

#[derive(Debug)]
struct Client {
    id: ClientId,
    rtc: Rtc,
    pending: Option<SdpPendingOffer>,
    cid: Option<ChannelId>,
    tracks_in: Vec<TrackInEntry>,
    tracks_out: Vec<TrackOut>,
    chosen_rid: Option<Rid>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ClientId(u64);

impl Deref for ClientId {
    type Target = u64;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Debug)]
struct TrackIn {
    origin: ClientId,
    mid: Mid,
    kind: MediaKind,
}

#[derive(Debug)]
struct TrackInEntry {
    id: Arc<TrackIn>,
    last_keyframe_request: Option<Instant>,
}

#[derive(Debug)]
struct TrackOut {
    track_in: Weak<TrackIn>,
    state: TrackOutState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrackOutState {
    ToOpen,
    Negotiating(Mid),
    Open(Mid),
}

impl TrackOut {
    fn mid(&self) -> Option<Mid> {
        match self.state {
            TrackOutState::ToOpen => None,
            TrackOutState::Negotiating(m) | TrackOutState::Open(m) => Some(m),
        }
    }
}

impl Client {
    fn new(rtc: Rtc) -> Client {
        static ID_COUNTER: AtomicU64 = AtomicU64::new(0);
        let next_id = ID_COUNTER.fetch_add(1, Ordering::SeqCst);
        Client {
            id: ClientId(next_id),
            rtc,
            pending: None,
            cid: None,
            tracks_in: vec![],
            tracks_out: vec![],
            chosen_rid: None,
        }
    }

    fn accepts(&self, input: &Input) -> bool {
        self.rtc.accepts(input)
    }

    fn handle_input(&mut self, input: Input) {
        if !self.rtc.is_alive() {
            return;
        }

        if let Err(e) = self.rtc.handle_input(input) {
            warn!("Client ({}) disconnected: {:?}", *self.id, e);
            self.rtc.disconnect();
        }
    }

    fn poll_output(&mut self, socket: &UdpSocket) -> Vec<Propagated> {
        if !self.rtc.is_alive() {
            return vec![Propagated::Noop];
        }

        // Incoming tracks from other clients cause new entries in track_out that
        // need SDP negotiation with the remote peer.
        if self.negotiate_if_needed() {
            return vec![Propagated::Noop];
        }

        let mut propagate = Vec::new();
        loop {
            match self.rtc.poll_output() {
                Ok(output) => {
                    let p = match output {
                        Output::Transmit(transmit) => {
                            socket
                                .send_to(&transmit.contents, transmit.destination)
                                .expect("sending UDP data");
                            Propagated::Noop
                        }
                        Output::Timeout(t) => {
                            propagate.push(Propagated::Timeout(t));

                            return propagate;
                        }
                        Output::Event(e) => match e {
                            Event::IceConnectionStateChange(v) => {
                                if v == IceConnectionState::Disconnected {
                                    // Ice disconnect could result in trying to establish a new connection,
                                    // but this impl just disconnects directly.
                                    self.rtc.disconnect();
                                }
                                Propagated::Noop
                            }
                            Event::MediaAdded(e) => self.handle_media_added(e.mid, e.kind),
                            Event::KeyframeRequest(req) => self.handle_incoming_keyframe_req(req),
                            Event::ChannelOpen(cid, _) => {
                                self.cid = Some(cid);
                                Propagated::Noop
                            }
                            Event::ChannelData(data) => self.handle_channel_data(data),

                            // NB: To see statistics, uncomment set_stats_interval() above.
                            Event::MediaIngressStats(data) => {
                                info!("{:?}", data);
                                Propagated::Noop
                            }
                            Event::MediaEgressStats(data) => {
                                info!("{:?}", data);
                                Propagated::Noop
                            }
                            Event::PeerStats(data) => {
                                info!("{:?}", data);
                                Propagated::Noop
                            }
                            Event::RtpPacket(data) => Propagated::RtpPacket(self.id, data),
                            _ => Propagated::Noop,
                        },
                    };

                    propagate.push(p);
                }
                Err(e) => {
                    warn!("Client ({}) poll_output failed: {:?}", *self.id, e);
                    self.rtc.disconnect();
                    return Vec::new();
                }
            }
        }
    }

    fn handle_media_added(&mut self, mid: Mid, kind: MediaKind) -> Propagated {
        let track_in = TrackInEntry {
            id: Arc::new(TrackIn {
                origin: self.id,
                mid,
                kind,
            }),
            last_keyframe_request: None,
        };

        // The Client instance owns the strong reference to the incoming
        // track, all other clients have a weak reference.
        let weak = Arc::downgrade(&track_in.id);
        self.tracks_in.push(track_in);

        Propagated::TrackOpen(self.id, weak)
    }

    fn handle_incoming_keyframe_req(&self, mut req: KeyframeRequest) -> Propagated {
        // Need to figure out the track_in mid that needs to handle the keyframe request.
        let Some(track_out) = self.tracks_out.iter().find(|t| t.mid() == Some(req.mid)) else {
            return Propagated::Noop;
        };
        let Some(track_in) = track_out.track_in.upgrade() else {
            return Propagated::Noop;
        };

        // This is the rid picked from incoming mediadata, and to which we need to
        // send the keyframe request.
        req.rid = self.chosen_rid;

        Propagated::KeyframeRequest(self.id, req, track_in.origin, track_in.mid)
    }

    fn negotiate_if_needed(&mut self) -> bool {
        if self.cid.is_none() || self.pending.is_some() {
            // Don't negotiate if there is no data channel, or if we have pending changes already.
            return false;
        }

        let mut change = self.rtc.sdp_api();

        for track in &mut self.tracks_out {
            if let TrackOutState::ToOpen = track.state {
                if let Some(track_in) = track.track_in.upgrade() {
                    let stream_id = track_in.origin.to_string();
                    let mid =
                        change.add_media(track_in.kind, Direction::SendOnly, Some(stream_id), None);
                    track.state = TrackOutState::Negotiating(mid);
                }
            }
        }

        if !change.has_changes() {
            return false;
        }

        let Some((offer, pending)) = change.apply() else {
            return false;
        };

        let Some(mut channel) = self.cid.and_then(|id| self.rtc.channel(id)) else {
            return false;
        };

        let json = serde_json::to_string(&offer).unwrap();
        channel
            .write(false, json.as_bytes())
            .expect("to write answer");

        self.pending = Some(pending);

        true
    }

    fn handle_channel_data(&mut self, d: ChannelData) -> Propagated {
        if let Ok(offer) = serde_json::from_slice::<'_, SdpOffer>(&d.data) {
            self.handle_offer(offer);
        } else if let Ok(answer) = serde_json::from_slice::<'_, SdpAnswer>(&d.data) {
            self.handle_answer(answer);
        }

        Propagated::Noop
    }

    fn handle_offer(&mut self, offer: SdpOffer) {
        let answer = self
            .rtc
            .sdp_api()
            .accept_offer(offer)
            .expect("offer to be accepted");

        // Keep local track state in sync, cancelling any pending negotiation
        // so we can redo it after this offer is handled.
        for track in &mut self.tracks_out {
            if let TrackOutState::Negotiating(_) = track.state {
                track.state = TrackOutState::ToOpen;
            }
        }

        let mut channel = self
            .cid
            .and_then(|id| self.rtc.channel(id))
            .expect("channel to be open");

        let json = serde_json::to_string(&answer).unwrap();
        channel
            .write(false, json.as_bytes())
            .expect("to write answer");
    }

    fn handle_answer(&mut self, answer: SdpAnswer) {
        if let Some(pending) = self.pending.take() {
            self.rtc
                .sdp_api()
                .accept_answer(pending, answer)
                .expect("answer to be accepted");

            for track in &mut self.tracks_out {
                if let TrackOutState::Negotiating(m) = track.state {
                    track.state = TrackOutState::Open(m);
                }
            }
        }
    }

    fn handle_track_open(&mut self, track_in: Weak<TrackIn>) {
        let track_out = TrackOut {
            track_in,
            state: TrackOutState::ToOpen,
        };
        self.tracks_out.push(track_out);
    }

    fn handle_media_data_out(&mut self, origin: ClientId, data: &MediaData) {
        // Figure out which outgoing track maps to the incoming media data.
        let Some(mid) = self
            .tracks_out
            .iter()
            .find(|o| {
                o.track_in
                    .upgrade()
                    .filter(|i| i.origin == origin && i.mid == data.mid)
                    .is_some()
            })
            .and_then(|o| o.mid())
        else {
            return;
        };

        if data.rid.is_some() && data.rid != Some("h".into()) {
            // This is where we plug in a selection strategy for simulcast. For
            // now either let rid=None through (which would be no simulcast layers)
            // or "h" if we have simulcast (see commented out code in chat.html).
            return;
        }

        // Remember this value for keyframe requests.
        if self.chosen_rid != data.rid {
            self.chosen_rid = data.rid;
        }

        let Some(writer) = self.rtc.writer(mid) else {
            return;
        };

        // Match outgoing pt to incoming codec.
        let Some(pt) = writer.match_params(data.params) else {
            return;
        };

        if let Err(e) = writer.write(pt, data.network_time, data.time, data.data.clone()) {
            warn!("Client ({}) failed: {:?}", *self.id, e);
            self.rtc.disconnect();
        }
    }

    fn handle_packet(&mut self, origin: ClientId, packet: &RtpPacket) {
        // Figure out which outgoing track maps to the incoming media data.
        let Some(mid) = self
            .tracks_out
            .iter()
            .find(|o| {
                o.track_in
                    .upgrade()
                    .filter(|i| i.origin == origin && i.kind == MediaKind::Video)
                    .is_some()
            })
            .and_then(|o| o.mid())
        else {
            return;
        };

        let mut dir = self.rtc.direct_api();
        let Some(writer) = dir.stream_tx_by_mid(mid, None) else {
            return;
        };

        let mut payload = packet.payload.clone();

        let i = (payload.as_slice()[0] & 0x80) != 0;
        let p = (payload.as_slice()[0] & 0x40) != 0;
        let l = (payload.as_slice()[0] & 0x20) != 0;
        let f = (payload.as_slice()[0] & 0x10) != 0;
        let b = (payload.as_slice()[0] & 0x08) != 0;
        let e = (payload.as_slice()[0] & 0x04) != 0;
        let v = (payload.as_slice()[0] & 0x02) != 0;
        let z = (payload.as_slice()[0] & 0x01) != 0;
        let m = (payload.as_slice()[1] & 0x80) != 0;

        println!("before ssrc: {:?}\ni: {i}\np: {p}\nl: {l}\nf: {f}\nb: {b}\ne: {e}\nv: {v}\nz: {z}\nm: {m}\nPICTURE ID: {:?}\nTL0PICIDX: {:?}\nlen payload: {}\n\n\n", packet.header.ssrc, ((payload.as_slice()[1] & 0x7F) as u16) << 8 | payload.as_slice()[2] as u16, payload.as_slice()[3], payload.len());

        let mut pl_ind = 3;

        if l {
            if f {
                pl_ind += 1;
            } else {
                pl_ind += 2;
            }
        }

        if p && f {
            let mut b = 1u8;
            while b != 0 {
                let reader = payload.get(pl_ind).unwrap();
                b = reader & 0x01;
                pl_ind += 1;
            }
        }

        if v {
            let ns = payload.get(pl_ind).unwrap() >> 5;
            let y = payload.get(pl_ind).unwrap() & 0x10 != 0;
            let g = payload.get(pl_ind).unwrap() & 0x08 != 0;

            pl_ind += 1;

            let ns = (ns + 1) as usize;
            let mut ng = 0;

            if y {
                pl_ind += 4 * ns;
            }

            if g {
                ng = *payload.get(pl_ind).unwrap();
                pl_ind += 1;
            }

            for _ in 0..ng as usize {
                let b = *payload.get(pl_ind).unwrap();
                pl_ind += 1;

                let r = ((b >> 2) & 0x3) as usize;

                for _ in 0..r {
                    pl_ind += 1;
                }
            }
        }

        _ = payload.drain(5..pl_ind).collect::<Vec<u8>>();

        // *payload.get_mut(0).unwrap() &= 0xFE;
        *payload.get_mut(0).unwrap() &= 0x0C;
        *payload.get_mut(0).unwrap() |= 0x90;

        let ii = (payload.as_slice()[0] & 0x80) != 0;
        let pp = (payload.as_slice()[0] & 0x40) != 0;
        let ll = (payload.as_slice()[0] & 0x20) != 0;
        let ff = (payload.as_slice()[0] & 0x10) != 0;
        let bb = (payload.as_slice()[0] & 0x08) != 0;
        let ee = (payload.as_slice()[0] & 0x04) != 0;
        let vv = (payload.as_slice()[0] & 0x02) != 0;
        let zz = (payload.as_slice()[0] & 0x01) != 0;

        println!("after ssrc: {:?}\ni: {ii}\np: {pp}\nl: {ll}\nf: {ff}\nb: {bb}\ne: {ee}\nv: {vv}\nz: {zz}\nm: {m}\nPICTURE ID: {:?}\nTL0PICIDX: {:?}\nlen payload: {}\n\n\n", packet.header.ssrc, ((payload.as_slice()[1] & 0x7F) as u16) << 8 | payload.as_slice()[2] as u16, payload.as_slice()[3], payload.len());

        // if v {
        //     panic!("aaaaa");
        // }

        writer
            .write_rtp(
                packet.header.payload_type,
                packet.seq_no,
                packet.time.numer() as u32,
                packet.timestamp,
                packet.header.marker,
                packet.header.ext_vals.clone(),
                false,
                payload,
            )
            .unwrap();
    }

    fn handle_keyframe_request(&mut self, req: KeyframeRequest, mid_in: Mid) {
        let has_incoming_track = self.tracks_in.iter().any(|i| i.id.mid == mid_in);

        // This will be the case for all other client but the one where the track originates.
        if !has_incoming_track {
            return;
        }

        let mut dir = self.rtc.direct_api();
        let stream = dir.stream_rx_by_mid(mid_in, None).unwrap();
        stream.request_keyframe(req.kind);

        // let Some(mut writer) = self.rtc.writer(mid_in) else {
        //     return;
        // };
        //
        // if let Err(e) = writer.request_keyframe(req.rid, req.kind) {
        //     // This can fail if the rid doesn't match any media.
        //     info!("request_keyframe failed: {:?}", e);
        // }
    }
}

/// Events propagated between client.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
enum Propagated {
    /// When we have nothing to propagate.
    Noop,

    /// Poll client has reached timeout.
    Timeout(Instant),

    /// A new incoming track opened.
    TrackOpen(ClientId, Weak<TrackIn>),

    /// Data to be propagated from one client to another.
    MediaData(ClientId, MediaData),

    /// A keyframe request from one client to the source.
    KeyframeRequest(ClientId, KeyframeRequest, ClientId, Mid),

    RtpPacket(ClientId, RtpPacket),
}

impl Propagated {
    /// Get client id, if the propagated event has a client id.
    fn client_id(&self) -> Option<ClientId> {
        match self {
            Propagated::TrackOpen(c, _)
            | Propagated::MediaData(c, _)
            | Propagated::KeyframeRequest(c, _, _, _)
            | Propagated::RtpPacket(c, _) => Some(*c),
            _ => None,
        }
    }
}
