//! Accounting for connection counts and failures, and getting the real client IP behind a reverse proxy.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Ban after this many failures within the counting window
const MAX_FAILS: u32 = 10;
const FAIL_WINDOW: Duration = Duration::from_secs(600);
const BAN_TIME: Duration = Duration::from_secs(600);

pub struct Limits {
    pub max_conns: usize,
    pub max_per_ip: usize,
    inner: Mutex<LimitState>,
}

#[derive(Default)]
struct LimitState {
    total: usize,
    per_ip: HashMap<IpAddr, usize>,
    /// ip -> (window start, failure count, ban expiry)
    fails: HashMap<IpAddr, (Instant, u32, Option<Instant>)>,
}

/// Holds a slot while the connection lives and returns it on drop. Takes a global slot first, then a per-IP slot via [`ConnSlot::bind`] once the real IP is known
pub struct ConnSlot {
    limits: Arc<Limits>,
    ip: Option<IpAddr>,
}

impl Drop for ConnSlot {
    fn drop(&mut self) {
        let mut st = self.limits.inner.lock().unwrap();
        st.total = st.total.saturating_sub(1);
        if let Some(ip) = self.ip {
            if let Some(n) = st.per_ip.get_mut(&ip) {
                *n -= 1;
                if *n == 0 {
                    st.per_ip.remove(&ip);
                }
            }
        }
    }
}

impl ConnSlot {
    /// Take a slot for the real IP; returns false if banned or over the per-IP limit
    pub fn bind(&mut self, ip: IpAddr) -> bool {
        let mut st = self.limits.inner.lock().unwrap();
        if banned(&mut st, ip) {
            return false;
        }
        let n = st.per_ip.get(&ip).copied().unwrap_or(0);
        if n >= self.limits.max_per_ip {
            return false;
        }
        st.per_ip.insert(ip, n + 1);
        self.ip = Some(ip);
        true
    }
}

fn banned(st: &mut LimitState, ip: IpAddr) -> bool {
    if let Some((_, _, Some(until))) = st.fails.get(&ip) {
        if Instant::now() < *until {
            return true;
        }
        st.fails.remove(&ip);
    }
    false
}

impl Limits {
    pub fn new(max_conns: usize, max_per_ip: usize) -> Arc<Limits> {
        Arc::new(Limits { max_conns, max_per_ip, inner: Mutex::new(LimitState::default()) })
    }

    /// Take a global slot; when `ip` is known (direct connection), take the per-IP slot too
    pub fn acquire(self: &Arc<Self>, ip: Option<IpAddr>) -> Option<ConnSlot> {
        {
            let mut st = self.inner.lock().unwrap();
            if st.total >= self.max_conns {
                return None;
            }
            st.total += 1;
        }
        let mut slot = ConnSlot { limits: self.clone(), ip: None };
        if let Some(ip) = ip {
            if !slot.bind(ip) {
                return None;
            }
        }
        Some(slot)
    }

    pub fn record_fail(&self, ip: IpAddr) {
        let mut st = self.inner.lock().unwrap();
        let now = Instant::now();
        // Clean out expired records along the way so the table doesn't grow forever
        if st.fails.len() > 10_000 {
            st.fails.retain(|_, (start, _, until)| {
                until.map(|u| u > now).unwrap_or(false) || now.duration_since(*start) < FAIL_WINDOW
            });
        }
        let e = st.fails.entry(ip).or_insert((now, 0, None));
        if now.duration_since(e.0) > FAIL_WINDOW {
            *e = (now, 0, None);
        }
        e.1 += 1;
        if e.1 >= MAX_FAILS && e.2.is_none() {
            e.2 = Some(now + BAN_TIME);
            eprintln!("{ip}: 鉴权失败 {} 次，拉黑 {} 分钟", e.1, BAN_TIME.as_secs() / 60);
        }
    }
}

/// An IP or subnet, for `--trusted-proxy`
#[derive(Debug, Clone, PartialEq)]
pub struct Cidr {
    net: IpAddr,
    bits: u8,
}

