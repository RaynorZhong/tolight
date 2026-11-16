//! Request quotas protect HTTP service; message quotas remain transactional in Store.

use chrono::{DateTime, Duration, Utc};
use std::{
    collections::{HashMap, VecDeque},
    net::IpAddr,
    sync::Mutex,
};
use tolight_core::models::ApiError;

#[derive(Default)]
pub struct RequestLimiter {
    entries: Mutex<HashMap<String, VecDeque<DateTime<Utc>>>>,
}

impl RequestLimiter {
    pub fn check(&self, key: String, limit: usize, now: DateTime<Utc>) -> Result<(), ApiError> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let lower = now - Duration::seconds(60);
        entries.retain(|_, times| {
            times.retain(|time| *time > lower);
            !times.is_empty()
        });
        let times = entries.entry(key).or_default();
        if times.len() >= limit {
            // Sorting also handles an injected clock going backwards.
            let mut ordered: Vec<_> = times.iter().copied().collect();
            ordered.sort();
            let until = ordered[ordered.len() - limit] + Duration::seconds(60);
            return Err(ApiError::new("rate_limited").retry(retry_seconds(until - now)));
        }
        times.push_back(now);
        Ok(())
    }
}

/// Never uses caller-supplied forwarded headers. IPv6 admission is grouped by /64.
pub fn source_bucket(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(address) => address.to_string(),
        IpAddr::V6(address) => {
            if let Some(address) = address.to_ipv4_mapped() {
                return address.to_string();
            }
            let octets = address.octets();
            format!(
                "{:02x}{:02x}:{:02x}{:02x}:{:02x}{:02x}:{:02x}{:02x}/64",
                octets[0],
                octets[1],
                octets[2],
                octets[3],
                octets[4],
                octets[5],
                octets[6],
                octets[7]
            )
        }
    }
}

#[derive(Default)]
pub struct ApplicationQuota {
    entries: HashMap<String, Vec<DateTime<Utc>>>,
}

impl ApplicationQuota {
    /// Called while admission holds its async mutex across the application transaction.
    pub fn retry_after(&mut self, source: &str, limit: usize, now: DateTime<Utc>) -> Option<u64> {
        let lower = now - Duration::days(1);
        self.entries.retain(|_, times| {
            times.retain(|time| *time > lower);
            !times.is_empty()
        });
        let times = self.entries.entry(source.to_string()).or_default();
        if times.len() < limit {
            return None;
        }
        times.sort();
        Some(retry_seconds(
            times[times.len() - limit] + Duration::days(1) - now,
        ))
    }

    pub fn record(&mut self, source: &str, now: DateTime<Utc>) {
        self.entries
            .entry(source.to_string())
            .or_default()
            .push(now);
    }
}

fn retry_seconds(duration: Duration) -> u64 {
    let duration = duration.to_std().unwrap_or_default();
    (duration.as_secs() + u64::from(duration.subsec_nanos() != 0)).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_limit_counts_the_window_and_rounds_retry_up() {
        let limiter = RequestLimiter::default();
        let time = DateTime::from_timestamp(1000, 0).unwrap();
        limiter.check("agent".into(), 2, time).unwrap();
        limiter
            .check("agent".into(), 2, time + Duration::seconds(1))
            .unwrap();
        let blocked = limiter
            .check("agent".into(), 2, time + Duration::milliseconds(1500))
            .unwrap_err();
        assert_eq!(blocked.retry_after, Some(59));
        // Failed checks do not consume a request, and the exact boundary slides out.
        limiter
            .check("agent".into(), 2, time + Duration::seconds(60))
            .unwrap();
    }

    #[test]
    fn source_addresses_merge_only_the_intended_networks() {
        assert_eq!(
            source_bucket("2001:db8::1".parse().unwrap()),
            source_bucket("2001:db8::ffff".parse().unwrap())
        );
        assert_ne!(
            source_bucket("2001:db8::1".parse().unwrap()),
            source_bucket("2001:db8:0:1::1".parse().unwrap())
        );
        assert_ne!(
            source_bucket("127.0.0.1".parse().unwrap()),
            source_bucket("127.0.0.2".parse().unwrap())
        );
        assert_eq!(
            source_bucket("127.0.0.1".parse().unwrap()),
            source_bucket("::ffff:127.0.0.1".parse().unwrap())
        );
    }

    #[test]
    fn request_and_application_retries_round_up_without_losing_nanoseconds() {
        let start = DateTime::from_timestamp(1000, 0).unwrap();
        let just_before_next_second = start + Duration::seconds(1) - Duration::nanoseconds(1);
        let requests = RequestLimiter::default();
        requests.check("source".into(), 1, start).unwrap();
        assert_eq!(
            requests
                .check("source".into(), 1, just_before_next_second)
                .unwrap_err()
                .retry_after,
            Some(60)
        );
        assert_eq!(
            requests
                .check("source".into(), 1, start + Duration::seconds(1))
                .unwrap_err()
                .retry_after,
            Some(59)
        );
        let mut applications = ApplicationQuota::default();
        applications.record("source", start);
        assert_eq!(
            applications.retry_after("source", 1, just_before_next_second),
            Some(86400)
        );
        assert_eq!(
            applications.retry_after("source", 1, start + Duration::seconds(1)),
            Some(86399)
        );
    }
}
