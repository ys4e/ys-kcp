//! KCP

use std::cmp;
use std::cmp::Ordering;
use std::collections::VecDeque;
use std::io::{self, Cursor, Read, Write};

use bytes::{Buf, BufMut, BytesMut};
use crate::{KcpResult, error::Error};

#[cfg(feature = "byte-check")]
use std::convert::TryInto;

#[cfg(feature = "tokio")]
use tokio::io::{AsyncWrite, AsyncWriteExt};
#[cfg(feature = "tokio")]
use std::{pin::Pin, task::{Context, Poll}};

//const KCP_RTO_NDL: u32 = 30;
const KCP_RTO_NDL: u32 = 20;
const KCP_RTO_MIN: u32 = 100;
const KCP_RTO_DEF: u32 = 200;
const KCP_RTO_MAX: u32 = 60000;

const KCP_CMD_PUSH: u8 = 81;
const KCP_CMD_ACK: u8 = 82;
const KCP_CMD_WASK: u8 = 83;
const KCP_CMD_WINS: u8 = 84;

const KCP_ASK_SEND: u32 = 1;
const KCP_ASK_TELL: u32 = 2;

const KCP_WND_SND: u16 = 32;
//const KCP_WND_RCV: u16 = 128;
const KCP_WND_RCV: u16 = 256;

pub const KCP_MTU_DEF: usize = 1400;
// const KCP_ACK_FAST: u32 = 3;

const KCP_INTERVAL: u32 = 100;
//pub const KCP_OVERHEAD: usize = 24;
//pub const KCP_OVERHEAD: usize = 28;

#[cfg(feature = "byte-check")]
pub const DEFAULT_KCP_OVERHEAD: usize = 28;
#[cfg(feature = "byte-check")]
pub const MAX_KCP_OVERHEAD: usize = 32;

#[cfg(not(feature = "byte-check"))]
pub const KCP_OVERHEAD: usize = 28;

const KCP_DEADLINK: u32 = 20;

const KCP_THRESH_INIT: u16 = 2;
const KCP_THRESH_MIN: u16 = 2;

const KCP_PROBE_INIT: u32 = 7000;
const KCP_PROBE_LIMIT: u32 = 120000;

/// Read `conv` from raw buffer
pub fn get_conv(buf: &[u8]) -> u32 {
    assert!(buf.len() >= DEFAULT_KCP_OVERHEAD);
    u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]])
}

/// Read `token` from raw buffer
pub fn get_token(buf: &[u8]) -> u32 {
    assert!(buf.len() >= DEFAULT_KCP_OVERHEAD);
    u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]])
}

#[inline]
#[cfg(feature = "byte-check")]
pub fn compute_hash(data: &[u8]) -> u32 {
    let hash = xxhash_rust::xxh3::xxh3_64(data);
    match (hash & 0xFFFFFFFF).try_into() {
        Ok(v) => v,
        Err(_) => unreachable!(),
    }
}

#[inline]
fn bound(lower: u32, v: u32, upper: u32) -> u32 {
    cmp::min(cmp::max(lower, v), upper)
}

#[inline]
fn timediff(later: u32, earlier: u32) -> i32 {
    later as i32 - earlier as i32
}

#[derive(Default, Clone, Debug)]
struct KcpSegment {
    #[cfg(feature = "byte-check")]
    overhead: usize,

    conv: u32,
    token: u32,
    cmd: u8,
    frg: u8,
    wnd: u16,
    ts: u32,
    sn: u32,
    una: u32,
    resendts: u32,
    rto: u32,
    fastack: u32,
    xmit: u32,
    
    #[cfg(feature = "byte-check")]
    byte_check_code: u32,
    
    data: BytesMut,
}

impl KcpSegment {
    #[cfg(not(feature = "byte-check"))]
    fn new_with_data(data: BytesMut) -> Self {
        KcpSegment {
            conv: 0,
            token: 0,
            cmd: 0,
            frg: 0,
            wnd: 0,
            ts: 0,
            sn: 0,
            una: 0,
            resendts: 0,
            rto: 0,
            fastack: 0,
            xmit: 0,
            
            #[cfg(feature = "byte-check")]
            byte_check_code: 0,
            
            data,
        }
    }

    #[cfg(feature = "byte-check")]
    fn new_with_data(data: BytesMut, overhead: usize) -> Self {
        KcpSegment {
            overhead,

            conv: 0,
            token: 0,
            cmd: 0,
            frg: 0,
            wnd: 0,
            ts: 0,
            sn: 0,
            una: 0,
            resendts: 0,
            rto: 0,
            fastack: 0,
            xmit: 0,

            #[cfg(feature = "byte-check")]
            byte_check_code: 0,

            data,
        }
    }

    fn encode(&self, buf: &mut BytesMut) {
        let overhead = {
            #[cfg(feature = "byte-check")]
            {
                self.overhead
            }

            #[cfg(not(feature = "byte-check"))]
            {
                KCP_OVERHEAD
            }
        };

        if buf.remaining_mut() < overhead {
            panic!(
                "REMAIN {} encoded {} {:?}",
                buf.remaining_mut(),
                overhead,
                self
            );
        }

        buf.put_u32_le(self.conv);
        buf.put_u32_le(self.token);
        buf.put_u8(self.cmd);
        buf.put_u8(self.frg);
        buf.put_u16_le(self.wnd);
        buf.put_u32_le(self.ts);
        buf.put_u32_le(self.sn);
        buf.put_u32_le(self.una);
        buf.put_u32_le(self.data.len() as u32);
        // BEG PATCH: miHoYo proprietary
        #[cfg(feature = "byte-check")]
        if self.overhead > DEFAULT_KCP_OVERHEAD {
            buf.put_u32_le(self.byte_check_code);
        }
        // END PATCH: miHoYo proprietary
        buf.put_slice(&self.data);
    }

    #[allow(dead_code)]
    #[cfg(not(feature = "byte-check"))]
    fn encoded_len(&self) -> usize {
        KCP_OVERHEAD + self.data.len()
    }

