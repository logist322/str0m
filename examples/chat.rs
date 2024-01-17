#[macro_use]
extern crate tracing;

use std::io::ErrorKind;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::ops::{Deref, AddAssign};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::sync::{Arc, Weak};
use std::thread::{self, panicking};
use std::time::{Duration, Instant};

use rouille::{Server, HeadersIter};
use rouille::{Request, Response};
use str0m::change::{self, SdpAnswer, SdpOffer, SdpPendingOffer};
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

    let host_addr = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));

    let (tx, rx) = mpsc::sync_channel(1);
    let (tx_change, rx_change) = mpsc::sync_channel(16);

    // Spin up a UDP socket for the RTC. All WebRTC traffic is going to be multiplexed over this single
    // server socket. Clients are identified via their respective remote (UDP) socket address.
    let socket = UdpSocket::bind(format!("{host_addr}:0")).expect("binding a random UDP port");
    let addr = socket.local_addr().expect("a local socket adddress");
    info!("Bound UDP port: {}", addr);

    // The run loop is on a separate thread to the web server.
    thread::spawn(move || run(socket, rx, rx_change));

    let server = Server::new("0.0.0.0:3000", move |request| {
        web_request(request, addr, tx.clone(), tx_change.clone())
    })
        .expect("starting the web server");

    let port = server.server_addr().port();
    info!("Connect a browser to https://{:?}:{:?}", addr.ip(), port);

    server.run();
}

#[derive(Debug)]
enum LayerChangeRequest {
    Spatial(u8),
    Temporal(u8),
}

