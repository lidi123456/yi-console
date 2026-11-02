//! 连接数和失败次数的记账，以及反向代理后面取真实客户端 IP。

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 统计窗口内失败这么多次就拉黑
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
    /// ip -> (窗口开始时间, 失败次数, 拉黑到期时间)
    fails: HashMap<IpAddr, (Instant, u32, Option<Instant>)>,
}

/// 连接存活期间占着名额，drop 时归还。先占全局名额，知道真实 IP 后再 [`ConnSlot::bind`] 占单 IP 名额
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
    /// 按真实 IP 占名额；被拉黑或超过单 IP 上限时返回 false
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

    /// 占一个全局名额；`ip` 已知（直连）时同时占单 IP 名额
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
        // 顺手清掉过期记录，防止表无限增长
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

/// IP 或网段，`--trusted-proxy` 用
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

/// TCP 对端是可信代理时，从 X-Forwarded-For 里从右往左取第一个不可信的地址；否则就是 TCP 对端
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
        // 直连的人伪造 XFF 无效
        assert_eq!(real_ip(&t, ip("8.8.8.8"), Some("1.1.1.1")), ip("8.8.8.8"));
        // 经代理：取最右边不可信的
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
