//! AMD ReliveVR (Wireless GVR) client protocol, replacing the ALVR client
//! protocol. Talks to the official `com.amd.wirelessgvr` headset app (Lenovo
//! Mirage Solo / Daydream) over UDP 1235.
//!
//! Protocol notes and evidence: relivevr-linux `trunk/docs/` (protocol.md,
//! re-notes.md, windows-pcap.md, client-apk.md).
//!
//! Wire format: 15-byte FlowCtrl fragment header (BE: u16 seq, u32 total len,
//! u32 offset, u32 fragment len, u8 channel), then a type byte + JSON for
//! control, or `[1][VideoData JSON][NUL][Annex-B]` on channel 1 for video.
//!
//! Session: client Hello (type 0, broadcast) / HELLO_DIRECT (type 7) → our
//! HelloResponse (type 0); client caps (type 5) + StartRequest (type 3); we
//! send StartSensor (type 5) so poses (type 4 DeviceEvents) start flowing,
//! then VideoInit (once the encoder produced SPS/PPS) and per-eye VideoData.

use crate::{
    connection::{compute_restart_settings_hash, VideoPacket},
    ConnectionContext, ServerCoreEvent, ServerNegotiatedStreamingConfig, SESSION_MANAGER,
};
use alvr_common::{
    error,
    glam::{Quat, Vec3},
    info,
    parking_lot::RwLock,
    glam::UVec2,
    warn, DeviceMotion, Fov, LifecycleState, Pose, ViewParams, HAND_RIGHT_ID, HEAD_ID,
    RIGHT_MENU_CLICK_ID, RIGHT_SQUEEZE_CLICK_ID, RIGHT_SYSTEM_CLICK_ID, RIGHT_TRACKPAD_CLICK_ID,
    RIGHT_TRACKPAD_TOUCH_ID, RIGHT_TRACKPAD_X_ID, RIGHT_TRACKPAD_Y_ID, RIGHT_TRIGGER_CLICK_ID,
    RIGHT_TRIGGER_VALUE_ID,
};
use alvr_packets::{BatteryInfo, ButtonEntry, ButtonValue};
use alvr_session::{
    CodecType, ControllersEmulationMode, H264Profile, SteamvrHmdInitConfig,
};
use serde_json::Value;
use std::{
    collections::{HashSet, VecDeque},
    net::{SocketAddr, UdpSocket},
    sync::{
        atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering},
        mpsc::{self, RecvTimeoutError},
        Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const PORT: u16 = 1235;
const FRAG_HEADER_LEN: usize = 15;
/// Largest FlowCtrl payload per datagram (client MaxDatagramSize is 65507).
const MAX_FRAG_PAYLOAD: usize = 65000;

const CHANNEL_SERVICE: u8 = 0;
const CHANNEL_VIDEO: u8 = 1;

const TYPE_HELLO: u8 = 0;
const TYPE_START_REQUEST: u8 = 3;
const TYPE_DEVICE_EVENT: u8 = 4;
const TYPE_CAPS: u8 = 5; // TrackableDeviceCaps (C→S), StartSensor/StopSensor (S→C)
const TYPE_UPDATE_REQUEST: u8 = 6;
const TYPE_HELLO_DIRECT: u8 = 7;
const TYPE_STATS: u8 = 8;
const TYPE_KEEPALIVE: u8 = 9;

/// Per-eye render/encode size of the Mirage Solo (StartRequest DisplayWidth/Height).
const EYE_RESOLUTION: u32 = 1440;
/// Mirage Solo display rate (client UpdateRequest reports ≈75.0).
const REFRESH_RATE: f32 = 75.0;
/// Symmetric per-eye FOV. Advertised in HelloResponse; the client passes it to
/// `gvr_buffer_viewport_set_source_fov` and GVR applies the lens distortion,
/// so SteamVR must render exactly this frustum with no distortion of its own.
const FOV_DEG: f32 = 100.0;

const CLIENT_TIMEOUT: Duration = Duration::from_secs(5);
/// Real /hmd/pose samples before the first VideoData (client pose queue must be
/// warm for the exact ptsSensor lookup; Windows had ~43 before frame 0).
const MIN_HMD_POSES_BEFORE_VIDEO: u64 = 30;
/// The client samples poses at ~680 Hz; ALVR's driver event loop is built for
/// about one per display frame and falls behind when fed every sample (SteamVR
/// then renders seconds-old poses). Forward at most one pose per interval.
const TRACKING_EVENT_INTERVAL: Duration = Duration::from_millis(4);
const IDR_REQUEST_INTERVAL: Duration = Duration::from_millis(500);
/// ALVR encodes an infinite GOP; the client cannot recover from a lost frame
/// without a new IDR (Windows sends one every 120 frames).
const PERIODIC_IDR_INTERVAL: Duration = Duration::from_secs(2);
/// Give the client time to (re)initialise its decoders after VideoInit before
/// the stream starts on a fresh IDR.
const VIDEO_INIT_SETTLE: Duration = Duration::from_millis(200);

/// 100 ns ticks (ReliveVR pose `time`, ~Unix epoch) ⇄ ALVR target timestamps.
fn ticks_to_timestamp(ticks: u64) -> Duration {
    Duration::from_nanos(ticks.saturating_mul(100))
}

fn timestamp_to_ticks(timestamp: Duration) -> u64 {
    (timestamp.as_nanos() / 100) as u64
}

fn fragment_header(seq: u16, total: usize, offset: usize, len: usize, channel: u8) -> [u8; 15] {
    let mut h = [0u8; FRAG_HEADER_LEN];
    h[0..2].copy_from_slice(&seq.to_be_bytes());
    h[2..6].copy_from_slice(&(total as u32).to_be_bytes());
    h[6..10].copy_from_slice(&(offset as u32).to_be_bytes());
    h[10..14].copy_from_slice(&(len as u32).to_be_bytes());
    h[14] = channel;
    h
}

/// One FlowCtrl message → datagrams (same seq; offset advances for >64 KB).
fn fragments(seq: u16, channel: u8, payload: &[u8]) -> Vec<Vec<u8>> {
    if payload.is_empty() {
        return vec![fragment_header(seq, 0, 0, 0, channel).to_vec()];
    }
    payload
        .chunks(MAX_FRAG_PAYLOAD)
        .enumerate()
        .map(|(i, chunk)| {
            let mut d = Vec::with_capacity(FRAG_HEADER_LEN + chunk.len());
            d.extend_from_slice(&fragment_header(
                seq,
                payload.len(),
                i * MAX_FRAG_PAYLOAD,
                chunk.len(),
                channel,
            ));
            d.extend_from_slice(chunk);
            d
        })
        .collect()
}

/// Shared sender: FlowCtrl sequence numbers must increase across all messages
/// (the client drops older IDs as "Message is old").
struct Sender {
    socket: UdpSocket,
    seq: AtomicU16,
}

impl Sender {
    fn send(&self, addr: SocketAddr, channel: u8, payload: &[u8]) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        for datagram in fragments(seq, channel, payload) {
            if let Err(e) = self.socket.send_to(&datagram, addr) {
                warn!("ReliveVR: send to {addr} failed: {e}");
                return;
            }
        }
    }

    fn send_json(&self, addr: SocketAddr, type_byte: u8, json: &str) {
        let mut payload = Vec::with_capacity(1 + json.len());
        payload.push(type_byte);
        payload.extend_from_slice(json.as_bytes());
        self.send(addr, CHANNEL_SERVICE, &payload);
    }
}