    #[allow(dead_code)]
    #[cfg(feature = "byte-check")]
    fn encoded_len(&self) -> usize {
        self.overhead + self.data.len()
    }
}

#[derive(Default)]
pub struct KcpOutput<O>(pub O);

impl<O: Write> Write for KcpOutput<O> {
    #[inline]
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        trace!("[RO] {} bytes", data.len());
        self.0.write(data)
    }

    #[inline]
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

#[cfg(feature = "tokio")]
impl<O: AsyncWrite + Unpin> AsyncWrite for KcpOutput<O> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<Result<usize, io::Error>> {
        Pin::new(&mut self.0).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.0.is_write_vectored()
    }
}

/// KCP control
#[derive(Default)]
pub struct Kcp<Output> {
    #[cfg(feature = "byte-check")]
    overhead: usize,

    /// Conversation ID
    conv: u32,
    /// Maximun Transmission Unit
    mtu: usize,
    /// Maximum Segment Size
    mss: u32,
    /// Connection state
    state: i32,

    /// User token
    token: u32,

    /// First unacknowledged packet
    snd_una: u32,
    /// Next packet
    snd_nxt: u32,
    /// Next packet to be received
    rcv_nxt: u32,

    /// Congetion window threshole
    ssthresh: u16,

    /// ACK receive variable RTT
    rx_rttval: u32,
    /// ACK receive static RTT
    rx_srtt: u32,
    /// Resend time (calculated by ACK delay time)
    rx_rto: u32,
    /// Minimal resend timeout
    rx_minrto: u32,

    /// Send window
    snd_wnd: u16,
    /// Receive window
    rcv_wnd: u16,
    /// Remote receive window
    rmt_wnd: u16,
    /// Congetion window
    cwnd: u16,
    /// Check window
    /// - IKCP_ASK_TELL, telling window size to remote
    /// - IKCP_ASK_SEND, ask remote for window size
    probe: u32,

    /// Last update time
    current: u32,
    /// Flush interval
    interval: u32,
    /// Next flush interval
    ts_flush: u32,
    xmit: u32,

    /// Enable nodelay
    nodelay: bool,
    /// Updated has been called or not
    updated: bool,

    /// Next check window timestamp
    ts_probe: u32,
    /// Check window wait time
    probe_wait: u32,

    /// Maximum resend time
    dead_link: u32,
    /// Maximum payload size
    incr: u32,

    snd_queue: VecDeque<KcpSegment>,
    rcv_queue: VecDeque<KcpSegment>,
    snd_buf: VecDeque<KcpSegment>,
    rcv_buf: VecDeque<KcpSegment>,

    /// Pending ACK
    acklist: VecDeque<(u32, u32)>,
    buf: BytesMut,

    /// ACK number to trigger fast resend
    fastresend: u32,
    /// Disable congetion control
    nocwnd: bool,
    /// Enable stream mode
    stream: bool,

    /// Get conv from the next input call
    input_conv: bool,

    pub output: KcpOutput<Output>,
}

impl<Output> Kcp<Output> {
    /// Creates a KCP control object, `conv` must be equal in both endpoints in one connection.
    /// `output` is the callback object for writing.
    ///
    /// `conv` represents conversation.
    pub fn new(conv: u32, token: u32, output: Output) -> Self {
        Kcp::construct(conv, token, output, false)
    }

    /// Creates a KCP control object in stream mode, `conv` must be equal in both endpoints in one connection.
    /// `output` is the callback object for writing.
    ///
    /// `conv` represents conversation.
    pub fn new_stream(conv: u32, token: u32, output: Output) -> Self {
        Kcp::construct(conv, token, output, true)
    }

    #[cfg(not(feature = "byte-check"))]
    fn construct(conv: u32, token: u32, output: Output, stream: bool) -> Self {
        Kcp {
            conv,
            snd_una: 0,
            snd_nxt: 0,
            rcv_nxt: 0,
            token,
            rx_rttval: 0,
            rx_srtt: 0,
            state: 0,
            cwnd: 0,
            probe: 0,
            current: 0,
            xmit: 0,
            nodelay: false,
            updated: false,
            ts_probe: 0,
            probe_wait: 0,
            dead_link: KCP_DEADLINK,
            incr: 0,
            fastresend: 0,
            nocwnd: false,
            stream,
            snd_wnd: KCP_WND_SND,
            rcv_wnd: KCP_WND_RCV,
            rmt_wnd: KCP_WND_RCV,
            mtu: KCP_MTU_DEF,
            mss: (KCP_MTU_DEF - KCP_OVERHEAD) as u32,
            buf: BytesMut::with_capacity((KCP_MTU_DEF + KCP_OVERHEAD) * 3),
            snd_queue: VecDeque::new(),
            rcv_queue: VecDeque::new(),
            snd_buf: VecDeque::new(),
            rcv_buf: VecDeque::new(),
            acklist: VecDeque::new(),
            rx_rto: KCP_RTO_DEF,
            rx_minrto: KCP_RTO_MIN,
            interval: KCP_INTERVAL,
            ts_flush: KCP_INTERVAL,
            ssthresh: KCP_THRESH_INIT,
            input_conv: false,
            output: KcpOutput(output),
        }
    }