// Handle a web request.
fn web_request(
    request: &Request,
    addr: SocketAddr,
    tx: SyncSender<Rtc>,
    tx_change: SyncSender<LayerChangeRequest>,
) -> Response {
    if request.method() == "GET" {
        return Response::html(include_str!("chat.html"));
    } else if request.method() == "POST" {
        let mut ok = false;

        if let Some(spatial) = request.get_param("spatial") {
            tx_change
                .send(LayerChangeRequest::Spatial(spatial.parse().unwrap()))
                .expect("to send spatial change");

            ok = true;
        }

        if let Some(temporal) = request.get_param("temporal") {
            tx_change
                .send(LayerChangeRequest::Temporal(temporal.parse().unwrap()))
                .expect("to send temporal change");

            ok = true;
        }

        if ok {
            return Response::empty_204();
        }
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
fn run(socket: UdpSocket, rx: Receiver<Rtc>, rx_change: Receiver<LayerChangeRequest>) -> Result<(), RtcError> {
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

        let mut to_propagate = Vec::new();

        while let Some(change) = match rx_change.try_recv() {
            Ok(change) => Some(change),
            Err(TryRecvError::Empty) => None,
            _ => panic!("Receiver<Change> disconnected"),
        } {
            for client in &mut clients {
                match change {
                    LayerChangeRequest::Spatial(s) => {
                        if s > client.target_spatial {
                            let Some(mid) = client
                                .tracks_out
                                .iter()
                                .find(|o| {
                                    o.track_in
                                        .upgrade()
                                        .filter(|i| i.kind == MediaKind::Video)
                                        .is_some()
                                })
                                .and_then(|o| o.mid())
                            else {
                                continue;
                            };

                            to_propagate.push(client.handle_incoming_keyframe_req(
                                KeyframeRequest {
                                    mid,
                                    rid: None,
                                    kind: KeyframeRequestKind::Fir,
                                },
                            ));

                            client.pending_spatial = Some(s);
                        } else {
                            client.new_spatial = Some(s);
                        }
                    }
                    LayerChangeRequest::Temporal(t) => client.target_temporal = t,
                }
            }
        }

        propagate(&mut clients, to_propagate);

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
                Propagated::RtpPacket(_, packet) => client.handle_packet(client_id, packet),
                Propagated::KeyframeRequest(_, req, origin, mid_in) => {
                    // Only one origin client handles the keyframe request.
                    if *origin == client.id {
                        client.handle_keyframe_request(*req, *mid_in)
                    }
                }
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

    // set on downscaling
    new_spatial: Option<u8>,
    // set on upscaling
    pending_spatial: Option<u8>,

    // currently set
    target_spatial: u8,
    target_temporal: u8,

    base_seq: u16,
    base_seq_prev: u16,

    vp9_header_descriptor: Vp9HeaderDescriptor,
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

            new_spatial: None,
            pending_spatial: None,

            target_spatial: 2,
            target_temporal: 2,

            base_seq: 0,
            base_seq_prev: 0,

            vp9_header_descriptor: Vp9HeaderDescriptor::default(),
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

        self.vp9_header_descriptor.parse(&packet.payload);

        if !self.vp9_header_descriptor.p
            && self.vp9_header_descriptor.sid == 0
            && self.vp9_header_descriptor.b
        {
            if let Some(new_s) = self.pending_spatial.take() {
                self.target_spatial = new_s;
            }
        }

        if self.vp9_header_descriptor.sid == 0 && self.vp9_header_descriptor.b {
            if let Some(new_s) = self.new_spatial.take() {
                self.target_spatial = new_s;
            }
        }

        if (self.vp9_header_descriptor.sid > self.target_spatial)
            || (self.vp9_header_descriptor.tid > self.target_temporal)
        {
            self.base_seq += 1;

            return;
        }

        let header_seq = packet.header.sequence_number - self.base_seq + self.base_seq_prev + 1;
        let marker = packet.header.marker
            || (self.vp9_header_descriptor.e
            && self.vp9_header_descriptor.sid == self.target_spatial);

        writer
            .write_rtp(
                packet.header.payload_type,
                (header_seq as u64).into(),
                packet.time.numer() as u32,
                packet.timestamp,
                marker,
                packet.header.ext_vals.clone(),
                true,
                packet.payload.clone(),
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

    /// A keyframe request from one client to the source.
    KeyframeRequest(ClientId, KeyframeRequest, ClientId, Mid),

    RtpPacket(ClientId, RtpPacket),
}

impl Propagated {
    /// Get client id, if the propagated event has a client id.
    fn client_id(&self) -> Option<ClientId> {
        match self {
            Propagated::TrackOpen(c, _)
            | Propagated::RtpPacket(c, _),
            | Propagated::KeyframeRequest(c, _, _, _) => Some(*c),
            _ => None,
        }
    }
}

trait BitRead {
    fn remaining(&self) -> usize;
    fn get_u8(&mut self) -> u8;
    fn get_u16(&mut self) -> u16;
}

impl BitRead for (&[u8], usize) {
    #[inline(always)]
    fn remaining(&self) -> usize {
        (self.0.len() * 8).saturating_sub(self.1)
    }

    #[inline(always)]
    fn get_u8(&mut self) -> u8 {
        if self.remaining() == 0 {
            panic!("Too few bits left");
        }

        let offs = self.1 / 8;
        let shift = (self.1 % 8) as u32;
        self.1 += 8;

        let mut n = self.0[offs];

        if shift > 0 {
            n <<= shift;
            n |= self.0[offs + 1] >> (8 - shift)
        }

        n
    }

    fn get_u16(&mut self) -> u16 {
        u16::from_be_bytes([self.get_u8(), self.get_u8()])
    }
}

#[derive(PartialEq, Eq, Debug, Default, Clone)]
pub struct Vp9HeaderDescriptor {
    /// picture ID is present
    pub i: bool,
    /// inter-picture predicted frame.
    pub p: bool,
    /// layer indices present
    pub l: bool,
    /// flexible mode
    pub f: bool,
    /// start of frame. beginning of new vp9 frame
    pub b: bool,
    /// end of frame
    pub e: bool,
    /// scalability structure (SS) present
    pub v: bool,
    /// Not a reference frame for upper spatial layers
    pub z: bool,

    /// Recommended headers
    /// 7 or 16 bits, picture ID.
    pub picture_id: u16,

    /// Conditionally recommended headers
    /// Temporal layer ID
    pub tid: u8,
    /// Switching up point
    pub u: bool,
    /// Spatial layer ID
    pub sid: u8,
    /// Inter-layer dependency used
    pub d: bool,

    /// Conditionally required headers
    /// Reference index (F=1)
    pub pdiff: Vec<u8>,
    /// Temporal layer zero index (F=0)
    pub tl0picidx: u8,

    /// Scalability structure headers
    /// N_S + 1 indicates the number of spatial layers present in the VP9 stream
    pub ns: u8,
    /// Each spatial layer's frame resolution present
    pub y: bool,
    /// PG description present flag.
    pub g: bool,
    /// N_G indicates the number of pictures in a Picture Group (PG)
    pub ng: u8,
    pub width: [Option<u16>; 3 as usize],
    pub height: [Option<u16>; 3 as usize],
    /// Temporal layer ID of pictures in a Picture Group
    pub pgtid: Vec<u8>,
    /// Switching up point of pictures in a Picture Group
    pub pgu: Vec<bool>,
    /// Reference indices of pictures in a Picture Group
    pub pgpdiff: Vec<Vec<u8>>,

    pub count: u8,
}

impl Vp9HeaderDescriptor {
    /// depacketize parses the passed byte slice and stores the result in the Vp9Packet this method is called upon
    fn parse(&mut self, packet: &[u8]) {
        assert!(packet.len() > 0);

        let mut reader = (packet, 0);
        let b = reader.get_u8();

        self.i = (b & 0x80) != 0;
        self.p = (b & 0x40) != 0;
        self.l = (b & 0x20) != 0;
        self.f = (b & 0x10) != 0;
        self.b = (b & 0x08) != 0;
        self.e = (b & 0x04) != 0;
        self.v = (b & 0x02) != 0;
        self.z = (b & 0x01) != 0;

        let mut payload_index = 1;

        if self.i {
            payload_index = self.parse_picture_id(&mut reader, payload_index);
        }

        if self.l {
            payload_index = self.parse_layer_info(&mut reader, payload_index);
        }

        if self.f && self.p {
            payload_index = self.parse_ref_indices(&mut reader, payload_index);
        }

        if self.v {
            _ = self.parse_ssdata(&mut reader, payload_index);
        }
    }

    // Picture ID:
    //
    //      +-+-+-+-+-+-+-+-+
    // I:   |M| PICTURE ID  |   M:0 => picture id is 7 bits.
    //      +-+-+-+-+-+-+-+-+   M:1 => picture id is 15 bits.
    // M:   | EXTENDED PID  |
    //      +-+-+-+-+-+-+-+-+
    //
    fn parse_picture_id(&mut self, reader: &mut dyn BitRead, mut payload_index: usize) -> usize {
        if reader.remaining() == 0 {
            panic!("too short");
        }
        let b = reader.get_u8();
        payload_index += 1;
        // PID present?
        if (b & 0x80) != 0 {
            if reader.remaining() == 0 {
                panic!("too short");
            }
            // M == 1, PID is 15bit
            self.picture_id = (((b & 0x7f) as u16) << 8) | (reader.get_u8() as u16);
            payload_index += 1;
        } else {
            self.picture_id = (b & 0x7F) as u16;
        }

        payload_index
    }

    fn parse_layer_info(&mut self, reader: &mut dyn BitRead, mut payload_index: usize) -> usize {
        payload_index = self.parse_layer_info_common(reader, payload_index);

        if self.f {
            payload_index
        } else {
            self.parse_layer_info_non_flexible_mode(reader, payload_index)
        }
    }

    // Layer indices (flexible mode):
    //
    //      +-+-+-+-+-+-+-+-+
    // L:   |  T  |U|  S  |D|
    //      +-+-+-+-+-+-+-+-+
    //
    fn parse_layer_info_common(
        &mut self,
        reader: &mut dyn BitRead,
        mut payload_index: usize,
    ) -> usize {
        if reader.remaining() == 0 {
            panic!("too short");
        }
        let b = reader.get_u8();
        payload_index += 1;

        self.tid = b >> 5;
        self.u = b & 0x10 != 0;
        self.sid = (b >> 1) & 0x7;
        self.d = b & 0x01 != 0;

        if self.sid >= 3 {
            panic!("too many spat");
        } else {
            payload_index
        }
    }

    // Layer indices (non-flexible mode):
    //
    //      +-+-+-+-+-+-+-+-+
    // L:   |  T  |U|  S  |D|
    //      +-+-+-+-+-+-+-+-+
    //      |   tl0picidx   |
    //      +-+-+-+-+-+-+-+-+
    //
    fn parse_layer_info_non_flexible_mode(
        &mut self,
        reader: &mut dyn BitRead,
        mut payload_index: usize,
    ) -> usize {
        if reader.remaining() == 0 {
            panic!("too short");
        }
        self.tl0picidx = reader.get_u8();
        payload_index += 1;
        payload_index
    }

    // Reference indices:
    //
    //      +-+-+-+-+-+-+-+-+                P=1,F=1: At least one reference index
    // P,F: | P_DIFF      |N|  up to 3 times          has to be specified.
    //      +-+-+-+-+-+-+-+-+                    N=1: An additional P_DIFF follows
    //                                                current P_DIFF.
    //
    fn parse_ref_indices(&mut self, reader: &mut dyn BitRead, mut payload_index: usize) -> usize {
        let mut b = 1u8;
        while (b & 0x01) != 0 {
            if reader.remaining() == 0 {
                panic!("too short");
            }
            b = reader.get_u8();
            payload_index += 1;

            self.pdiff.push(b >> 1);
            if self.pdiff.len() >= 3 {
                print!("too many diffs");
            }
        }

        payload_index
    }

    // Scalability structure (SS):
    //
    //      +-+-+-+-+-+-+-+-+
    // V:   | N_S |Y|G|-|-|-|
    //      +-+-+-+-+-+-+-+-+              -|
    // Y:   |     WIDTH     | (OPTIONAL)    .
    //      +               +               .
    //      |               | (OPTIONAL)    .
    //      +-+-+-+-+-+-+-+-+               . N_S + 1 times
    //      |     HEIGHT    | (OPTIONAL)    .
    //      +               +               .
    //      |               | (OPTIONAL)    .
    //      +-+-+-+-+-+-+-+-+              -|
    // G:   |      N_G      | (OPTIONAL)
    //      +-+-+-+-+-+-+-+-+                           -|
    // N_G: |  T  |U| R |-|-| (OPTIONAL)                 .
    //      +-+-+-+-+-+-+-+-+              -|            . N_G times
    //      |    P_DIFF     | (OPTIONAL)    . R times    .
    //      +-+-+-+-+-+-+-+-+              -|           -|
    //
    fn parse_ssdata(&mut self, reader: &mut dyn BitRead, mut payload_index: usize) -> usize {
        if reader.remaining() == 0 {
            panic!("too short");
        }

        let b = reader.get_u8();
        payload_index += 1;

        self.ns = b >> 5;
        self.y = b & 0x10 != 0;
        self.g = (b >> 1) & 0x7 != 0;

        let ns = (self.ns + 1) as usize;
        self.ng = 0;

        if self.y {
            if reader.remaining() < 4 * ns {
                panic!("too short");
            }

            for i in 0..ns {
                self.width[i] = Some(reader.get_u16());
                self.height[i] = Some(reader.get_u16());
            }
            payload_index += 4 * ns;
        }

        if self.g {
            if reader.remaining() == 0 {
                panic!("too short");
            }

            self.ng = reader.get_u8();
            payload_index += 1;
        }

        for i in 0..self.ng as usize {
            if reader.remaining() == 0 {
                panic!("too short");
            }
            let b = reader.get_u8();
            payload_index += 1;

            self.pgtid.push(b >> 5);
            self.pgu.push(b & 0x10 != 0);

            let r = ((b >> 2) & 0x3) as usize;
            if reader.remaining() < r {
                panic!("too short");
            }

            self.pgpdiff.push(vec![]);
            for _ in 0..r {
                let b = reader.get_u8();
                payload_index += 1;

                self.pgpdiff[i].push(b);
            }
        }

        payload_index
    }
}