fn hello_response_json() -> String {
    // Shape of the Windows server's HelloResponse; MaxDatagramSize,
    // DatagramSize and Port are mandatory (client null-derefs without them).
    let fov = FOV_DEG.to_radians();
    format!(
        r#"{{"ChannelsSupported":[true,true,false,false,true,false,false,false,false],"DatagramSize":65507,"MaxDatagramSize":65507,"Options":{{"EncoderSize":[{r},{r}],"HorizontalFOV":{fov},"KeyboardPresent":true,"Microphone":false,"MousePresent":true,"TouchScreenPresent":false,"VerticalFOV":{fov},"VideoCodecs":["avc"]}},"Port":{PORT},"ProtocolMinVersion":1,"ProtocolVersion":1,"ServerName":"ALVR ReliveVR","Transports":["UDP"]}}"#,
        r = EYE_RESOLUTION
    )
}

fn video_init_payload(config_nals: &[u8]) -> Vec<u8> {
    // NonLinearScaling must be false: frames are plain linear images (with true
    // the client applies its NLS un-warp mesh → lens-like distortion).
    let id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(1);
    let json = format!(
        r#"{{"BitDepth":8,"CodecID":"avc","Height":{r},"ID":{id},"NonLinearScaling":false,"Viewport":[0,0,{r},{r}],"Width":{r}}}"#,
        r = EYE_RESOLUTION
    );
    let mut body = Vec::with_capacity(json.len() + 2 + config_nals.len());
    body.push(0);
    body.extend_from_slice(json.as_bytes());
    body.push(0);
    body.extend_from_slice(config_nals);
    body
}