    #[cfg(feature = "byte-check")]
    fn construct(conv: u32, token: u32, output: Output, stream: bool) -> Self {
        Kcp {
            overhead: DEFAULT_KCP_OVERHEAD,
            conv,
            snd_una: 0,
            snd_nxt: 0,
            rcv_nxt: 0,
            token,
            rx_rttval: 0,
            rx_srtt: 0,
            state: 0,
            cwnd: 0,
            probe: 0,
            current: 0,
            xmit: 0,
            nodelay: false,
            updated: false,
            ts_probe: 0,
            probe_wait: 0,
            dead_link: KCP_DEADLINK,
            incr: 0,
            fastresend: 0,
            nocwnd: false,
            stream,
            snd_wnd: KCP_WND_SND,
            rcv_wnd: KCP_WND_RCV,
            rmt_wnd: KCP_WND_RCV,
            mtu: KCP_MTU_DEF,
            mss: (KCP_MTU_DEF - DEFAULT_KCP_OVERHEAD) as u32,
            buf: BytesMut::with_capacity((KCP_MTU_DEF + MAX_KCP_OVERHEAD) * 3),
            snd_queue: VecDeque::new(),
            rcv_queue: VecDeque::new(),
            snd_buf: VecDeque::new(),
            rcv_buf: VecDeque::new(),
            acklist: VecDeque::new(),
            rx_rto: KCP_RTO_DEF,
            rx_minrto: KCP_RTO_MIN,
            interval: KCP_INTERVAL,
            ts_flush: KCP_INTERVAL,
            ssthresh: KCP_THRESH_INIT,
            input_conv: false,
            output: KcpOutput(output),
        }
    }

    /// Check buffer size without actually consuming it
    pub fn peeksize(&self) -> KcpResult<usize> {
        match self.rcv_queue.front() {
            Some(segment) => {
                if segment.frg == 0 {
                    return Ok(segment.data.len());
                }

                if self.rcv_queue.len() < (segment.frg + 1) as usize {
                    return Err(Error::ExpectingFragment);
                }

                let mut len = 0;

                for segment in &self.rcv_queue {
                    len += segment.data.len();
                    if segment.frg == 0 {
                        break;
                    }
                }

                Ok(len)
            }
            None => Err(Error::RecvQueueEmpty),
        }
    }

    // move available data from rcv_buf -> rcv_queue
    pub fn move_buf(&mut self) {
        while !self.rcv_buf.is_empty() {
            let nrcv_que = self.rcv_queue.len();
            {
                let seg = &self.rcv_buf[0];
                if seg.sn == self.rcv_nxt && nrcv_que < self.rcv_wnd as usize {
                    self.rcv_nxt += 1;
                } else {
                    break;
                }
            }

            let seg = self.rcv_buf.pop_front().unwrap();
            self.rcv_queue.push_back(seg);
        }
    }

    /// Receive data from buffer
    pub fn recv(&mut self, buf: &mut [u8]) -> KcpResult<usize> {
        if self.rcv_queue.is_empty() {
            return Err(Error::RecvQueueEmpty);
        }

        let peeksize = self.peeksize()?;

        if peeksize > buf.len() {
            debug!("recv peeksize={} bufsize={} too small", peeksize, buf.len());
            return Err(Error::UserBufTooSmall);
        }

        let recover = self.rcv_queue.len() >= self.rcv_wnd as usize;

        // Merge fragment
        let mut cur = Cursor::new(buf);
        while let Some(seg) = self.rcv_queue.pop_front() {
            Write::write_all(&mut cur, &seg.data)?;

            trace!("recv sn={}", seg.sn);

            if seg.frg == 0 {
                break;
            }
        }
        assert_eq!(cur.position() as usize, peeksize);

        self.move_buf();

        // fast recover
        if self.rcv_queue.len() < self.rcv_wnd as usize && recover {
            // ready to send back IKCP_CMD_WINS in ikcp_flush
            // tell remote my window size
            self.probe |= KCP_ASK_TELL;
        }

        Ok(cur.position() as usize)
    }

    /// Send bytes into buffer
    pub fn send(&mut self, mut buf: &[u8]) -> KcpResult<usize> {
        let mut sent_size = 0;

        assert!(self.mss > 0);

        // append to previous segment in streaming mode (if possible)
        if self.stream {
            if let Some(old) = self.snd_queue.back_mut() {
                let l = old.data.len();
                if l < self.mss as usize {
                    let capacity = self.mss as usize - l;
                    let extend = cmp::min(buf.len(), capacity);

                    trace!(
                        "send stream mss={} last length={} extend={}",
                        self.mss,
                        l,
                        extend
                    );

                    let (lf, rt) = buf.split_at(extend);
                    old.data.extend_from_slice(lf);
                    buf = rt;

                    old.frg = 0;
                    sent_size += extend;
                }

                if buf.is_empty() {
                    return Ok(sent_size);
                }
            }
        }

        let count = if buf.len() <= self.mss as usize {
            1
        } else {
            (buf.len() + self.mss as usize - 1) / self.mss as usize
        };

        if count >= KCP_WND_RCV as usize {
            debug!("send bufsize={} mss={} too large", buf.len(), self.mss);
            return Err(Error::UserBufTooBig);
        }
        assert!(count > 0);

        // let count = cmp::max(1, count);

        for i in 0..count {
            let size = cmp::min(self.mss as usize, buf.len());

            let (_, rt) = buf.split_at(size);

            let mut new_segment = {
                #[cfg(feature = "byte-check")]
                {
                    KcpSegment::new_with_data(BytesMut::with_capacity(size), self.overhead)
                }

                #[cfg(not(feature = "byte-check"))]
                {
                    KcpSegment::new_with_data(BytesMut::with_capacity(size))
                }
            };
            buf = rt;

            new_segment.frg = if self.stream {
                0
            } else {
                (count - i - 1) as u8
            };

            self.snd_queue.push_back(new_segment);
            sent_size += size;
        }

        Ok(sent_size)
    }

    fn update_ack(&mut self, rtt: u32) {
        if self.rx_srtt == 0 {
            self.rx_srtt = rtt;
            self.rx_rttval = rtt / 2;
        } else {
            let delta = if rtt > self.rx_srtt {
                rtt - self.rx_srtt
            } else {
                self.rx_srtt - rtt
            };
            self.rx_rttval = (3 * self.rx_rttval + delta) / 4;
            self.rx_srtt = ((7 * (self.rx_srtt as u64) + (rtt as u64)) / 8) as u32;
            if self.rx_srtt < 1 {
                self.rx_srtt = 1;
            }
        }
        let rto = self.rx_srtt + cmp::max(self.interval, 4 * self.rx_rttval);
        self.rx_rto = bound(self.rx_minrto, rto, KCP_RTO_MAX);
    }