impl Cidr {
    pub fn parse(s: &str) -> Option<Cidr> {
        let (ip, bits) = match s.trim().split_once('/') {
            Some((ip, b)) => (ip.parse::<IpAddr>().ok()?, Some(b.parse::<u8>().ok()?)),
            None => (s.trim().parse::<IpAddr>().ok()?, None),
        };
        let ip = ip.to_canonical();
        let max = if ip.is_ipv4() { 32 } else { 128 };
        let bits = bits.unwrap_or(max);
        (bits <= max).then_some(Cidr { net: ip, bits })
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.net, ip.to_canonical()) {
            (IpAddr::V4(n), IpAddr::V4(i)) => {
                let mask = if self.bits == 0 { 0 } else { u32::MAX << (32 - self.bits) };
                u32::from(n) & mask == u32::from(i) & mask
            }
            (IpAddr::V6(n), IpAddr::V6(i)) => {
                let mask = if self.bits == 0 { 0 } else { u128::MAX << (128 - self.bits) };
                u128::from(n) & mask == u128::from(i) & mask
            }
            _ => false,
        }
    }
}

pub fn trusted(list: &[Cidr], ip: IpAddr) -> bool {
    list.iter().any(|c| c.contains(ip))
}

/// When the TCP peer is a trusted proxy, take the first untrusted address from X-Forwarded-For, right to left; otherwise it's the TCP peer
pub fn real_ip(list: &[Cidr], peer: IpAddr, xff: Option<&str>) -> IpAddr {
    if !trusted(list, peer) {
        return peer;
    }
    let Some(xff) = xff else { return peer };
    let hops: Vec<IpAddr> = xff.split(',').filter_map(|s| s.trim().parse().ok()).collect();
    for ip in hops.iter().rev() {
        if !trusted(list, *ip) {
            return ip.to_canonical();
        }
    }
    hops.first().copied().unwrap_or(peer)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn cidr() {
        let c = Cidr::parse("10.0.0.0/8").unwrap();
        assert!(c.contains(ip("10.1.2.3")));
        assert!(!c.contains(ip("11.0.0.1")));
        assert!(Cidr::parse("127.0.0.1").unwrap().contains(ip("::ffff:127.0.0.1")));
        assert!(Cidr::parse("::1").unwrap().contains(ip("::1")));
        assert!(Cidr::parse("1.2.3.4/33").is_none());
        assert!(Cidr::parse("x").is_none());
    }

    #[test]
    fn xff_only_from_trusted_peer() {
        let t = vec![Cidr::parse("127.0.0.1").unwrap()];
        // A forged XFF from a direct connection has no effect
        assert_eq!(real_ip(&t, ip("8.8.8.8"), Some("1.1.1.1")), ip("8.8.8.8"));
        // Through a proxy: take the rightmost untrusted address
        assert_eq!(real_ip(&t, ip("127.0.0.1"), Some("6.6.6.6, 5.5.5.5")), ip("5.5.5.5"));
        assert_eq!(real_ip(&t, ip("127.0.0.1"), None), ip("127.0.0.1"));
        assert_eq!(real_ip(&[], ip("127.0.0.1"), Some("5.5.5.5")), ip("127.0.0.1"));
    }

    #[test]
    fn slots() {
        let l = Limits::new(10, 1);
        let a = l.acquire(Some(ip("1.1.1.1"))).unwrap();
        assert!(l.acquire(Some(ip("1.1.1.1"))).is_none());
        let mut b = l.acquire(None).unwrap();
        assert!(!b.bind(ip("1.1.1.1")));
        assert!(b.bind(ip("2.2.2.2")));
        drop(a);
        assert!(l.acquire(Some(ip("1.1.1.1"))).is_some());
        for _ in 0..MAX_FAILS {
            l.record_fail(ip("3.3.3.3"));
        }
        assert!(l.acquire(Some(ip("3.3.3.3"))).is_none());
    }
}