#[allow(clippy::too_many_arguments)]
fn video_data_payload(
    frame_num: u64,
    eye: u32,
    nals: &[u8],
    pts: u64,
    is_idr: bool,
    pts_sensor: u64,
    pts_encoder_lat: u64,
    pts_server_lat: u64,
) -> Vec<u8> {
    // Field order as in the Windows capture.
    let json = format!(
        r#"{{"cmpFrmSize":{},"encType":{},"frameNum":{frame_num},"frmType":{eye},"pts":{pts},"ptsEncoderLat":{pts_encoder_lat},"ptsSend":0,"ptsSensor":{pts_sensor},"ptsServerLat":{pts_server_lat}}}"#,
        nals.len(),
        if is_idr { 0 } else { 2 },
    );
    let mut body = Vec::with_capacity(json.len() + 2 + nals.len());
    body.push(1);
    body.extend_from_slice(json.as_bytes());
    body.push(0);
    body.extend_from_slice(nals);
    body
}

fn json_f32s<const N: usize>(v: &Value) -> Option<[f32; N]> {
    let a = v.as_array()?;
    if a.len() != N {
        return None;
    }
    let mut out = [0.0; N];
    for (o, x) in out.iter_mut().zip(a) {
        *o = x.as_f64()? as f32;
    }
    Some(out)
}

/// `{"time":…, "val":{"orient":[x,y,z,w],"pos":[x,y,z],"orientV":…,"posV":…}}`
fn parse_pose_sample(sample: &Value) -> Option<(u64, DeviceMotion)> {
    let time = sample.get("time")?.as_u64()?;
    let val = sample.get("val")?;
    let [qx, qy, qz, qw] = json_f32s::<4>(val.get("orient")?)?;
    let pos = json_f32s::<3>(val.get("pos")?).unwrap_or_default();
    let lin = val
        .get("posV")
        .and_then(json_f32s::<3>)
        .unwrap_or_default();
    let ang = val
        .get("orientV")
        .and_then(json_f32s::<3>)
        .unwrap_or_default();
    Some((
        time,
        DeviceMotion {
            pose: Pose {
                orientation: Quat::from_xyzw(qx, qy, qz, qw).normalize(),
                position: Vec3::from(pos),
            },
            linear_velocity: Vec3::from(lin),
            angular_velocity: Vec3::from(ang),
        },
    ))
}

/// Recent real /hmd/pose times. The client resolves ptsSensor by exact match in
/// its own pose queue; ALVR's PoseHistory picks a pose by orientation
/// similarity, which can yield stale or synthetic times.
#[derive(Default)]
struct RecentPoses {
    times: HashSet<u64>,
    order: VecDeque<u64>,
    latest: u64,
}

impl RecentPoses {
    const CAPACITY: usize = 20_000;

    fn insert(&mut self, time: u64) {
        if self.times.insert(time) {
            self.order.push_back(time);
            if self.order.len() > Self::CAPACITY {
                if let Some(old) = self.order.pop_front() {
                    self.times.remove(&old);
                }
            }
        }
        self.latest = self.latest.max(time);
    }
}

struct Session {
    addr: SocketAddr,
    ipd_m: f32,
    streaming: bool,
    last_rx: Instant,
    video_thread: Option<JoinHandle<()>>,
    latest_controller: Option<DeviceMotion>,
    /// Real /hmd/pose samples received this session.
    hmd_poses: Arc<AtomicU64>,
    recent_poses: Arc<alvr_common::parking_lot::Mutex<RecentPoses>>,
    synth_stop: Arc<AtomicBool>,
    synth_thread: Option<JoinHandle<()>>,
    /// Buttons the emulated SteamVR controller registered (Vive wand, forced
    /// in start_streaming). Daydream inputs are emitted directly as these destination IDs:
    /// ALVR's automatic source-profile mapping drops trigger/click and system.
    registered_buttons: HashSet<u64>,
    unknown_inputs: HashSet<String>,
    last_tracking_event: Option<Instant>,
    ctrl_poses: u64,
    max_head_angular_speed: f32,
}