    #[inline]
    fn shrink_buf(&mut self) {
        self.snd_una = match self.snd_buf.front() {
            Some(seg) => seg.sn,
            None => self.snd_nxt,
        };
    }

    fn parse_ack(&mut self, sn: u32) {
        if timediff(sn, self.snd_una) < 0 || timediff(sn, self.snd_nxt) >= 0 {
            return;
        }

        for i in (0..self.snd_buf.len()).rev() {
            match sn.cmp(&self.snd_buf[i].sn) {
                Ordering::Equal => {
                    self.snd_buf.remove(i);
                }
                Ordering::Less => break,
                _ => (),
            }
        }
    }

    fn parse_una(&mut self, una: u32) {
        while !self.snd_buf.is_empty() {
            if timediff(una, self.snd_buf[0].sn) > 0 {
                // self.snd_buf.remove(0);
                self.snd_buf.pop_front();
            } else {
                break;
            }
        }
    }

    fn parse_fastack(&mut self, sn: u32) {
        if timediff(sn, self.snd_una) < 0 || timediff(sn, self.snd_nxt) >= 0 {
            return;
        }

        for seg in &mut self.snd_buf {
            if timediff(sn, seg.sn) < 0 {
                break;
            } else if sn != seg.sn {
                seg.fastack += 1;
            }
        }
    }

    #[inline]
    fn ack_push(&mut self, sn: u32, ts: u32) {
        self.acklist.push_back((sn, ts));
    }

    fn parse_data(&mut self, new_segment: KcpSegment) {
        let sn = new_segment.sn;

        if timediff(sn, self.rcv_nxt + self.rcv_wnd as u32) >= 0 || timediff(sn, self.rcv_nxt) < 0 {
            return;
        }

        let mut repeat = false;
        let mut new_index = self.rcv_buf.len();

        for segment in self.rcv_buf.iter().rev() {
            if segment.sn == sn {
                repeat = true;
                break;
            } else if timediff(sn, segment.sn) > 0 {
                break;
            }
            new_index -= 1;
        }

        if !repeat {
            self.rcv_buf.insert(new_index, new_segment);
        }

        // move available data from rcv_buf -> rcv_queue
        self.move_buf();
    }

    /// Get `conv` from the next `input` call
    #[inline]
    pub fn input_conv(&mut self) {
        self.input_conv = true;
    }

    /// Check if Kcp is waiting for the next input
    #[inline]
    pub fn waiting_conv(&self) -> bool {
        self.input_conv
    }

    /// Set `conv` value
    #[inline]
    pub fn set_conv(&mut self, conv: u32) {
        self.conv = conv;
    }

    /// Get `conv`
    #[inline]
    pub fn conv(&self) -> u32 {
        self.conv
    }

    /// Set `token` value
    #[inline]
    pub fn set_token(&mut self, token: u32) {
        self.token = token;
    }

    /// Get `token`
    #[inline]
    pub fn token(&self) -> u32 {
        self.token
    }

    /// Call this when you received a packet from raw connection
    pub fn input(&mut self, buf: &[u8]) -> KcpResult<usize> {
        let input_size = buf.len();

        trace!("[RI] {} bytes", buf.len());

        if buf.len() < self.header_len() {
            debug!(
                "input bufsize={} too small, at least {}",
                buf.len(),
                self.header_len()
            );
            return Err(Error::InvalidSegmentSize(buf.len()));
        }

        let mut flag = false;
        let mut max_ack = 0;
        let old_una = self.snd_una;

        let mut buf = Cursor::new(buf);
        while buf.remaining() >= self.header_len() as usize {
            let conv = buf.get_u32_le();
            if conv != self.conv {
                // This allows getting conv from this call, which allows us to allocate
                // conv from the server side.
                if self.input_conv {
                    debug!("input conv={} updated, original conv={}", conv, self.conv);
                    self.conv = conv;
                    self.input_conv = false;
                } else {
                    debug!("input conv={} expected conv={} not match", conv, self.conv);
                    return Err(Error::ConvInconsistent(self.conv, conv));
                }
            }

            let token = buf.get_u32_le();

            let cmd = buf.get_u8();
            let frg = buf.get_u8();
            let wnd = buf.get_u16_le();
            let ts = buf.get_u32_le();
            let sn = buf.get_u32_le();
            let una = buf.get_u32_le();
            let len = buf.get_u32_le() as usize;

            #[cfg(feature = "byte-check")]
            let byte_check_code = if self.overhead > DEFAULT_KCP_OVERHEAD {
                buf.get_u32_le()
            } else {
                0
            };

            if buf.remaining() < len as usize {
                debug!(
                    "input bufsize={} payload length={} remaining={} not match",
                    input_size,
                    len,
                    buf.remaining()
                );
                return Err(Error::InvalidSegmentDataSize(len, buf.remaining()));
            }

            match cmd {
                KCP_CMD_PUSH | KCP_CMD_ACK | KCP_CMD_WASK | KCP_CMD_WINS => {}
                _ => {
                    debug!("input cmd={} unrecognized", cmd);
                    return Err(Error::UnsupportedCmd(cmd));
                }
            }

            if token != self.token {
                return Err(Error::TokenMismatch(token, self.token));
            }

            self.rmt_wnd = wnd;

            self.parse_una(una);
            self.shrink_buf();

            let mut has_read_data = false;

            match cmd {
                KCP_CMD_ACK => {
                    let rtt = timediff(self.current, ts);
                    if rtt >= 0 {
                        self.update_ack(rtt as u32);
                    }
                    self.parse_ack(sn);
                    self.shrink_buf();

                    if !flag {
                        max_ack = sn;
                        flag = true;
                    } else if timediff(sn, max_ack) > 0 {
                        max_ack = sn;
                    }

                    trace!(
                        "input ack: sn={} rtt={} rto={}",
                        sn,
                        timediff(self.current, ts),
                        self.rx_rto
                    );
                }
                KCP_CMD_PUSH => {
                    trace!("input psh: sn={} ts={}", sn, ts);

                    if timediff(sn, self.rcv_nxt + self.rcv_wnd as u32) < 0 {
                        self.ack_push(sn, ts);
                        if timediff(sn, self.rcv_nxt) >= 0 {
                            let mut sbuf = BytesMut::with_capacity(len as usize);
                            unsafe {
                                sbuf.set_len(len as usize);
                            }
                            buf.read_exact(&mut sbuf).unwrap();
                            has_read_data = true;

                            let mut segment = {
                                #[cfg(feature = "byte-check")]
                                {
                                    KcpSegment::new_with_data(sbuf, self.overhead)
                                }

                                #[cfg(not(feature = "byte-check"))]
                                {
                                    KcpSegment::new_with_data(sbuf)
                                }
                            };

                            segment.conv = conv;
                            segment.token = token;
                            segment.cmd = cmd;
                            segment.frg = frg;
                            segment.wnd = wnd;
                            segment.ts = ts;
                            segment.sn = sn;
                            segment.una = una;

                            #[cfg(feature = "byte-check")]
                            {
                                segment.byte_check_code = byte_check_code;
                            }

                            self.parse_data(segment);
                        }
                    }
                }
                KCP_CMD_WASK => {
                    trace!("input probe");
                    self.probe |= KCP_ASK_TELL;
                }
                KCP_CMD_WINS => {
                    // Do nothing
                    trace!("input wins: {}", wnd);
                }
                _ => unreachable!(),
            }

            // Force skip unread data
            if !has_read_data {
                let next_pos = buf.position() + len as u64;
                buf.set_position(next_pos);
            }
        }

        if flag {
            self.parse_fastack(max_ack);
        }

        if self.snd_una > old_una && self.cwnd < self.rmt_wnd {
            let mss = self.mss;
            if self.cwnd < self.ssthresh {
                self.cwnd += 1;
                self.incr += mss;
            } else {
                if self.incr < mss {
                    self.incr = mss;
                }
                self.incr += (mss * mss) / self.incr + (mss / 16);
                if (self.cwnd + 1) as u32 * mss <= self.incr {
                    self.cwnd += 1;
                }
            }
            if self.cwnd > self.rmt_wnd {
                self.cwnd = self.rmt_wnd;
                self.incr = self.rmt_wnd as u32 * mss;
            }
        }

        Ok(buf.position() as usize)
    }

