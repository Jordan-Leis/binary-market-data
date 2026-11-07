//! Shared pieces for the measurement binaries.

use flate2::{Compress, Compression, FlushCompress};

/// Wire-overhead model. These are protocol constants, not measurements, and
/// the cost model labels anything built on them as modelled.
pub mod wire {
    /// IPv4 (20) + TCP with the timestamp option (32).
    pub const TCP_IP_PER_SEGMENT: u64 = 52;
    /// 1500-byte MTU minus the headers above.
    pub const MSS: u64 = 1448;
    /// TLS 1.3 with AES-GCM: 5-byte header, 1-byte content type, 16-byte tag.
    pub const TLS_PER_RECORD: u64 = 22;
    pub const TLS_RECORD_MAX: u64 = 16384;

    /// WebSocket server-to-client frame header (unmasked).
    pub fn ws_header(payload: u64) -> u64 {
        if payload < 126 {
            2
        } else if payload < 65536 {
            4
        } else {
            10
        }
    }

    /// Bytes on the wire for one WebSocket message sent on its own,
    /// assuming it is not coalesced with others into one TCP segment.
    pub fn bytes_for_frame(payload: u64) -> u64 {
        let ws = payload + ws_header(payload);
        let records = ws.div_ceil(TLS_RECORD_MAX).max(1);
        let tls = ws + records * TLS_PER_RECORD;
        let segments = tls.div_ceil(MSS).max(1);
        tls + segments * TCP_IP_PER_SEGMENT
    }
}

/// permessage-deflate (RFC 7692) for one connection. With context takeover
/// the window carries across messages, which is the best compression and the
/// reason the state is per connection.
pub struct Pmd {
    c: Compress,
    takeover: bool,
    buf: Vec<u8>,
}

impl Pmd {
    pub fn new(takeover: bool) -> Self {
        Self { c: Compress::new(Compression::new(6), false), takeover, buf: Vec::with_capacity(1 << 16) }
    }

    /// Compressed payload length, after stripping the 4-byte sync tail as the
    /// RFC requires.
    pub fn compress(&mut self, msg: &[u8]) -> &[u8] {
        if !self.takeover {
            self.c.reset();
        }
        self.buf.clear();
        let need = msg.len() + msg.len() / 2 + 64;
        if self.buf.capacity() < need {
            self.buf.reserve(need);
        }
        let before_in = self.c.total_in();
        loop {
            let consumed = (self.c.total_in() - before_in) as usize;
            let st = self.c.compress_vec(&msg[consumed..], &mut self.buf, FlushCompress::Sync);
            assert!(st.is_ok());
            if (self.c.total_in() - before_in) as usize == msg.len() && self.buf.len() < self.buf.capacity() {
                break;
            }
            self.buf.reserve(self.buf.capacity());
        }
        let n = self.buf.len();
        debug_assert!(n >= 4 && self.buf[n - 4..] == [0, 0, 0xff, 0xff]);
        &self.buf[..n.saturating_sub(4)]
    }
}

pub fn now_cpu_ns() -> u64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: getrusage writes into the struct we own.
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    let t = |tv: libc::timeval| tv.tv_sec as u64 * 1_000_000_000 + tv.tv_usec as u64 * 1000;
    t(ru.ru_utime) + t(ru.ru_stime)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Decompress, FlushDecompress};

    #[test]
    fn pmd_output_inflates_back() {
        for takeover in [true, false] {
            let mut p = Pmd::new(takeover);
            let mut d = Decompress::new(false);
            for i in 0..50 {
                let msg = format!(r#"{{"hello":"world {i}","pad":"{}"}}"#, "x".repeat(i * 37));
                let mut comp = p.compress(msg.as_bytes()).to_vec();
                comp.extend_from_slice(&[0, 0, 0xff, 0xff]);
                if !takeover {
                    d.reset(false);
                }
                let mut out = Vec::with_capacity(msg.len() + 64);
                d.decompress_vec(&comp, &mut out, FlushDecompress::Sync).unwrap();
                assert_eq!(out, msg.as_bytes());
            }
        }
    }

    #[test]
    fn wire_model_matches_hand_counts() {
        // 100-byte payload: 2 ws + 22 tls + 52 tcp/ip.
        assert_eq!(wire::bytes_for_frame(100), 176);
        // 2000 bytes: 4 ws, 1 record, 2 segments.
        assert_eq!(wire::bytes_for_frame(2000), 2000 + 4 + 22 + 104);
    }
}