pub fn handshake_loop(ctx: Arc<ConnectionContext>, lifecycle_state: Arc<RwLock<LifecycleState>>) {
    let socket = match UdpSocket::bind(("0.0.0.0", PORT)) {
        Ok(s) => s,
        Err(e) => {
            error!("ReliveVR: cannot bind UDP {PORT}: {e}");
            return;
        }
    };
    socket.set_read_timeout(Some(Duration::from_millis(100))).ok();
    socket.set_broadcast(true).ok();
    let sender = Arc::new(Sender {
        socket: socket.try_clone().expect("clone UDP socket"),
        seq: AtomicU16::new(1),
    });
    info!("ReliveVR: listening on UDP {PORT}");

    let mut session: Option<Session> = None;
    let mut buf = vec![0u8; 65536];

    while *lifecycle_state.read() != LifecycleState::ShuttingDown {
        if let Some(s) = &session {
            if s.last_rx.elapsed() > CLIENT_TIMEOUT {
                info!("ReliveVR: client {} timed out", s.addr);
                stop_streaming(&ctx, session.take());
            }
        }

        let (n, addr) = match socket.recv_from(&mut buf) {
            Ok(r) => r,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue
            }
            Err(e) => {
                warn!("ReliveVR: recv failed: {e}");
                continue;
            }
        };
        if n < FRAG_HEADER_LEN + 1 {
            continue;
        }
        let payload = &buf[FRAG_HEADER_LEN..n];
        let type_byte = payload[0];
        let body = &payload[1..];
        let json = || serde_json::from_slice::<Value>(body.strip_suffix(&[0]).unwrap_or(body)).ok();

        // Ignore our own broadcast echoes.
        if body.windows(13).any(|w| w == b"ALVR ReliveVR") {
            continue;
        }

        if let Some(s) = &mut session {
            if s.addr == addr {
                s.last_rx = Instant::now();
            }
        }

        match type_byte {
            TYPE_HELLO | TYPE_HELLO_DIRECT => {
                if type_byte == TYPE_HELLO_DIRECT {
                    // A (re)connecting client starts a fresh session.
                    if session.is_some() {
                        stop_streaming(&ctx, session.take());
                    }
                    info!("ReliveVR: HELLO_DIRECT from {addr}");
                    session = Some(Session {
                        addr,
                        ipd_m: 0.064,
                        streaming: false,
                        last_rx: Instant::now(),
                        video_thread: None,
                        latest_controller: None,
                        hmd_poses: Arc::new(AtomicU64::new(0)),
                        recent_poses: Default::default(),
                        synth_stop: Arc::new(AtomicBool::new(false)),
                        synth_thread: None,
                        registered_buttons: HashSet::new(),
                        unknown_inputs: HashSet::new(),
                        last_tracking_event: None,
                        ctrl_poses: 0,
                        max_head_angular_speed: 0.0,
                    });
                }
                sender.send_json(addr, TYPE_HELLO, &hello_response_json());
            }
            TYPE_START_REQUEST => {
                if let (Some(s), Some(v)) = (&mut session, json()) {
                    info!("ReliveVR: StartRequest {v}");
                    if let Some(ipd) = v.get("InterpupillaryDistance").and_then(Value::as_f64) {
                        if ipd > 0.0 {
                            s.ipd_m = ipd as f32;
                        }
                    }
                }
            }
            TYPE_CAPS => {
                let v = json();
                info!("ReliveVR: device caps {}", v.clone().unwrap_or_default());
                let is_ctrl = v
                    .as_ref()
                    .and_then(|v| v.get("class"))
                    .and_then(Value::as_str)
                    == Some("ctrl");
                // Windows order: VideoInit/StartSensor after the controller caps.
                if let Some(s) = &mut session {
                    if is_ctrl && !s.streaming && s.addr == addr {
                        start_streaming(&ctx, &sender, s);
                    }
                }
            }
            TYPE_DEVICE_EVENT => {
                if let (Some(s), Some(v)) = (&mut session, json()) {
                    if s.streaming {
                        handle_device_event(&ctx, s, &v);
                    }
                }
            }
            TYPE_UPDATE_REQUEST => {
                if let Some(v) = json() {
                    info!("ReliveVR: UpdateRequest {v}");
                }
            }
            TYPE_STATS | TYPE_KEEPALIVE => (),
            other => {
                info!("ReliveVR: unhandled type {other} ({} bytes)", body.len());
            }
        }
    }

    stop_streaming(&ctx, session.take());
}