    fn wnd_unused(&self) -> u16 {
        if self.rcv_queue.len() < self.rcv_wnd as usize {
            self.rcv_wnd - self.rcv_queue.len() as u16
        } else {
            0
        }
    }

    fn probe_wnd_size(&mut self) {
        // probe window size (if remote window size equals zero)
        if self.rmt_wnd == 0 {
            if self.probe_wait == 0 {
                self.probe_wait = KCP_PROBE_INIT;
                self.ts_probe = self.current + self.probe_wait;
            } else {
                if timediff(self.current, self.ts_probe) >= 0 && self.probe_wait < KCP_PROBE_INIT {
                    self.probe_wait = KCP_PROBE_INIT;
                }
                self.probe_wait += self.probe_wait / 2;
                if self.probe_wait > KCP_PROBE_LIMIT {
                    self.probe_wait = KCP_PROBE_LIMIT;
                }
                self.ts_probe = self.current + self.probe_wait;
                self.probe |= KCP_ASK_SEND;
            }
        } else {
            self.ts_probe = 0;
            self.probe_wait = 0;
        }
    }

    /// Determine when you should call `update`.
    /// Return when you should invoke `update` in millisec, if there is no `input`/`send` calling.
    /// You can call `update` in that time without calling it repeatly.
    pub fn check(&self, current: u32) -> u32 {
        if !self.updated {
            return 0;
        }

        let mut ts_flush = self.ts_flush;
        let mut tm_packet = u32::max_value();

        if timediff(current, ts_flush) >= 10000 || timediff(current, ts_flush) < -10000 {
            ts_flush = current;
        }

        if timediff(current, ts_flush) >= 0 {
            // return self.interval;
            return 0;
        }

        let tm_flush = timediff(ts_flush, current) as u32;
        for seg in &self.snd_buf {
            let diff = timediff(seg.resendts, current);
            if diff <= 0 {
                // return self.interval;
                return 0;
            }
            if (diff as u32) < tm_packet {
                tm_packet = diff as u32;
            }
        }

        cmp::min(cmp::min(tm_packet, tm_flush), self.interval)
    }

    /// Change MTU size, default is 1400
    ///
    /// MTU = Maximum Transmission Unit
    pub fn set_mtu(&mut self, mtu: usize) -> KcpResult<()> {
        if mtu < 50 || mtu < self.header_len() {
            debug!("set_mtu mtu={} invalid", mtu);
            return Err(Error::InvalidMtu(mtu));
        }

        self.mtu = mtu;
        self.mss = (self.mtu - self.header_len()) as u32;

        let additional = ((mtu + self.header_len()) * 3) as isize - self.buf.capacity() as isize;
        if additional > 0 {
            self.buf.reserve(additional as usize);
        }

        Ok(())
    }

    /// Get MTU
    pub fn mtu(&self) -> usize {
        self.mtu
    }

    /// Set check interval
    pub fn set_interval(&mut self, mut interval: u32) {
        if interval > 5000 {
            interval = 5000;
        } else if interval < 10 {
            interval = 10;
        }
        self.interval = interval;
    }