fn start_streaming(ctx: &Arc<ConnectionContext>, sender: &Arc<Sender>, s: &mut Session) {
    // SteamVR only needs a restart when the HMD init config (resolution,
    // refresh rate) or restart-relevant settings change; everything else goes
    // with ClientConnected below.
    {
        let mut session_manager = SESSION_MANAGER.write();
        // A 3DoF Daydream controller has no fingers: hand skeleton emulation
        // makes SteamVR show hands instead of the controller.
        if session_manager
            .settings()
            .headset
            .controllers
            .as_option()
            .is_some_and(|c| c.hand_skeleton.enabled())
        {
            info!("ReliveVR: disabling hand skeleton emulation");
            session_manager
                .session_mut()
                .session_settings
                .headset
                .controllers
                .content
                .hand_skeleton
                .enabled = false;
        }
        // The Daydream controller (touchpad, trigger, app button) is closest to
        // a Vive wand; Touch emulation also infers a hand skeleton from touches.
        if let Some(controllers) = session_manager.settings().headset.controllers.as_option()
            && controllers.emulation_mode != ControllersEmulationMode::ViveWand
        {
            info!("ReliveVR: switching controller emulation to Vive wand");
            session_manager
                .session_mut()
                .session_settings
                .headset
                .controllers
                .content
                .emulation_mode
                .variant = alvr_session::ControllersEmulationModeDefaultVariant::ViveWand;
        }
        let init_config = SteamvrHmdInitConfig {
            eye_resolution_width: EYE_RESOLUTION,
            eye_resolution_height: EYE_RESOLUTION,
            target_eye_resolution_width: EYE_RESOLUTION,
            target_eye_resolution_height: EYE_RESOLUTION,
            refresh_rate: REFRESH_RATE as _,
        };
        let hash = compute_restart_settings_hash(&init_config, session_manager.settings());
        if session_manager.session().restart_settings_hash != hash {
            info!("ReliveVR: SteamVR HMD config changed, restarting SteamVR");
            let mut session = session_manager.session_mut();
            session.steamvr_hmd_init_config = init_config;
            session.restart_settings_hash = hash;
            drop(session);
            drop(session_manager);
            crate::notify_restart_driver();
            return;
        }
    }

    info!("ReliveVR: starting stream to {}", s.addr);
    s.streaming = true;

    s.registered_buttons = crate::registered_button_set();
    if s.registered_buttons.is_empty() {
        warn!("ReliveVR: controller emulation disabled in ALVR settings; inputs ignored");
    }

    // Startup deadlock: SteamVR only renders/encodes with a head pose, the client
    // only sends poses after StartSensor, and it apparently ignores StartSensor
    // before VideoInit, which needs SPS/PPS from the first encoded frame. So feed
    // SteamVR a synthetic head pose until real ones arrive; video_loop sends
    // VideoInit + StartSensor once the encoder produced parameter sets.
    sender.send_json(s.addr, TYPE_CAPS, r#"{"Message":"StartSensor"}"#);
    s.synth_stop.store(false, Ordering::Relaxed);
    s.synth_thread = Some(thread::spawn({
        let ctx = Arc::clone(ctx);
        let stop = Arc::clone(&s.synth_stop);
        let hmd_poses = Arc::clone(&s.hmd_poses);
        move || {
            let head = DeviceMotion {
                pose: Pose {
                    orientation: Quat::IDENTITY,
                    position: Vec3::new(0.0, 1.6, 0.0),
                },
                linear_velocity: Vec3::ZERO,
                angular_velocity: Vec3::ZERO,
            };
            while !stop.load(Ordering::Relaxed) && hmd_poses.load(Ordering::Relaxed) == 0 {
                let ticks = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| (d.as_nanos() / 100) as u64)
                    .unwrap_or(0);
                report_tracking(&ctx, ticks_to_timestamp(ticks), &[(*HEAD_ID, head)]);
                thread::sleep(Duration::from_millis(13));
            }
        }
    }));

    let half = FOV_DEG.to_radians() / 2.0;
    let fov = Fov {
        left: -half,
        right: half,
        up: half,
        down: -half,
    };
    let eye = |x: f32| ViewParams {
        pose: Pose {
            position: Vec3::new(x, 0.0, 0.0),
            orientation: Quat::IDENTITY,
        },
        fov,
    };
    ctx.events_sender
        .send(ServerCoreEvent::LocalViewParams([
            eye(-s.ipd_m / 2.0),
            eye(s.ipd_m / 2.0),
        ]))
        .ok();

    let max_queued = SESSION_MANAGER
        .read()
        .settings()
        .connection
        .max_queued_server_video_frames
        .max(4);
    let (video_tx, video_rx) = mpsc::sync_channel(max_queued);
    *ctx.video_channel_sender.lock() = Some(video_tx);
    *ctx.decoder_config.lock() = None;

    s.video_thread = Some(thread::spawn({
        let ctx = Arc::clone(ctx);
        let sender = Arc::clone(sender);
        let addr = s.addr;
        let hmd_poses = Arc::clone(&s.hmd_poses);
        let recent_poses = Arc::clone(&s.recent_poses);
        move || video_loop(&ctx, &sender, addr, video_rx, &hmd_poses, &recent_poses)
    }));

    // Fixed Mirage Solo stream: H.264 (the client's only decoder), no foveation
    // (the client cannot undo it), limited-range 8 bit.
    let resolution = UVec2::new(EYE_RESOLUTION, EYE_RESOLUTION);
    ctx.events_sender
        .send(ServerCoreEvent::ClientConnected(ServerNegotiatedStreamingConfig {
            transcoding_view_resolution: resolution,
            emulated_headset_view_resolution: resolution,
            refresh_rate: REFRESH_RATE,
            foveated_encoding: None,
            codec: CodecType::H264,
            h264_profile: H264Profile::High,
            use_10bit_encoder: false,
            encoding_gamma: 1.0,
            enable_hdr: false,
        }))
        .ok();
    ctx.events_sender.send(ServerCoreEvent::RequestIDR).ok();
}