    /// Set nodelay
    ///
    /// fastest config: nodelay(true, 20, 2, true)
    ///
    /// `nodelay`: default is disable (false)
    /// `interval`: internal update timer interval in millisec, default is 100ms
    /// `resend`: 0:disable fast resend(default), 1:enable fast resend
    /// `nc`: `false`: normal congestion control(default), `true`: disable congestion control
    pub fn set_nodelay(&mut self, nodelay: bool, interval: i32, resend: i32, nc: bool) {
        if nodelay {
            self.nodelay = true;
            self.rx_minrto = KCP_RTO_NDL;
        } else {
            self.nodelay = false;
            self.rx_minrto = KCP_RTO_MIN;
        }

        match interval {
            interval if interval < 10 => self.interval = 10,
            interval if interval > 5000 => self.interval = 5000,
            _ => self.interval = interval as u32,
        }

        if resend >= 0 {
            self.fastresend = resend as u32;
        }

        self.nocwnd = nc;
    }

    /// Set `wndsize`
    /// set maximum window size: `sndwnd=32`, `rcvwnd=32` by default
    pub fn set_wndsize(&mut self, sndwnd: u16, rcvwnd: u16) {
        if sndwnd > 0 {
            self.snd_wnd = sndwnd as u16;
        }

        if rcvwnd > 0 {
            self.rcv_wnd = cmp::max(rcvwnd, KCP_WND_RCV) as u16;
        }
    }

    /// `snd_wnd` Send window
    pub fn snd_wnd(&self) -> u16 {
        self.snd_wnd
    }

    /// `rcv_wnd` Receive window
    pub fn rcv_wnd(&self) -> u16 {
        self.rcv_wnd
    }

    /// Get `waitsnd`, how many packet is waiting to be sent
    pub fn wait_snd(&self) -> usize {
        self.snd_buf.len() + self.snd_queue.len()
    }

    /// Set `rx_minrto`
    pub fn set_rx_minrto(&mut self, rto: u32) {
        self.rx_minrto = rto;
    }

    /// Set `fastresend`
    pub fn set_fast_resend(&mut self, fr: u32) {
        self.fastresend = fr;
    }

    /// KCP header size
    #[cfg(not(feature = "byte-check"))]
    pub fn header_len(&self) -> usize {
        KCP_OVERHEAD as usize
    }

    /// KCP header size
    #[cfg(feature = "byte-check")]
    pub fn header_len(&self) -> usize {
        self.overhead
    }

    /// Sets the KCP instance's overhead/header length.
    #[cfg(feature = "byte-check")]
    pub fn set_header_len(&mut self, overhead: usize) {
        self.overhead = overhead;

        // Update the maximum segment size which is dependent on the overhead
        self.mss = (KCP_MTU_DEF - overhead) as u32;
    }

    /// Enabled stream or not
    pub fn is_stream(&self) -> bool {
        self.stream
    }

    /// Maximum Segment Size
    pub fn mss(&self) -> u32 {
        self.mss
    }

    /// Set maximum resend times
    pub fn set_maximum_resend_times(&mut self, dead_link: u32) {
        self.dead_link = dead_link;
    }

    /// Check if KCP connection is dead (resend times excceeded)
    pub fn is_dead_link(&self) -> bool {
        self.state != 0
    }
}

impl<Output: Write> Kcp<Output> {
    fn _flush_ack(&mut self, segment: &mut KcpSegment) -> KcpResult<()> {
        // flush acknowledges
        // while let Some((sn, ts)) = self.acklist.pop_front() {
        for &(sn, ts) in &self.acklist {
            if self.buf.len() + self.header_len() > self.mtu as usize {
                self.output.write_all(&self.buf)?;
                self.buf.clear();
            }
            segment.sn = sn;
            segment.ts = ts;
            segment.encode(&mut self.buf);
        }
        self.acklist.clear();

        Ok(())
    }

    fn _flush_probe_commands(&mut self, cmd: u8, segment: &mut KcpSegment) -> KcpResult<()> {
        segment.cmd = cmd;
        if self.buf.len() + self.header_len() > self.mtu as usize {
            self.output.write_all(&self.buf)?;
            self.buf.clear();
        }
        segment.encode(&mut self.buf);
        Ok(())
    }

    fn flush_probe_commands(&mut self, segment: &mut KcpSegment) -> KcpResult<()> {
        // flush window probing commands
        if (self.probe & KCP_ASK_SEND) != 0 {
            self._flush_probe_commands(KCP_CMD_WASK, segment)?;
        }

        // flush window probing commands
        if (self.probe & KCP_ASK_TELL) != 0 {
            self._flush_probe_commands(KCP_CMD_WINS, segment)?;
        }
        self.probe = 0;
        Ok(())
    }

    /// Flush pending ACKs
    pub fn flush_ack(&mut self) -> KcpResult<()> {
        if !self.updated {
            debug!("flush updated() must be called at least once");
            return Err(Error::NeedUpdate);
        }

        let mut segment = KcpSegment {
            conv: self.conv,
            cmd: KCP_CMD_ACK,
            wnd: self.wnd_unused(),
            una: self.rcv_nxt,
            ..Default::default()
        };

        self._flush_ack(&mut segment)
    }

    /// Flush pending data in buffer.
    pub fn flush(&mut self) -> KcpResult<()> {
        if !self.updated {
            debug!("flush updated() must be called at least once");
            return Err(Error::NeedUpdate);
        }

        let mut segment = KcpSegment {
            conv: self.conv,
            token: self.token,
            cmd: KCP_CMD_ACK,
            wnd: self.wnd_unused(),
            una: self.rcv_nxt,
            ..Default::default()
        };

        self._flush_ack(&mut segment)?;
        self.probe_wnd_size();
        self.flush_probe_commands(&mut segment)?;

        // println!("SNDBUF size {}", self.snd_buf.len());

        // calculate window size
        let mut cwnd = cmp::min(self.snd_wnd, self.rmt_wnd);
        if !self.nocwnd {
            cwnd = cmp::min(self.cwnd, cwnd);
        }

        // move data from snd_queue to snd_buf
        while timediff(self.snd_nxt, self.snd_una + cwnd as u32) < 0 {
            match self.snd_queue.pop_front() {
                Some(mut new_segment) => {
                    new_segment.conv = self.conv;
                    new_segment.token = self.token;
                    new_segment.cmd = KCP_CMD_PUSH;
                    new_segment.wnd = segment.wnd;
                    new_segment.ts = self.current;
                    new_segment.sn = self.snd_nxt;
                    self.snd_nxt += 1;
                    new_segment.una = self.rcv_nxt;
                    new_segment.resendts = self.current;
                    new_segment.rto = self.rx_rto;
                    new_segment.fastack = 0;
                    new_segment.xmit = 0;

                    #[cfg(feature = "byte-check")]
                    {
                        new_segment.byte_check_code = compute_hash(&new_segment.data);
                    }
                    
                    self.snd_buf.push_back(new_segment);
                }
                None => break,
            }
        }

        // calculate resent
        let resent = if self.fastresend > 0 {
            self.fastresend
        } else {
            u32::max_value()
        };

        let rtomin = if !self.nodelay { self.rx_rto >> 3 } else { 0 };

        let mut lost = false;
        let mut change = 0;

        for snd_segment in &mut self.snd_buf {
            let mut need_send = false;

            if snd_segment.xmit == 0 {
                need_send = true;
                snd_segment.xmit += 1;
                snd_segment.rto = self.rx_rto;
                snd_segment.resendts = self.current + snd_segment.rto + rtomin;
            } else if timediff(self.current, snd_segment.resendts) >= 0 {
                need_send = true;
                snd_segment.xmit += 1;
                self.xmit += 1;
                if !self.nodelay {
                    snd_segment.rto += self.rx_rto;
                } else {
                    snd_segment.rto += self.rx_rto / 2;
                }
                snd_segment.resendts = self.current + snd_segment.rto;
                lost = true;
            } else if snd_segment.fastack >= resent {
                need_send = true;
                snd_segment.xmit += 1;
                snd_segment.fastack = 0;
                snd_segment.resendts = self.current + snd_segment.rto;
                change += 1;
            }

            if need_send {
                snd_segment.ts = self.current;
                snd_segment.wnd = segment.wnd;
                snd_segment.una = self.rcv_nxt;

                let overhead = {
                    #[cfg(feature = "byte-check")]
                    {
                        self.overhead
                    }

                    #[cfg(not(feature = "byte-check"))]
                    {
                        KCP_OVERHEAD
                    }
                };
                let need = overhead + snd_segment.data.len();

                if self.buf.len() + need > self.mtu as usize {
                    self.output.write_all(&self.buf)?;
                    self.buf.clear();
                }

                snd_segment.encode(&mut self.buf);

                if snd_segment.xmit >= self.dead_link {
                    self.state = -1;
                }
            }
        }

        // Flush all data in buffer
        if !self.buf.is_empty() {
            self.output.write_all(&self.buf)?;
            self.buf.clear();
        }

        // update ssthresh
        if change > 0 {
            let inflight = self.snd_nxt - self.snd_una;
            self.ssthresh = inflight as u16 / 2;
            if self.ssthresh < KCP_THRESH_MIN {
                self.ssthresh = KCP_THRESH_MIN;
            }
            self.cwnd = self.ssthresh + resent as u16;
            self.incr = self.cwnd as u32 * self.mss;
        }

        if lost {
            self.ssthresh = cwnd / 2;
            if self.ssthresh < KCP_THRESH_MIN {
                self.ssthresh = KCP_THRESH_MIN;
            }
            self.cwnd = 1;
            self.incr = self.mss;
        }

        if self.cwnd < 1 {
            self.cwnd = 1;
            self.incr = self.mss;
        }

        Ok(())
    }

    /// Update state every 10ms ~ 100ms.
    ///
    /// Or you can ask `check` when to call this again.
    pub fn update(&mut self, current: u32) -> KcpResult<()> {
        self.current = current;

        if !self.updated {
            self.updated = true;
            self.ts_flush = self.current;
        }

        let mut slap = timediff(self.current, self.ts_flush);

        if slap >= 10000 || slap < -10000 {
            self.ts_flush = self.current;
            slap = 0;
        }

        if slap >= 0 {
            self.ts_flush += self.interval;
            if timediff(self.current, self.ts_flush) >= 0 {
                self.ts_flush = self.current + self.interval;
            }
            self.flush()?;
        }

        Ok(())
    }
}

#[cfg(feature = "tokio")]
impl<Output: AsyncWrite + Unpin + Send> Kcp<Output> {
    async fn _async_flush_ack(&mut self, segment: &mut KcpSegment) -> KcpResult<()> {
        // flush acknowledges
        // while let Some((sn, ts)) = self.acklist.pop_front() {
        for &(sn, ts) in &self.acklist {
            if self.buf.len() + self.header_len() > self.mtu {
                self.output.write_all(&self.buf).await?;
                self.buf.clear();
            }
            segment.sn = sn;
            segment.ts = ts;
            segment.encode(&mut self.buf);
        }
        self.acklist.clear();

        Ok(())
    }

    async fn _async_flush_probe_commands(
        &mut self,
        cmd: u8,
        segment: &mut KcpSegment,
    ) -> KcpResult<()> {
        segment.cmd = cmd;
        if self.buf.len() + self.header_len() > self.mtu {
            self.output.write_all(&self.buf).await?;
            self.buf.clear();
        }
        segment.encode(&mut self.buf);
        Ok(())
    }

    async fn async_flush_probe_commands(&mut self, segment: &mut KcpSegment) -> KcpResult<()> {
        // flush window probing commands
        if (self.probe & KCP_ASK_SEND) != 0 {
            self._async_flush_probe_commands(KCP_CMD_WASK, segment)
                .await?;
        }

        // flush window probing commands
        if (self.probe & KCP_ASK_TELL) != 0 {
            self._async_flush_probe_commands(KCP_CMD_WINS, segment)
                .await?;
        }
        self.probe = 0;
        Ok(())
    }