fn stop_streaming(ctx: &ConnectionContext, session: Option<Session>) {
    let Some(mut s) = session else { return };
    s.synth_stop.store(true, Ordering::Relaxed);
    if let Some(t) = s.synth_thread.take() {
        t.join().ok();
    }
    if !s.streaming {
        return;
    }
    // Dropping the sender ends video_loop.
    *ctx.video_channel_sender.lock() = None;
    if let Some(t) = s.video_thread.take() {
        t.join().ok();
    }
    ctx.events_sender
        .send(ServerCoreEvent::ClientDisconnected)
        .ok();
    info!("ReliveVR: stream to {} stopped", s.addr);
}

fn handle_device_event(ctx: &ConnectionContext, s: &mut Session, v: &Value) {
    let Some(events) = v.get("events").and_then(Value::as_array) else {
        return; // `{}`: Daydream system button
    };

    let mut hmd: Option<(u64, DeviceMotion)> = None;
    let mut buttons = Vec::new();
    for event in events {
        let Some(id) = event.get("id").and_then(Value::as_str) else {
            continue;
        };
        let last_sample = event
            .get("data")
            .and_then(Value::as_array)
            .and_then(|d| d.last());
        match id {
            "/hmd/pose" => hmd = last_sample.and_then(parse_pose_sample),
            "/ctrlRight/pose" => {
                match last_sample.and_then(parse_pose_sample) {
                    Some((_, motion)) => {
                        s.latest_controller = Some(motion);
                        s.ctrl_poses += 1;
                    }
                    None => {
                        if s.unknown_inputs.insert("ctrl-pose-parse".into()) {
                            info!("ReliveVR: cannot parse controller pose {}", event);
                        }
                    }
                }
            }
            "/hmd/battery" | "/ctrlRight/battery" => {
                if let Some(level) = last_sample.and_then(|d| d.get("val")).and_then(Value::as_f64) {
                    let device_id = if id.starts_with("/hmd") { *HEAD_ID } else { *HAND_RIGHT_ID };
                    ctx.events_sender
                        .send(ServerCoreEvent::Battery(BatteryInfo {
                            device_id,
                            gauge_value: level as f32,
                            is_plugged: false,
                        }))
                        .ok();
                }
            }
            _ if id.contains("/in/") => {
                let val = last_sample.and_then(|d| d.get("val"));
                if !daydream_input(id, val, &mut buttons) && s.unknown_inputs.insert(id.to_owned()) {
                    info!("ReliveVR: unmapped input {id} = {}", val.cloned().unwrap_or_default());
                }
            }
            _ => (),
        }
    }

    buttons.retain(|entry| s.registered_buttons.contains(&entry.path_id));
    if !buttons.is_empty() {
        ctx.events_sender.send(ServerCoreEvent::Buttons(buttons)).ok();
    }

    // Tracking is keyed by the HMD sample time: it comes back as the frame's
    // target timestamp and must be echoed exactly as VideoData ptsSensor.
    let Some((time, mut head)) = hmd else { return };
    // No velocities into SteamVR: ALVR's driver and SteamVR would both
    // extrapolate the pose, so the image would show a predicted pose while
    // ptsSensor names the raw sample, and the client's reprojection would
    // correct from the wrong pose (jitter). The client reprojects itself.
    let speed = head.angular_velocity.length();
    if speed > s.max_head_angular_speed {
        s.max_head_angular_speed = speed;
    }
    head.linear_velocity = Vec3::ZERO;
    head.angular_velocity = Vec3::ZERO;
    s.recent_poses.lock().insert(time);
    let n = s.hmd_poses.fetch_add(1, Ordering::Relaxed) + 1;
    if n == 1 || n % 3000 == 0 {
        info!(
            "ReliveVR: {n} HMD poses received (max reported head angular speed {:.2}, zeroed), {} controller poses, latest controller {:?}",
            s.max_head_angular_speed,
            s.ctrl_poses,
            s.latest_controller.map(|m| (m.pose.position, m.pose.orientation))
        );
    }

    if s
        .last_tracking_event
        .is_some_and(|t| t.elapsed() < TRACKING_EVENT_INTERVAL)
    {
        return; // still recorded in recent_poses; the next one goes through
    }
    s.last_tracking_event = Some(Instant::now());

    let mut motions = vec![(*HEAD_ID, head)];
    if let Some(ctrl) = s.latest_controller {
        motions.push((*HAND_RIGHT_ID, ctrl));
    }
    report_tracking(ctx, ticks_to_timestamp(time), &motions);
}

/// Daydream controller input → right-hand button entries of the emulated
/// controller (app = `/in/menu/click`; home = `/in/sys/click` is handled by
/// Daydream itself and leaves the app, so it is not mapped).
/// Returns false for inputs without a mapping.
fn daydream_input(id: &str, val: Option<&Value>, out: &mut Vec<ButtonEntry>) -> bool {
    let binary = |path_id: u64, out: &mut Vec<ButtonEntry>| {
        if let Some(pressed) = val.and_then(Value::as_bool) {
            out.push(ButtonEntry {
                path_id,
                value: ButtonValue::Binary(pressed),
            });
        }
    };
    let Some(input) = id.strip_prefix("/ctrlRight") else {
        return false;
    };
    match input {
        // Protocol Y grows downwards.
        "/in/tp/val" => {
            if let Some([x, y]) = val.and_then(json_f32s::<2>) {
                out.push(ButtonEntry {
                    path_id: *RIGHT_TRACKPAD_X_ID,
                    value: ButtonValue::Scalar(x.clamp(-1.0, 1.0)),
                });
                out.push(ButtonEntry {
                    path_id: *RIGHT_TRACKPAD_Y_ID,
                    value: ButtonValue::Scalar((-y).clamp(-1.0, 1.0)),
                });
            }
        }
        "/in/tp/touch" => binary(*RIGHT_TRACKPAD_TOUCH_ID, out),
        "/in/tp/click" => binary(*RIGHT_TRACKPAD_CLICK_ID, out),
        // The client reports its own trigger (derived from a Daydream button).
        "/in/tr/val" => {
            if let Some(v) = val.and_then(Value::as_f64) {
                out.push(ButtonEntry {
                    path_id: *RIGHT_TRIGGER_VALUE_ID,
                    value: ButtonValue::Scalar((v as f32).clamp(0.0, 1.0)),
                });
            }
        }
        "/in/tr/click" => {
            if let Some(pressed) = val.and_then(Value::as_bool) {
                out.push(ButtonEntry {
                    path_id: *RIGHT_TRIGGER_CLICK_ID,
                    value: ButtonValue::Binary(pressed),
                });
                out.push(ButtonEntry {
                    path_id: *RIGHT_TRIGGER_VALUE_ID,
                    value: ButtonValue::Scalar(if pressed { 1.0 } else { 0.0 }),
                });
            }
        }
        // App button → system (SteamVR dashboard).
        "/in/menu/click" | "/in/app/click" => binary(*RIGHT_SYSTEM_CLICK_ID, out),
        "/in/vol/+/click" => binary(*RIGHT_MENU_CLICK_ID, out),
        "/in/vol/-/click" => binary(*RIGHT_SQUEEZE_CLICK_ID, out),
        _ => return false,
    }
    true
}

fn report_tracking(ctx: &ConnectionContext, timestamp: Duration, motions: &[(u64, DeviceMotion)]) {
    if let Some(stats) = &mut *ctx.statistics_manager.write() {
        stats.report_tracking_received(timestamp);
    }
    {
        let session_manager = SESSION_MANAGER.read();
        ctx.tracking_manager.write().report_device_motions(
            &session_manager.settings().headset,
            timestamp,
            motions,
        );
    }
    ctx.events_sender
        .send(ServerCoreEvent::Tracking {
            poll_timestamp: timestamp,
        })
        .ok();
}