    /// Flush pending ACKs
    pub async fn async_flush_ack(&mut self) -> KcpResult<()> {
        if !self.updated {
            debug!("flush updated() must be called at least once");
            return Err(Error::NeedUpdate);
        }

        let mut segment = KcpSegment {
            conv: self.conv,
            token: self.token,
            cmd: KCP_CMD_ACK,
            wnd: self.wnd_unused(),
            una: self.rcv_nxt,
            ..Default::default()
        };

        self._async_flush_ack(&mut segment).await
    }

    /// Flush pending data in buffer.
    pub async fn async_flush(&mut self) -> KcpResult<()> {
        if !self.updated {
            debug!("flush updated() must be called at least once");
            return Err(Error::NeedUpdate);
        }

        let mut segment = KcpSegment {
            conv: self.conv,
            token: self.token,
            cmd: KCP_CMD_ACK,
            wnd: self.wnd_unused(),
            una: self.rcv_nxt,
            ..Default::default()
        };

        self._async_flush_ack(&mut segment).await?;
        self.probe_wnd_size();
        self.async_flush_probe_commands(&mut segment).await?;

        // calculate window size
        let mut cwnd = cmp::min(self.snd_wnd, self.rmt_wnd);
        if !self.nocwnd {
            cwnd = cmp::min(self.cwnd, cwnd);
        }

        // move data from snd_queue to snd_buf
        while timediff(self.snd_nxt, self.snd_una + u32::from(cwnd)) < 0 {
            match self.snd_queue.pop_front() {
                Some(mut new_segment) => {
                    new_segment.conv = self.conv;
                    new_segment.token = self.token;
                    new_segment.cmd = KCP_CMD_PUSH;
                    new_segment.wnd = segment.wnd;
                    new_segment.ts = self.current;
                    new_segment.sn = self.snd_nxt;
                    self.snd_nxt += 1;
                    new_segment.una = self.rcv_nxt;
                    new_segment.resendts = self.current;
                    new_segment.rto = self.rx_rto;
                    new_segment.fastack = 0;
                    new_segment.xmit = 0;
                    
                    #[cfg(feature = "byte-check")]
                    {
                        new_segment.byte_check_code = compute_hash(&new_segment.data);
                    }
                    
                    self.snd_buf.push_back(new_segment);
                }
                None => break,
            }
        }

        // calculate resent
        let resent = if self.fastresend > 0 {
            self.fastresend
        } else {
            u32::max_value()
        };

        let rtomin = if !self.nodelay { self.rx_rto >> 3 } else { 0 };

        let mut lost = false;
        let mut change = 0;

        for snd_segment in &mut self.snd_buf {
            let mut need_send = false;

            if snd_segment.xmit == 0 {
                need_send = true;
                snd_segment.xmit += 1;
                snd_segment.rto = self.rx_rto;
                snd_segment.resendts = self.current + snd_segment.rto + rtomin;
            } else if timediff(self.current, snd_segment.resendts) >= 0 {
                need_send = true;
                snd_segment.xmit += 1;
                self.xmit += 1;
                if !self.nodelay {
                    snd_segment.rto += cmp::max(snd_segment.rto, self.rx_rto);
                } else {
                    let step = snd_segment.rto; // (kcp->nodelay < 2) ? ((IINT32)(segment->rto)) : kcp->rx_rto;
                    snd_segment.rto += step / 2;
                }
                snd_segment.resendts = self.current + snd_segment.rto;
                lost = true;
            } else if snd_segment.fastack >= resent {
                need_send = true;
                snd_segment.xmit += 1;
                snd_segment.fastack = 0;
                snd_segment.resendts = self.current + snd_segment.rto;
                change += 1;
            }

            if need_send {
                snd_segment.ts = self.current;
                snd_segment.wnd = segment.wnd;
                snd_segment.una = self.rcv_nxt;

                let overhead = {
                    #[cfg(feature = "byte-check")]
                    {
                        self.overhead
                    }

                    #[cfg(not(feature = "byte-check"))]
                    {
                        KCP_OVERHEAD
                    }
                };
                let need = overhead + snd_segment.data.len();

                if self.buf.len() + need > self.mtu {
                    self.output.write_all(&self.buf).await?;
                    self.buf.clear();
                }

                snd_segment.encode(&mut self.buf);

                if snd_segment.xmit >= self.dead_link {
                    self.state = -1; // (IUINT32)-1
                }
            }
        }

        // Flush all data in buffer
        if !self.buf.is_empty() {
            self.output.write_all(&self.buf).await?;
            self.buf.clear();
        }

        // update ssthresh
        if change > 0 {
            let inflight = self.snd_nxt - self.snd_una;
            self.ssthresh = inflight as u16 / 2;
            if self.ssthresh < KCP_THRESH_MIN {
                self.ssthresh = KCP_THRESH_MIN;
            }
            self.cwnd = self.ssthresh + resent as u16;
            self.incr = self.cwnd as u32 * self.mss;
        }

        if lost {
            self.ssthresh = cwnd / 2;
            if self.ssthresh < KCP_THRESH_MIN {
                self.ssthresh = KCP_THRESH_MIN;
            }
            self.cwnd = 1;
            self.incr = self.mss;
        }

        if self.cwnd < 1 {
            self.cwnd = 1;
            self.incr = self.mss;
        }

        Ok(())
    }

    /// Update state every 10ms ~ 100ms.
    ///
    /// Or you can ask `check` when to call this again.
    pub async fn async_update(&mut self, current: u32) -> KcpResult<()> {
        self.current = current;

        if !self.updated {
            self.updated = true;
            self.ts_flush = self.current;
        }

        let mut slap = timediff(self.current, self.ts_flush);

        if !(-10000..10000).contains(&slap) {
            self.ts_flush = self.current;
            slap = 0;
        }

        if slap >= 0 {
            self.ts_flush += self.interval;
            if timediff(self.current, self.ts_flush) >= 0 {
                self.ts_flush = self.current + self.interval;
            }
            self.async_flush().await?;
        }

        Ok(())
    }
}