/// Pairs per-eye packets (left then right, same timestamp), sends VideoInit
/// once SPS/PPS exist, and only (re)starts the stream on a stereo IDR.
fn video_loop(
    ctx: &ConnectionContext,
    sender: &Sender,
    addr: SocketAddr,
    video_rx: mpsc::Receiver<VideoPacket>,
    hmd_poses: &AtomicU64,
    recent_poses: &alvr_common::parking_lot::Mutex<RecentPoses>,
) {
    let mut substituted = 0u64;
    // A successful pose lookup on the client erases every older pose from its
    // queue (Motor::GetHeadPoseByPresentTime: list::erase(begin, found)), so
    // ptsSensor must never go backwards: an older stamp is gone and the frame
    // is presented with no pose at all ("Pose for Present not found").
    let mut last_pts_sensor = 0u64;
    let mut backwards = 0u64;
    let mut max_backwards_ticks = 0u64;
    // Debug: first frames of the left eye exactly as sent, for offline decoding.
    let mut dump = std::env::var_os("HOME")
        .and_then(|h| std::fs::File::create(std::path::Path::new(&h).join("relivevr_left.h264")).ok());
    let pts_step = (10_000_000.0 / REFRESH_RATE as f64).round() as u64;
    let mut video_init_at: Option<Instant> = None;
    let mut last_periodic_idr = Instant::now();
    let mut waiting_idr = true;
    let mut last_idr_request = Instant::now();
    let mut pending_left: Option<VideoPacket> = None;
    let mut frame_num = 0u64;

    let request_idr = |last: &mut Instant| {
        if last.elapsed() > IDR_REQUEST_INTERVAL {
            *last = Instant::now();
            ctx.events_sender.send(ServerCoreEvent::RequestIDR).ok();
        }
    };

    loop {
        let packet = match video_rx.recv_timeout(Duration::from_millis(500)) {
            Ok(p) => p,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return,
        };

        let (left, right) = match packet.eye {
            0 => {
                if pending_left.replace(packet).is_some() {
                    warn!("ReliveVR: left frame without right; resyncing");
                    waiting_idr = true;
                }
                continue;
            }
            1 => match pending_left.take() {
                Some(l) if l.header.timestamp == packet.header.timestamp => (l, packet),
                _ => {
                    warn!("ReliveVR: unpaired right frame; resyncing");
                    waiting_idr = true;
                    request_idr(&mut last_idr_request);
                    continue;
                }
            },
            _ => {
                error!("ReliveVR: got a non-per-eye video frame; the encoder must run per eye");
                continue;
            }
        };

        let Some(config) = ctx.decoder_config.lock().clone() else {
            request_idr(&mut last_idr_request);
            continue;
        };
        let Some(init_at) = video_init_at else {
            sender.send(addr, CHANNEL_VIDEO, &video_init_payload(&config.config_buffer));
            // No second StartSensor here: the client restarts its pipeline
            // (OnStart) on every StartSensor; poses already flow.
            video_init_at = Some(Instant::now());
            waiting_idr = true;
            info!(
                "ReliveVR: VideoInit sent ({} B parameter sets)",
                config.config_buffer.len()
            );
            continue;
        };
        if init_at.elapsed() < VIDEO_INIT_SETTLE {
            continue;
        }

        // Frames rendered from synthetic poses (or before the client's pose
        // queue is warm) would carry a ptsSensor the client cannot match.
        if hmd_poses.load(Ordering::Relaxed) < MIN_HMD_POSES_BEFORE_VIDEO {
            waiting_idr = true;
            continue;
        }

        let is_idr = left.header.is_idr && right.header.is_idr;
        if is_idr {
            last_periodic_idr = Instant::now();
        } else if !waiting_idr && last_periodic_idr.elapsed() > PERIODIC_IDR_INTERVAL {
            last_periodic_idr = Instant::now();
            ctx.events_sender.send(ServerCoreEvent::RequestIDR).ok();
        }
        if waiting_idr {
            if !is_idr {
                request_idr(&mut last_idr_request);
                continue;
            }
            waiting_idr = false;
            info!("ReliveVR: stream (re)synced on IDR at frame {frame_num}");
        }

        let pts = frame_num * pts_step;
        let mut pts_sensor = timestamp_to_ticks(left.header.timestamp);
        {
            let recent = recent_poses.lock();
            // ptsSensor must be the pose the frame was rendered with: the client's
            // GVR reprojection corrects by the difference to the current pose
            // (a newer stamp under-corrects → jitter). Fall back to the freshest
            // real HMD time only if the render pose is not one the client sent.
            if !recent.times.contains(&pts_sensor) && recent.latest != 0 {
                pts_sensor = recent.latest;
                substituted += 1;
            }
        }
        if pts_sensor < last_pts_sensor {
            backwards += 1;
            max_backwards_ticks = max_backwards_ticks.max(last_pts_sensor - pts_sensor);
            pts_sensor = last_pts_sensor;
        }
        last_pts_sensor = pts_sensor;
        for (eye, p) in [(0u32, &left), (1u32, &right)] {
            // ALVR strips SPS/PPS from IDRs; MediaCodec wants them in-band.
            let nals = if is_idr {
                let mut v = config.config_buffer.clone();
                v.extend_from_slice(&p.payload);
                v
            } else {
                p.payload.clone()
            };
            if eye == 0 && frame_num < 300 {
                if let Some(f) = &mut dump {
                    use std::io::Write;
                    f.write_all(&nals).ok();
                }
            }
            let body = video_data_payload(frame_num, eye, &nals, pts, is_idr, pts_sensor, 0, 0);
            sender.send(addr, CHANNEL_VIDEO, &body);
        }
        if frame_num < 3 || frame_num % 150 == 0 {
            info!(
                "ReliveVR: frame {frame_num} L={}B R={}B idr={is_idr} ptsSensor={pts_sensor} pose age {} ms (substituted {substituted}, backwards {backwards}, max {:.1} ms)",
                left.payload.len(),
                right.payload.len(),
                recent_poses.lock().latest.saturating_sub(pts_sensor) / 10_000,
                max_backwards_ticks as f64 / 10_000.0
            );
        }
        frame_num += 1;
    }
}
